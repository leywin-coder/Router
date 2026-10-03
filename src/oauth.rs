use base64::Engine;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::AccountConfig;
use crate::state::ModelQuota;

/// Antigravity OAuth client credentials (loaded from env)
pub fn get_client_id() -> String {
    std::env::var("GOOGLE_CLIENT_ID")
        .expect("GOOGLE_CLIENT_ID must be set in .env or environment")
}

pub fn get_client_secret() -> String {
    std::env::var("GOOGLE_CLIENT_SECRET")
        .expect("GOOGLE_CLIENT_SECRET must be set in .env or environment")
}
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const CALLBACK_PORT: u16 = 51121;
pub const CLOUD_CODE_BASE: &str = "https://cloudcode-pa.googleapis.com";
pub const USER_AGENT: &str = "antigravity/1.15.8 linux/amd64";

pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub email: String,
    pub refresh_token: String,
    pub access_token: String,
    pub token_expires_at: i64,
    pub project_id: String,
}

impl From<&AccountConfig> for Account {
    fn from(cfg: &AccountConfig) -> Self {
        Self {
            name: cfg.name.clone(),
            email: cfg.email.clone(),
            refresh_token: cfg.refresh_token.clone(),
            access_token: cfg.access_token.clone(),
            token_expires_at: cfg.token_expires_at,
            project_id: cfg.project_id.clone(),
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: i64,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
    error_description: Option<String>,
}

#[derive(Clone)]
pub struct OAuthManager {
    pub accounts: Arc<RwLock<Vec<Account>>>,
    http: Client,
    config_path: PathBuf,
}

#[allow(dead_code)]
impl OAuthManager {
    pub fn new(account_configs: &[AccountConfig], config_path: PathBuf) -> Self {
        let accounts: Vec<Account> = account_configs.iter().map(Account::from).collect();
        Self {
            accounts: Arc::new(RwLock::new(accounts)),
            http: Client::new(),
            config_path,
        }
    }

    pub fn from_config(config: &crate::config::Config, config_path: PathBuf) -> Self {
        let all_accounts: Vec<Account> = config
            .providers
            .iter()
            .filter(|p| p.provider_type == "antigravity")
            .flat_map(|p| p.accounts.iter())
            .map(Account::from)
            .collect();
        Self {
            accounts: Arc::new(RwLock::new(all_accounts)),
            http: Client::new(),
            config_path,
        }
    }

    /// Ensure the token for the given account index is fresh.
    pub async fn get_token(&self, index: usize) -> anyhow::Result<String> {
        let now = chrono::Utc::now().timestamp();

        {
            let accounts = self.accounts.read().await;
            if index >= accounts.len() {
                anyhow::bail!("Account index {} out of range", index);
            }
            let account = &accounts[index];
            if !account.access_token.is_empty() && account.token_expires_at > now + 300 {
                return Ok(account.access_token.clone());
            }
        }

        self.refresh_token(index).await
    }

    async fn refresh_token(&self, index: usize) -> anyhow::Result<String> {
        let refresh_token = {
            let accounts = self.accounts.read().await;
            accounts[index].refresh_token.clone()
        };

        info!(account_index = index, "Refreshing OAuth token");

        let client_id = get_client_id();
        let client_secret = get_client_secret();

        let resp = self
            .http
            .post(TOKEN_ENDPOINT)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("refresh_token", &refresh_token),
            ])
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await?;

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<TokenErrorResponse>(&body) {
                anyhow::bail!(
                    "Token refresh failed for account {}: {} - {}",
                    index,
                    err.error,
                    err.error_description.unwrap_or_default()
                );
            }
            anyhow::bail!("Token refresh failed with status {}: {}", status, body);
        }

        let token_resp: TokenResponse = serde_json::from_str(&body)?;
        let now = chrono::Utc::now().timestamp();
        let expires_at = now + token_resp.expires_in;

        {
            let mut accounts = self.accounts.write().await;
            accounts[index].access_token = token_resp.access_token.clone();
            accounts[index].token_expires_at = expires_at;
            if let Some(new_refresh) = token_resp.refresh_token {
                accounts[index].refresh_token = new_refresh;
            }
        }

        if let Err(e) = self.persist_tokens().await {
            warn!("Failed to persist tokens to config: {}", e);
        }

