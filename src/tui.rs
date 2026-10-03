use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::Frame;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::{AccountConfig, Config, ProviderConfig};
use crate::oauth;
use crate::proxy;
use crate::state::{self, LogEntry, LogKind, ModelQuota};

// ─── Screens ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Screen {
    Main,
    Provider(usize), // index into config.providers
    AddAccount(AddAccountState),
    Logs,
}

#[derive(Debug, Clone, PartialEq)]
enum AddAccountState {
    WaitingForAuth {
        url: String,
        provider_idx: usize,
        pkce_verifier: Option<String>,
    },
    Success { account_name: String },
    Error { message: String },
}

// ─── Background events ────────────────────────────────────────────

enum BgEvent {
    OAuthCode(String),
    OAuthSuccess(String),
    OAuthError(String),
    ServerStarted,
    ServerStopped,
    Log(LogEntry),
    QuotasUpdated(HashMap<String, ModelQuota>),
    CodexModelsUpdated(Vec<crate::codex_oauth::DiscoveredModel>),
}

// ─── App ───────────────────────────────────────────────────────────

pub struct App {
    config: Config,
    config_path: PathBuf,
    screen: Screen,
    should_quit: bool,

    // Main screen
    main_list: ListState,
    provider_types: Vec<(&'static str, &'static str)>, // (type_id, display_name)

    // Provider screen
    provider_list: ListState,

    // Server
    server_running: bool,
    server_handle: Option<JoinHandle<()>>,
    request_count: u64,

    // Clipboard
    clipboard: Option<arboard::Clipboard>,

    // OAuth manager
    oauth_mgr: oauth::OAuthManager,

    // Live Request / Response logs
    logs: Vec<LogEntry>,
    logs_scroll: usize,

    // Model Quotas & Discovered Models
    quotas: HashMap<String, ModelQuota>,
    codex_models: Vec<crate::codex_oauth::DiscoveredModel>,

    // Background channel
    bg_tx: mpsc::UnboundedSender<BgEvent>,
    bg_rx: mpsc::UnboundedReceiver<BgEvent>,

    // OAuth callback listener handle
    oauth_handle: Option<JoinHandle<()>>,

    // Status message (bottom bar)
    status_msg: Option<(String, std::time::Instant)>,
}

impl App {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        let mut main_list = ListState::default();
        main_list.select(Some(0));

        let oauth_mgr = oauth::OAuthManager::from_config(&config, config_path.clone());

        let app = Self {
            config,
            config_path,
            screen: Screen::Main,
            should_quit: false,

            main_list,
            provider_types: vec![
                ("antigravity", "Antigravity (Google)"),
                ("codex", "OpenAI Codex"),
            ],

            provider_list: ListState::default(),

            server_running: false,
            server_handle: None,
            request_count: 0,

            clipboard: arboard::Clipboard::new().ok(),
            oauth_mgr,

            logs: Vec::new(),
            logs_scroll: 0,

            quotas: HashMap::new(),
            codex_models: Vec::new(),

            bg_tx,
            bg_rx,
            oauth_handle: None,
            status_msg: None,
        };

        // Trigger initial quota refresh in background if accounts exist
        app.trigger_quota_refresh();

