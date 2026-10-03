use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use crate::config::Config;
use crate::oauth::OAuthManager;

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub kind: LogKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Incoming,
    Outgoing,
    Success,
    Warn,
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct ModelQuota {
    pub model_id: String,
    pub display_name: Option<String>,
    pub remaining_fraction: Option<f64>, // e.g. 0.93 -> 93%
    pub reset_time: Option<String>,
}

pub struct AppState {
    pub config: Config,
    pub oauth: OAuthManager,
    pub counter: AtomicUsize,
    pub http: reqwest::Client,
    pub log_tx: Option<mpsc::UnboundedSender<LogEntry>>,
    pub quotas: Arc<RwLock<HashMap<String, ModelQuota>>>,
}

impl AppState {
    /// Pick the next account index via round-robin.
    pub fn next_account_index(&self) -> usize {
        let accs = self.config.antigravity_accounts();
        if accs.is_empty() {
            return 0;
        }
        self.counter.fetch_add(1, Ordering::Relaxed) % accs.len()
    }

    pub fn log(&self, kind: LogKind, message: impl Into<String>) {
        if let Some(tx) = &self.log_tx {
            let now = chrono::Local::now().format("%H:%M:%S").to_string();
            let _ = tx.send(LogEntry {
                timestamp: now,
                kind,
                message: message.into(),
            });
        }
    }
}

pub type SharedState = Arc<AppState>;