        Ok(token_resp.access_token)
    }

    async fn persist_tokens(&self) -> anyhow::Result<()> {
        let accounts = self.accounts.read().await;
        let mut account_cfgs = Vec::new();
        for a in accounts.iter() {
            account_cfgs.push(AccountConfig {
                name: a.name.clone(),
                email: a.email.clone(),
                refresh_token: a.refresh_token.clone(),
                access_token: a.access_token.clone(),
                token_expires_at: a.token_expires_at,
                project_id: a.project_id.clone(),
            });
        }
        crate::config::save_provider_accounts("antigravity", &account_cfgs)?;
        Ok(())
    }

    pub async fn account_count(&self) -> usize {
        let accounts = self.accounts.read().await;
        accounts.len()
    }

    /// Fetch quotas & discovered models dynamically from Google CloudCode API
    pub async fn fetch_quotas(&self) -> anyhow::Result<HashMap<String, ModelQuota>> {
        let count = self.account_count().await;
        if count == 0 {
            anyhow::bail!("No accounts available");
        }

        let mut last_err = anyhow::anyhow!("No valid account found");

        for idx in 0..count {
            let token = match self.get_token(idx).await {
                Ok(t) => t,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };

            let project_id = {
                let accounts = self.accounts.read().await;
                accounts[idx].project_id.clone()
            };

            let proj = if project_id.is_empty() {
                "aicode-consumers".to_string()
            } else {
                project_id
            };

            let body = serde_json::json!({"project": proj});
            let url = format!("{}/v1internal:fetchAvailableModels", CLOUD_CODE_BASE);

            let resp = match self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", token))
                .header("Content-Type", "application/json")
                .header("User-Agent", USER_AGENT)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = e.into();
                    continue;
                }
            };

            if !resp.status().is_success() {
                last_err = anyhow::anyhow!("status {}", resp.status());
                continue;
            }

            let data: serde_json::Value = resp.json().await?;
            let mut result = HashMap::new();

            if let Some(models) = data.get("models").and_then(|m| m.as_object()) {
                for (model_name, info) in models {
                    let display_name = info.get("displayName").and_then(|v| v.as_str()).map(|s| s.to_string());
                    if let Some(q_info) = info.get("quotaInfo") {
                        let remaining_fraction =
                            q_info.get("remainingFraction").and_then(|v| v.as_f64());
                        let reset_time = q_info
                            .get("resetTime")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        result.insert(
                            model_name.clone(),
                            ModelQuota {
                                model_id: model_name.clone(),
                                display_name,
                                remaining_fraction,
                                reset_time,
                            },
                        );
                    }
                }
            }

            return Ok(result);
        }

        Err(last_err)
    }

    /// Load user profile email from Google UserInfo endpoint
    pub async fn fetch_user_email(token: &str) -> Option<String> {
        let client = Client::new();
        let resp = client
            .get("https://www.googleapis.com/oauth2/v2/userinfo")
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .ok()?;

        if !resp.status().is_success() {
            return None;
        }

        let val: serde_json::Value = resp.json().await.ok()?;
        val.get("email").and_then(|e| e.as_str()).map(|s| s.to_string())
    }

    /// Load project ID from Google Cloud Code assist service
    pub async fn resolve_project_id(token: &str) -> Option<String> {
        let client = Client::new();
        let url = format!("{}/v1internal:loadCodeAssist", CLOUD_CODE_BASE);
        let body = serde_json::json!({
            "metadata": { "ideType": "ANTIGRAVITY" }
        });

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .json(&body)
            .send()
            .await
            .ok()?;

        if !resp.status().is_success() {
            return None;
        }

        let val: serde_json::Value = resp.json().await.ok()?;
        if let Some(proj) = val.get("cloudaicompanionProject").and_then(|p| p.as_str()) {
            return Some(proj.to_string());
        }
        if let Some(proj) = val.get("cloudaicompanionProject").and_then(|p| p.get("id")).and_then(|id| id.as_str()) {
            return Some(proj.to_string());
        }

        Some("aicode-consumers".to_string())
    }

    /// Exchange an authorization code for tokens, email, and project_id
    pub async fn exchange_code(
        code: &str,
        redirect_uri: &str,
    ) -> anyhow::Result<(TokenResponse, Option<String>, Option<String>)> {
        let client_id = get_client_id();
        let client_secret = get_client_secret();
        let http = Client::new();
        let resp = http
            .post(TOKEN_ENDPOINT)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("code", code),
                ("redirect_uri", redirect_uri),
            ])
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await?;

        if !status.is_success() {
            anyhow::bail!("Code exchange failed with status {}: {}", status, body);
        }

        let token_resp: TokenResponse = serde_json::from_str(&body)?;
        let email = Self::fetch_user_email(&token_resp.access_token).await;
        let project_id = Self::resolve_project_id(&token_resp.access_token).await;
        Ok((token_resp, email, project_id))
    }
}

/// Build the Google OAuth authorization URL
pub fn build_auth_url(state: &str) -> String {
    let scopes = SCOPES.join(" ");
    let client_id = get_client_id();
    format!(
        "{}?client_id={}&redirect_uri=http://localhost:{}/oauth-callback&response_type=code&scope={}&access_type=offline&prompt=consent&state={}",
        AUTH_ENDPOINT,
        client_id,
        CALLBACK_PORT,
        urlencoding::encode(&scopes),
        state
    )
}

/// Listen for the OAuth callback on the callback port.
/// Returns the authorization code.
pub async fn wait_for_oauth_callback() -> anyhow::Result<String> {
    let listener =
        tokio::net::TcpListener::bind(format!("127.0.0.1:{}", CALLBACK_PORT)).await?;

    let (stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        listener.accept(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Timeout waiting for OAuth callback"))??;

    let mut buf = vec![0u8; 4096];
    stream.readable().await?;
    let n = stream.try_read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Empty request"))?;

    // Send response to browser
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body><h2>Authentication successful!</h2><p>You can close this tab and return to the terminal.</p></body></html>";
    stream.writable().await?;
    let _ = stream.try_write(response.as_bytes());

    // Extract code from "GET /oauth-callback?code=...&state=... HTTP/1.1"
    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Could not parse request path"))?;

    let url_str = format!("http://localhost{}", path);
    let url = url::Url::parse(&url_str)?;
    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| anyhow::anyhow!("No 'code' parameter found in callback"))?;

    Ok(code)
}