        app
    }

    fn set_status(&mut self, msg: &str) {
        self.status_msg = Some((msg.to_string(), std::time::Instant::now()));
    }

    fn trigger_quota_refresh(&self) {
        let oauth_mgr = self.oauth_mgr.clone();
        let config = self.config.clone();
        let tx = self.bg_tx.clone();

        tokio::spawn(async move {
            if oauth_mgr.account_count().await > 0 {
                if let Ok(quotas) = oauth_mgr.fetch_quotas().await {
                    let _ = tx.send(BgEvent::QuotasUpdated(quotas));
                }
            }

            // Also dynamically fetch Codex models if codex accounts are configured
            let codex_accounts = config.codex_accounts();
            if let Some(acc) = codex_accounts.first() {
                let client = reqwest::Client::new();
                if let Ok(models) = crate::codex_oauth::fetch_codex_models(&client, &acc.access_token).await {
                    let _ = tx.send(BgEvent::CodexModelsUpdated(models));
                }
            }
        });
    }

    pub fn auto_start_if_ready(&mut self) {
        if !self.config.antigravity_accounts().is_empty() && !self.server_running {
            self.start_server();
        }
    }

    // ─── Event handling ────────────────────────────────────────────

    pub fn handle_key(&mut self, code: KeyCode) {
        match &self.screen {
            Screen::Main => self.handle_main_key(code),
            Screen::Provider(_) => self.handle_provider_key(code),
            Screen::AddAccount(_) => self.handle_add_account_key(code),
            Screen::Logs => self.handle_logs_key(code),
        }
    }

    fn handle_main_key(&mut self, code: KeyCode) {
        let item_count = self.provider_types.len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('l') => {
                self.screen = Screen::Logs;
                self.logs_scroll = 0;
            }
            KeyCode::Char('r') => {
                self.trigger_quota_refresh();
                self.set_status("Refreshing model quotas...");
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = self.main_list.selected().unwrap_or(0);
                let new = if i == 0 { item_count.saturating_sub(1) } else { i - 1 };
                self.main_list.select(Some(new));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = self.main_list.selected().unwrap_or(0);
                let new = if i >= item_count.saturating_sub(1) { 0 } else { i + 1 };
                self.main_list.select(Some(new));
            }
            KeyCode::Enter => {
                let selected = self.main_list.selected().unwrap_or(0);
                if selected < self.provider_types.len() {
                    let type_id = self.provider_types[selected].0;
                    let prov_idx = self
                        .config
                        .providers
                        .iter()
                        .position(|p| p.provider_type == type_id);
                    let idx = match prov_idx {
                        Some(i) => i,
                        None => {
                            self.config.providers.push(ProviderConfig {
                                name: self.provider_types[selected].1.to_string(),
                                provider_type: type_id.to_string(),
                                accounts: vec![],
                            });
                            let _ = self.config.save(&self.config_path);
                            self.config.providers.len() - 1
                        }
                    };
                    self.provider_list = ListState::default();
                    self.provider_list.select(Some(0));
                    self.screen = Screen::Provider(idx);
                }
            }
            KeyCode::Char('s') => {
                self.toggle_server();
            }
            _ => {}
        }
    }

    fn handle_provider_key(&mut self, code: KeyCode) {
        let prov_idx = match &self.screen {
            Screen::Provider(i) => *i,
            _ => return,
        };

        let account_count = self.config.providers[prov_idx].accounts.len();
        let item_count = account_count + 1; // accounts + "Add Account"

        match code {
            KeyCode::Esc | KeyCode::Backspace => {
                self.screen = Screen::Main;
            }
            KeyCode::Char('l') => {
                self.screen = Screen::Logs;
                self.logs_scroll = 0;
            }
            KeyCode::Char('r') => {
                self.trigger_quota_refresh();
                self.set_status("Refreshing model quotas...");
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = self.provider_list.selected().unwrap_or(0);
                let new = if i == 0 { item_count.saturating_sub(1) } else { i - 1 };
                self.provider_list.select(Some(new));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = self.provider_list.selected().unwrap_or(0);
                let new = if i >= item_count.saturating_sub(1) { 0 } else { i + 1 };
                self.provider_list.select(Some(new));
            }
            KeyCode::Enter => {
                let selected = self.provider_list.selected().unwrap_or(0);
                if selected == account_count {
                    self.start_add_account(prov_idx);
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                let selected = self.provider_list.selected().unwrap_or(0);
                if selected < account_count {
                    let name = self.config.providers[prov_idx].accounts[selected]
                        .name
                        .clone();
                    self.config.providers[prov_idx].accounts.remove(selected);
                    let _ = self.config.save(&self.config_path);
                    self.reload_config();
                    self.set_status(&format!("Removed account '{}'", name));
                    self.trigger_quota_refresh();
                    let new_count = self.config.providers[prov_idx].accounts.len();
                    if selected >= new_count && selected > 0 {
                        self.provider_list.select(Some(selected - 1));
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_add_account_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Backspace => {
                if let Some(h) = self.oauth_handle.take() {
                    h.abort();
                }
                if let Screen::AddAccount(AddAccountState::WaitingForAuth {
                    provider_idx, ..
                }) = &self.screen
                {
                    self.screen = Screen::Provider(*provider_idx);
                } else {
                    self.screen = Screen::Main;
                }
            }
            KeyCode::Char('c') | KeyCode::Char('y') => {
                if let Screen::AddAccount(AddAccountState::WaitingForAuth { url, .. }) = &self.screen
                {
                    let url_str = url.clone();
                    let dir = crate::config::default_storage_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    // Write URL file for simple terminal/script access or manual copy
                    let _ = std::fs::write(dir.join("latest-auth-url.txt"), &url_str);

                    let mut copied = false;
                    if let Some(cb) = &mut self.clipboard {
                        if cb.set_text(url_str.clone()).is_ok() {
                            copied = true;
                        }
                    }
                    if !copied {
                        let _ = copy_to_clipboard(&url_str);
                    }
                    self.set_status("Copied auth URL! (Also saved to ~/.config/llm-router/latest-auth-url.txt)");
                }
            }
            KeyCode::Char('o') => {
                if let Screen::AddAccount(AddAccountState::WaitingForAuth { url, .. }) = &self.screen
                {
                    #[cfg(target_os = "linux")]
                    {
                        // Spawn detached so it does not block TUI or fail with xdg-open portal hangs
                        let _ = std::process::Command::new("firefox")
                            .arg("--new-tab")
                            .arg(url)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                            .or_else(|_| {
                                std::process::Command::new("xdg-open")
                                    .arg(url)
                                    .stdin(std::process::Stdio::null())
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .spawn()
                            });
                        self.set_status("Opened browser with auth URL");
                    }
                }
            }
            KeyCode::Enter => {
                match &self.screen {
                    Screen::AddAccount(AddAccountState::Success { .. }) => {
                        let prov_idx = self.config.providers.len().saturating_sub(1);
                        self.provider_list = ListState::default();
                        self.provider_list.select(Some(0));
                        self.screen = Screen::Provider(prov_idx);
                    }
                    Screen::AddAccount(AddAccountState::Error { .. }) => {
                        self.screen = Screen::Main;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn handle_logs_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace | KeyCode::Char('l') => {
                self.screen = Screen::Main;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.logs_scroll = self.logs_scroll.saturating_add(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.logs_scroll = self.logs_scroll.saturating_sub(1);
            }
            KeyCode::Char('c') => {
                self.logs.clear();
                self.logs_scroll = 0;
                self.set_status("Logs cleared");
            }
            _ => {}
        }
    }

    // ─── Add Account Flow ──────────────────────────────────────────

    fn start_add_account(&mut self, provider_idx: usize) {
        let provider_type = self.config.providers.get(provider_idx)
            .map(|p| p.provider_type.as_str())
            .unwrap_or("antigravity");

        let state_param = uuid::Uuid::new_v4().to_string();

        if provider_type == "codex" {
            let pkce = crate::codex_oauth::generate_pkce();
            let auth_url = crate::codex_oauth::build_codex_auth_url(&state_param, &pkce.challenge);

            self.screen = Screen::AddAccount(AddAccountState::WaitingForAuth {
                url: auth_url,
                provider_idx,
                pkce_verifier: Some(pkce.verifier),
            });

            let tx = self.bg_tx.clone();
            let handle = tokio::spawn(async move {
                match crate::codex_oauth::wait_for_codex_callback().await {
                    Ok(code) => {
                        let _ = tx.send(BgEvent::OAuthCode(code));
                    }
                    Err(e) => {
                        let _ = tx.send(BgEvent::OAuthError(e.to_string()));
                    }
                }
            });
            self.oauth_handle = Some(handle);
        } else {
            let auth_url = oauth::build_auth_url(&state_param);

            self.screen = Screen::AddAccount(AddAccountState::WaitingForAuth {
                url: auth_url,
                provider_idx,
                pkce_verifier: None,
            });

            let tx = self.bg_tx.clone();
            let handle = tokio::spawn(async move {
                match oauth::wait_for_oauth_callback().await {
                    Ok(code) => {
                        let _ = tx.send(BgEvent::OAuthCode(code));
                    }
                    Err(e) => {
                        let _ = tx.send(BgEvent::OAuthError(e.to_string()));
                    }
                }
            });
            self.oauth_handle = Some(handle);
        }
    }

    // ─── Process background events ────────────────────────────────

    pub fn poll_bg_events(&mut self) {
        while let Ok(event) = self.bg_rx.try_recv() {
            match event {
                BgEvent::OAuthCode(code) => {
                    self.handle_oauth_code(code);
                }
                BgEvent::OAuthSuccess(account_name) => {
                    self.reload_config();
                    self.screen = Screen::AddAccount(AddAccountState::Success { account_name });
                    self.trigger_quota_refresh();
                }
                BgEvent::OAuthError(msg) => {
                    self.screen = Screen::AddAccount(AddAccountState::Error { message: msg });
                }
                BgEvent::ServerStarted => {
                    self.server_running = true;
                    self.set_status("Server started");
                }
                BgEvent::ServerStopped => {
                    self.server_running = false;
                    self.set_status("Server stopped");
                }
                BgEvent::Log(entry) => {
                    if entry.kind == LogKind::Success {
                        self.request_count += 1;
                    }
                    self.logs.push(entry);
                    if self.logs.len() > 1000 {
                        self.logs.remove(0);
                    }
                }
                BgEvent::QuotasUpdated(quotas) => {
                    self.quotas = quotas;
                    self.set_status("Quotas updated");
                }
                BgEvent::CodexModelsUpdated(models) => {
                    self.codex_models = models;
                }
            }
        }
    }

    fn handle_oauth_code(&mut self, code: String) {
        let (provider_idx, pkce_verifier) = match &self.screen {
            Screen::AddAccount(AddAccountState::WaitingForAuth {
                provider_idx,
                pkce_verifier,
                ..
            }) => (*provider_idx, pkce_verifier.clone()),
            _ => return,
        };

        let provider_type = self
            .config
            .providers
            .get(provider_idx)
            .map(|p| p.provider_type.clone())
            .unwrap_or_else(|| "antigravity".to_string());

        let tx = self.bg_tx.clone();
        let config_path = self.config_path.clone();
        let config = self.config.clone();

        if provider_type == "codex" {
            let verifier = pkce_verifier.unwrap_or_default();
            tokio::spawn(async move {
                let client = reqwest::Client::new();
                match crate::codex_oauth::exchange_codex_code(&client, &code, &verifier).await {
                    Ok((token_resp, account_info)) => {
                        let mut config = config;
                        let email = account_info.email.unwrap_or_else(|| {
                            let count = config.providers[provider_idx].accounts.len() + 1;
                            format!("codex-account{}", count)
                        });

                        let account = AccountConfig {
                            name: email.clone(),
                            email: email.clone(),
                            refresh_token: token_resp.refresh_token.unwrap_or_default(),
                            access_token: token_resp.access_token,
                            token_expires_at: chrono::Utc::now().timestamp()
                                + token_resp.expires_in.unwrap_or(3600) as i64,
                            project_id: account_info.account_id.unwrap_or_default(),
                        };

                        config.providers[provider_idx].accounts.push(account);
                        let _ = config.save(&config_path);

                        let _ = tx.send(BgEvent::OAuthSuccess(email));
                    }
                    Err(e) => {
                        let _ = tx.send(BgEvent::OAuthError(e.to_string()));
                    }
                }
            });

            self.screen = Screen::AddAccount(AddAccountState::WaitingForAuth {
                url: "Exchanging authorization code with OpenAI...".to_string(),
                provider_idx,
                pkce_verifier: None,
            });
        } else {
            let redirect_uri = format!("http://localhost:{}/oauth-callback", oauth::CALLBACK_PORT);
            tokio::spawn(async move {
                match oauth::OAuthManager::exchange_code(&code, &redirect_uri).await {
                    Ok((token_resp, email_opt, project_id_opt)) => {
                        let mut config = config;
                        let email = email_opt.unwrap_or_else(|| {
                            let count = config.providers[provider_idx].accounts.len() + 1;
                            format!("account{}", count)
                        });

                        let account = AccountConfig {
                            name: email.clone(),
                            email: email.clone(),
                            refresh_token: token_resp.refresh_token.unwrap_or_default(),
                            access_token: token_resp.access_token,
                            token_expires_at: chrono::Utc::now().timestamp()
                                + token_resp.expires_in,
                            project_id: project_id_opt
                                .unwrap_or_else(|| "aicode-consumers".to_string()),
                        };

                        config.providers[provider_idx].accounts.push(account);
                        let _ = config.save(&config_path);

                        let _ = tx.send(BgEvent::OAuthSuccess(email));
                    }
                    Err(e) => {
                        let _ = tx.send(BgEvent::OAuthError(e.to_string()));
                    }
                }
            });

            self.screen = Screen::AddAccount(AddAccountState::WaitingForAuth {
                url: "Exchanging authorization code with Google...".to_string(),
                provider_idx,
                pkce_verifier: None,
            });
        }
    }

    // ─── Server toggle ─────────────────────────────────────────────

    fn toggle_server(&mut self) {
        if self.server_running {
            if let Some(h) = self.server_handle.take() {
                h.abort();
                self.server_running = false;
                self.set_status("Server stopped");
            }
        } else {
            self.start_server();
        }
    }

    fn start_server(&mut self) {
        let config = self.config.clone();
        let config_path = self.config_path.clone();
        let tx = self.bg_tx.clone();
        let bind = config.server.bind.clone();

        let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogEntry>();
        let bg_tx_clone = self.bg_tx.clone();

        tokio::spawn(async move {
            while let Some(entry) = log_rx.recv().await {
                let _ = bg_tx_clone.send(BgEvent::Log(entry));
            }
        });

        let handle = tokio::spawn(async move {
            let oauth_mgr = oauth::OAuthManager::from_config(&config, config_path);

            let http_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap();

            let state = Arc::new(state::AppState {
                config,
                oauth: oauth_mgr,
                counter: AtomicUsize::new(0),
                http: http_client,
                log_tx: Some(log_tx),
                quotas: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            });

            let app = axum::Router::new()
                .route(
                    "/v1/chat/completions",
                    axum::routing::post(proxy::chat_completions),
                )
                .route("/v1/models", axum::routing::get(proxy::list_models))
                .route("/health", axum::routing::get(proxy::health))
                .with_state(state);

            let _ = tx.send(BgEvent::ServerStarted);

            match tokio::net::TcpListener::bind(&bind).await {
                Ok(listener) => {
                    let _ = axum::serve(listener, app).await;
                }
                Err(e) => {
                    let _ = tx.send(BgEvent::OAuthError(format!("Server bind failed: {}", e)));
                }
            }

            let _ = tx.send(BgEvent::ServerStopped);
        });

        self.server_handle = Some(handle);
        self.set_status(&format!("Starting server on {}...", self.config.server.bind));
    }

    // ─── Rendering ─────────────────────────────────────────────────

    pub fn draw(&mut self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // title bar
                Constraint::Min(5),    // main content
                Constraint::Length(3), // status / keybindings
            ])
            .split(frame.area());

        self.draw_title_bar(frame, chunks[0]);

        match self.screen.clone() {
            Screen::Main => self.draw_main(frame, chunks[1]),
            Screen::Provider(idx) => self.draw_provider(frame, chunks[1], idx),
            Screen::AddAccount(state) => self.draw_add_account(frame, chunks[1], &state),
            Screen::Logs => self.draw_logs(frame, chunks[1]),
        }

        self.draw_status_bar(frame, chunks[2]);
    }

    fn draw_title_bar(&self, frame: &mut Frame, area: Rect) {
        let server_status = if self.server_running {
            Span::styled(" ● RUNNING ", Style::default().fg(Color::Black).bg(Color::Green))
        } else {
            Span::styled(" ○ STOPPED ", Style::default().fg(Color::Black).bg(Color::DarkGray))
        };

        let title = Line::from(vec![
            Span::styled(" LLM Router ", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" │ "),
            server_status,
            Span::raw(" │ "),
            Span::styled(
                format!(" {} ", self.config.server.bind),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(" │ "),
            Span::styled(
                format!(" {} reqs ", self.request_count),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ "),
            Span::styled(
                format!(" [L] Logs ({}) ", self.logs.len()),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw(" │ "),
            Span::styled(" [R] Refresh Quota ", Style::default().fg(Color::Blue)),
        ]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let para = Paragraph::new(title).block(block);
        frame.render_widget(para, area);
    }

    fn draw_main(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);

        // Left: Provider list
        let mut items: Vec<ListItem> = Vec::new();
        for (type_id, display_name) in &self.provider_types {
            let account_count = self
                .config
                .providers
                .iter()
                .find(|p| p.provider_type == *type_id)
                .map(|p| p.accounts.len())
                .unwrap_or(0);

            let authed = self
                .config
                .providers
                .iter()
                .find(|p| p.provider_type == *type_id)
                .map(|p| {
                    p.accounts
                        .iter()
                        .filter(|a| !a.refresh_token.is_empty())
                        .count()
                })
                .unwrap_or(0);

            let status = if account_count == 0 {
                Span::styled(" (no accounts)", Style::default().fg(Color::DarkGray))
            } else {
                Span::styled(
                    format!(" ({}/{} authed)", authed, account_count),
                    Style::default().fg(if authed > 0 {
                        Color::Green
                    } else {
                        Color::Red
                    }),
                )
            };

            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    format!("  {} ", display_name),
                    Style::default().fg(Color::White),
                ),
                status,
            ])));
        }

        let list = List::new(items)
            .block(
                Block::default()
                    .title(" Providers ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▸ ");

        frame.render_stateful_widget(list, chunks[0], &mut self.main_list);

        // Right: Dynamic Models & Quota Tracker
        let mut model_items: Vec<ListItem> = Vec::new();

        if self.quotas.is_empty() {
            model_items.push(ListItem::new(Line::from(vec![
                Span::styled("  Fetching available models from Antigravity...", Style::default().fg(Color::Yellow)),
            ])));
        } else {
            // Sort models alphabetically
            let mut sorted_keys: Vec<_> = self.quotas.keys().cloned().collect();
            sorted_keys.sort();

            for model_id in sorted_keys {
                // Filter out non-LLM models like tab completion
                if model_id.starts_with("tab_") || model_id.starts_with("chat_") {
                    continue;
                }

                let quota = self.quotas.get(&model_id).unwrap();

                let (quota_span, bar_span) = match quota.remaining_fraction {
                    Some(frac) => {
                        let pct = (frac * 100.0).round() as u32;
                        let color = if pct > 50 {
                            Color::Green
                        } else if pct > 20 {
                            Color::Yellow
                        } else {
                            Color::Red
                        };

                        let filled = (pct as usize / 10).min(10);
                        let empty = 10 - filled;
                        let bar = format!("[{}{}]", "█".repeat(filled), "░".repeat(empty));

                        (
                            Span::styled(format!("{:>3}%", pct), Style::default().fg(color).bold()),
                            Span::styled(format!(" {} ", bar), Style::default().fg(color)),
                        )
                    }
                    None => (
                        Span::styled("100%", Style::default().fg(Color::Green).bold()),
                        Span::styled(" [██████████] ", Style::default().fg(Color::Green)),
                    ),
                };

                let display = quota.display_name.as_deref().unwrap_or(&model_id);

                model_items.push(ListItem::new(Line::from(vec![
                    Span::styled(format!(" {:<24} ", display), Style::default().fg(Color::Cyan).bold()),
                    bar_span,
                    quota_span,
                    Span::styled(format!(" ({})", model_id), Style::default().fg(Color::DarkGray)),
                ])));
            }
        }

        let model_list = List::new(model_items).block(
            Block::default()
                .title(format!(" Available Models ({}) — Press [R] to sync ", self.quotas.len()))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

        frame.render_widget(model_list, chunks[1]);
    }

    fn draw_provider(&mut self, frame: &mut Frame, area: Rect, prov_idx: usize) {
        let provider = &self.config.providers[prov_idx];

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);

        // Left: Account list
        let mut items: Vec<ListItem> = Vec::new();

        for account in &provider.accounts {
            let status = if !account.refresh_token.is_empty() {
                let expired = account.token_expires_at < chrono::Utc::now().timestamp();
                if expired {
                    Span::styled(" ⟳ token expired", Style::default().fg(Color::Yellow))
                } else {
                    Span::styled(" ✓ authenticated", Style::default().fg(Color::Green))
                }
            } else {
                Span::styled(" ✗ not authenticated", Style::default().fg(Color::Red))
            };

            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    format!("  {} ", account.name),
                    Style::default().fg(Color::White),
                ),
                status,
            ])));
        }

        // Add Account option
        items.push(ListItem::new(Line::from(vec![Span::styled(
            "  [+ Add Account] ",
            Style::default().fg(Color::Cyan).bold(),
        )])));

        let title = format!(" {} — Accounts ", provider.name);
        let list = List::new(items)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▸ ");

        frame.render_stateful_widget(list, chunks[0], &mut self.provider_list);

        // Right: Model quota summary
        let mut info_lines = vec![
            Line::from(Span::styled(
                "Model Quotas & Limits",
                Style::default().fg(Color::Yellow).bold(),
            )),
            Line::from(""),
        ];

        let provider_type = &provider.provider_type;

        if provider_type == "codex" {
            if self.codex_models.is_empty() {
                if provider.accounts.is_empty() {
                    info_lines.push(Line::from(Span::styled(
                        "  Add a Codex account to discover live models",
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    info_lines.push(Line::from(Span::styled(
                        "  Fetching available models from OpenAI...",
                        Style::default().fg(Color::Yellow),
                    )));
                }
            } else {
                for m in &self.codex_models {
                    info_lines.push(Line::from(vec![
                        Span::styled(format!("  {:<24} ", m.name), Style::default().fg(Color::Cyan)),
                        Span::styled(format!("({})", m.id), Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }

            info_lines.push(Line::from(""));
            info_lines.push(Line::from(Span::styled(
                "Discovered via https://chatgpt.com/backend-api/codex/models",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            let models = [
                ("Claude Sonnet 4.6", "claude-sonnet-4-6"),
                ("Claude Opus 4.6", "claude-opus-4-6-thinking"),
                ("Gemini 3.8 Flash", "gemini-3.8-flash-tiered"),
                ("Gemini 3.7 Flash", "gemini-3.7-flash-tiered"),
                ("Gemini 3.6 Flash", "gemini-3.6-flash-high"),
                ("Gemini 3.1 Pro", "gemini-3.1-pro-high"),
                ("Gemini 2.5 Pro", "gemini-2.5-pro"),
                ("GPT-OSS 120B", "gpt-oss-120b-medium"),
            ];

            for (label, model_id) in &models {
                let quota = self.quotas.get(*model_id);
                let quota_str = match quota {
                    Some(q) => {
                        if let Some(frac) = q.remaining_fraction {
                            let pct = (frac * 100.0).round() as u32;
                            format!("{}%", pct)
                        } else if let Some(ref reset) = q.reset_time {
                            format!("Reset: {}", &reset[11..19.min(reset.len())])
                        } else {
                            "100%".to_string()
                        }
                    }
                    None => "Syncing...".to_string(),
                };

                info_lines.push(Line::from(vec![
                    Span::styled(format!("  {:<18} ", label), Style::default().fg(Color::Cyan)),
                    Span::styled(quota_str, Style::default().fg(Color::Green).bold()),
                ]));
            }

            info_lines.push(Line::from(""));
            info_lines.push(Line::from(Span::styled(
                "Press [R] to re-query Google CloudCode quota",
                Style::default().fg(Color::DarkGray),
            )));
        }

        let info = Paragraph::new(info_lines).block(
            Block::default()
                .title(" Live Quota Tracker ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .padding(Padding::horizontal(1)),
        );

        frame.render_widget(info, chunks[1]);
    }

    fn draw_add_account(&self, frame: &mut Frame, area: Rect, state: &AddAccountState) {
        let dialog = centered_rect(80, 60, area);
        frame.render_widget(Clear, dialog);

        match state {
            AddAccountState::WaitingForAuth { url, provider_idx, .. } => {
                let provider_name = self.config.providers.get(*provider_idx)
                    .map(|p| p.name.as_str())
                    .unwrap_or("Provider");

                let lines = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("  {} Authentication", provider_name),
                        Style::default().fg(Color::Cyan).bold(),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  Copy the OAuth URL below and open it in your browser:",
                        Style::default().fg(Color::White),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("  {}", url),
                        Style::default().fg(Color::Yellow),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("  [C / Y]", Style::default().fg(Color::Green).bold()),
                        Span::styled(" Copy URL to clipboard     ", Style::default().fg(Color::White)),
                        Span::styled("  [O]", Style::default().fg(Color::Blue).bold()),
                        Span::styled(" Open browser automatically", Style::default().fg(Color::White)),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  Listening on localhost:51121 for Google redirect...",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  Press Esc to cancel",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];

                let para = Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .title(" Add Account ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Cyan)),
                    );
                frame.render_widget(para, dialog);
            }
            AddAccountState::Success { account_name } => {
                let lines = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "  ✓ Authentication Successful!",
                        Style::default().fg(Color::Green).bold(),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("  Account '{}' added to rotation pool.", account_name),
                        Style::default().fg(Color::White),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  Press Enter to continue",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];

                let para = Paragraph::new(lines).block(
                    Block::default()
                        .title(" Success ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Green)),
                );
                frame.render_widget(para, dialog);
            }
            AddAccountState::Error { message } => {
                let lines = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "  ✗ Authentication Failed",
                        Style::default().fg(Color::Red).bold(),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("  {}", message),
                        Style::default().fg(Color::White),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  Press Esc to go back",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];

                let para = Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .title(" Error ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Red)),
                    );
                frame.render_widget(para, dialog);
            }
        }
    }

    fn draw_logs(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .title(" Live Request / Response Logs (Scroll: ↑/k, ↓/j, [C]lear, [Esc] Back) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let inner_height = area.height.saturating_sub(2) as usize;

        let total = self.logs.len();
        let end = total.saturating_sub(self.logs_scroll);
        let start = end.saturating_sub(inner_height);

        let visible_logs = if total == 0 {
            &[]
        } else {
            &self.logs[start..end.min(total)]
        };

        let items: Vec<ListItem> = visible_logs
            .iter()
            .map(|entry| {
                let (prefix, color) = match entry.kind {
                    LogKind::Incoming => (" IN  ", Color::Cyan),
                    LogKind::Outgoing => (" OUT ", Color::Blue),
                    LogKind::Success =>  (" OK  ", Color::Green),
                    LogKind::Warn =>     (" WARN", Color::Yellow),
                    LogKind::Error =>    (" ERR ", Color::Red),
                };

                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{} ", entry.timestamp),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        prefix,
                        Style::default().fg(Color::Black).bg(color).bold(),
                    ),
                    Span::raw(" "),
                    Span::styled(&entry.message, Style::default().fg(Color::White)),
                ]))
            })
            .collect();

        let list = List::new(items).block(block);
        frame.render_widget(list, area);
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        let keybinds = match &self.screen {
            Screen::Main => {
                "  q quit  │  ↑↓ nav  │  Enter sel  │  s start/stop  │  l logs  │  r refresh quota"
            }
            Screen::Provider(_) => {
                "  Esc back  │  ↑↓ nav  │  Enter sel  │  d delete  │  l logs  │  r refresh quota"
            }
            Screen::AddAccount(_) => "  c/y copy URL  │  o open browser  │  Esc cancel",
            Screen::Logs => "  Esc/l close logs  │  ↑↓ scroll  │  c clear logs",
        };

        let status_text = self
            .status_msg
            .as_ref()
            .filter(|(_, t)| t.elapsed() < std::time::Duration::from_secs(5))
            .map(|(msg, _)| msg.as_str())
            .unwrap_or("");

        let line = Line::from(vec![
            Span::styled(keybinds, Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(status_text, Style::default().fg(Color::Yellow).bold()),
        ]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let para = Paragraph::new(line).block(block);
        frame.render_widget(para, area);
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn reload_config(&mut self) {
        if let Ok(config) = Config::load(&self.config_path) {
            self.oauth_mgr = oauth::OAuthManager::from_config(&config, self.config_path.clone());
            self.config = config;
        }
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        if cb.set_text(text.to_string()).is_ok() {
            return Ok(());
        }
    }

    let encoded = base64_encode(text.as_bytes());
    print!("\x1b]52;c;{}\x07", encoded);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    Ok(())
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };

        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);

        if i + 1 < data.len() {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }

        if i + 2 < data.len() {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }

        i += 3;
    }
    out
}
