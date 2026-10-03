use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ServerConfig {
    pub bind: String,
    pub master_key: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(
        alias = "provider_type",
        alias = "type",
        default = "default_provider_type"
    )]
    pub provider_type: String, // "antigravity", "gemini", "openai", etc.
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

fn default_provider_type() -> String {
    "antigravity".to_string()
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct AccountConfig {
    pub name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub token_expires_at: i64,
    #[serde(default)]
    pub project_id: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct AccountsFile {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

pub fn default_storage_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("llm-router")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("llm-router")
    } else {
        PathBuf::from(".llm-router")
    }
}

pub fn provider_accounts_path(provider_type: &str) -> PathBuf {
    let dir = default_storage_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{}-accounts.json", provider_type))
}

pub fn load_provider_accounts(provider_type: &str) -> Vec<AccountConfig> {
    let path = provider_accounts_path(provider_type);
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str::<AccountsFile>(&content)
            .map(|f| f.accounts)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_provider_accounts(provider_type: &str, accounts: &[AccountConfig]) -> anyhow::Result<()> {
    let path = provider_accounts_path(provider_type);
    let payload = AccountsFile {
        accounts: accounts.to_vec(),
    };
    let content = serde_json::to_string_pretty(&payload)?;
    std::fs::write(path, content)?;
    Ok(())
}

#[allow(dead_code)]
impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut config: Config = if path.exists() {
            let content = std::fs::read_to_string(path)?;
            toml::from_str(&content)?
        } else {
            Config::default()
        };

        // Ensure default providers list exists
        if config.providers.is_empty() {
            config.providers.push(ProviderConfig {
                name: "Antigravity (Google)".to_string(),
                provider_type: "antigravity".to_string(),
                accounts: Vec::new(),
            });
            config.providers.push(ProviderConfig {
                name: "OpenAI Codex".to_string(),
                provider_type: "codex".to_string(),
                accounts: Vec::new(),
            });
        }

        // Dynamically populate accounts from external storage
        for p in config.providers.iter_mut() {
            let loaded = load_provider_accounts(&p.provider_type);
            if !loaded.is_empty() {
                p.accounts = loaded;
            }
        }

        Ok(config)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        // Save config.toml without dumping tokens
        let mut clean_config = self.clone();
        for p in clean_config.providers.iter_mut() {
            p.accounts.clear();
        }
        let content = toml::to_string_pretty(&clean_config)?;
        std::fs::write(path, content)?;

        // Save each provider's accounts to dedicated json storage
        for p in &self.providers {
            let _ = save_provider_accounts(&p.provider_type, &p.accounts);
        }

        Ok(())
    }

    /// Get all accounts for a specific provider type
    pub fn accounts_for(&self, provider_type: &str) -> Vec<&AccountConfig> {
        self.providers
            .iter()
            .filter(|p| p.provider_type == provider_type)
            .flat_map(|p| p.accounts.iter())
            .collect()
    }

    /// Shortcut for antigravity accounts
    pub fn antigravity_accounts(&self) -> Vec<&AccountConfig> {
        self.accounts_for("antigravity")
    }

    /// Shortcut for codex accounts
    pub fn codex_accounts(&self) -> Vec<&AccountConfig> {
        self.accounts_for("codex")
    }

    /// Resolve an incoming model name: check models map first, or pass through directly
    pub fn resolve_model(&self, requested: &str) -> String {
        if let Some(upstream) = self.models.get(requested) {
            return upstream.clone();
        }
        // Normalize common alias names if not mapped
        match requested {
            "claude-sonnet-4.5" | "claude-sonnet" => "claude-sonnet-4-6".to_string(),
            "claude-opus-4.5" | "claude-opus" => "claude-opus-4-6-thinking".to_string(),
            "gemini-3.8-flash" => "gemini-3.8-flash-tiered".to_string(),
            "gemini-3.7-flash" => "gemini-3.7-flash-tiered".to_string(),
            "gemini-3.6-flash" => "gemini-3.6-flash-high".to_string(),
            "gemini-3-pro" => "gemini-3.1-pro-high".to_string(),
            "gpt-oss" => "gpt-oss-120b-medium".to_string(),
            other => other.to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind: "127.0.0.1:8787".to_string(),
                master_key: None,
            },
            providers: vec![ProviderConfig {
                name: "Antigravity (Google)".to_string(),
                provider_type: "antigravity".to_string(),
                accounts: vec![],
            }],
            models: HashMap::new(),
        }
    }
}
