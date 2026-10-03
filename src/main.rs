mod config;
mod oauth;
mod codex_oauth;
mod rtk;
mod proxy;
mod state;
mod translate;
mod tui;

use std::io;
use std::path::PathBuf;

use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

#[derive(Parser)]
#[command(name = "llm-router", about = "LLM Router — multi-account Antigravity proxy")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    // Initialize tracing to file (not stdout, since we have a TUI)
    let log_file = std::fs::File::create("llm-router.log").ok();
    if let Some(file) = log_file {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(file)
            .with_ansi(false)
            .init();
    }

    let cli = Cli::parse();

    let config = config::Config::load(&cli.config)?;
    let app = tui::App::new(config, cli.config);

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Auto-start server if accounts exist
    let mut app = app;
    app.auto_start_if_ready();

    // Main loop
    let result = run_app(&mut terminal, &mut app).await;

    // Cleanup terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut tui::App,
) -> anyhow::Result<()> {
    loop {
        // Process background events
        app.poll_bg_events();

        // Draw
        terminal.draw(|frame| app.draw(frame))?;

        // Poll for keyboard events with a short timeout
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        }

        if app.should_quit() {
            break;
        }
    }

    Ok(())
}
