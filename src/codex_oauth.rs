use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub fn get_codex_client_id() -> String {
    std::env::var("CODEX_CLIENT_ID").unwrap_or_else(|_| "app_EMoamEEZ73f0CkXaXp7hrann".to_string())
}
pub const CODEX_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_CALLBACK_PORT: u16 = 1455;
pub const CODEX_CALLBACK_PATH: &str = "/auth/callback";
pub const CODEX_SCOPE: &str = "openid profile email offline_access";

#[derive(Debug, Clone)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> PkceChallenge {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);

    PkceChallenge {
        verifier,
        challenge,
    }
}

pub fn build_codex_auth_url(state: &str, challenge: &str) -> String {
    let redirect_uri = format!("http://localhost:{}{}", CODEX_CALLBACK_PORT, CODEX_CALLBACK_PATH);
    let client_id = get_codex_client_id();
    format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=codex_cli_rs&state={}",
        CODEX_AUTH_URL,
        client_id,
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(CODEX_SCOPE),
        urlencoding::encode(challenge),
        urlencoding::encode(state),
    )
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CodexTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexAccountInfo {
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub plan_type: Option<String>,
}

pub fn parse_jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let payload_b64 = parts[1];
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(payload_b64))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub fn extract_codex_info(id_token: &str) -> CodexAccountInfo {
    let mut info = CodexAccountInfo::default();
    if let Some(claims) = parse_jwt_claims(id_token) {
        if let Some(email) = claims.get("email").and_then(|e| e.as_str()) {
            info.email = Some(email.to_string());
        }
        if let Some(auth_obj) = claims.get("https://api.openai.com/auth") {
            if let Some(acc_id) = auth_obj.get("chatgpt_account_id").and_then(|s| s.as_str()) {
                info.account_id = Some(acc_id.to_string());
            }
            if let Some(plan) = auth_obj.get("chatgpt_plan_type").and_then(|s| s.as_str()) {
                info.plan_type = Some(plan.to_string());
            }
        }
    }
    info
}

pub async fn exchange_codex_code(
    client: &Client,
    code: &str,
    verifier: &str,
) -> anyhow::Result<(CodexTokenResponse, CodexAccountInfo)> {
    let redirect_uri = format!("http://localhost:{}{}", CODEX_CALLBACK_PORT, CODEX_CALLBACK_PATH);

    let mut params = HashMap::new();
    params.insert("grant_type", "authorization_code");
    let client_id = get_codex_client_id();
    params.insert("client_id", &client_id);
    params.insert("code", code);
    params.insert("redirect_uri", &redirect_uri);
    params.insert("code_verifier", verifier);

    let resp = client
        .post(CODEX_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&params)
        .send()
        .await?;

    let status = resp.status();
    let body = resp.text().await?;

    if !status.is_success() {
        anyhow::bail!("Codex token exchange failed (HTTP {}): {}", status, body);
    }

    let token_resp: CodexTokenResponse = serde_json::from_str(&body)?;
    let mut account_info = CodexAccountInfo::default();
    if let Some(ref id_token) = token_resp.id_token {
        account_info = extract_codex_info(id_token);
    }

    Ok((token_resp, account_info))
}

pub async fn fetch_codex_models(
    client: &Client,
    access_token: &str,
) -> anyhow::Result<Vec<DiscoveredModel>> {
    let url = "https://chatgpt.com/backend-api/codex/models?client_version=0.159.0";
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("originator", "codex_cli_rs")
        .header("User-Agent", "codex_cli_rs/0.159.0")
        .send()
        .await?;

    if !resp.status().is_success() {
        let err = resp.text().await?;
        anyhow::bail!("Codex fetch models failed: {}", err);
    }

    let val: serde_json::Value = resp.json().await?;
    let items = val
        .get("models")
        .or_else(|| val.get("data"))
        .and_then(|v| v.as_array());

    let mut result = Vec::new();
    if let Some(models) = items {
        for m in models {
            let id = m.get("id")
                .or_else(|| m.get("slug"))
                .or_else(|| m.get("model"))
                .and_then(|v| v.as_str());

            if let Some(model_id) = id {
                let name = m.get("display_name")
                    .or_else(|| m.get("displayName"))
                    .or_else(|| m.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(model_id);

                result.push(DiscoveredModel {
                    id: model_id.to_string(),
                    name: name.to_string(),
                });
            }
        }
    }

    Ok(result)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub name: String,
}

pub async fn wait_for_codex_callback() -> anyhow::Result<String> {
    let listener =
        tokio::net::TcpListener::bind(format!("127.0.0.1:{}", CODEX_CALLBACK_PORT)).await?;

    let (stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        listener.accept(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Timeout waiting for Codex OAuth callback on port 1455"))??;

    let mut buf = vec![0u8; 4096];
    stream.readable().await?;
    let n = stream.try_read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Empty request"))?;

    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body><h2>Codex Authentication Successful!</h2><p>You can close this window and return to your terminal.</p></body></html>";
    stream.writable().await?;
    let _ = stream.try_write(response.as_bytes());

    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Could not parse request path"))?;

    let url = url::Url::parse(&format!("http://localhost{}", path))?;
    for (k, v) in url.query_pairs() {
        if k == "code" {
            return Ok(v.to_string());
        }
    }

    anyhow::bail!("No authorization code found in Codex callback URL")
}
