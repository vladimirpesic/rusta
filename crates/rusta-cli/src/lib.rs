//! `rusta` — the CLI host: development plan §6.9 (R3), milestone M8.
//!
//! The library half (`rusta_cli`) carries the whole session machinery so the
//! e2e acceptance tests can drive scripted sessions without a TTY
//! ([`App::handle_line`] is the REPL's per-line entry point); `main.rs` is a
//! thin `clap` shim.
//!
//! Layout (plan §5): `config.rs` (§7 `rusta.toml` discovery + schema),
//! `git.rs` (§6.9 auto-commit/undo), `agent.rs` (§6.1 turn lifecycle),
//! `commands.rs` (§6.9 slash commands), `repl.rs` (the [`App`] host +
//! reedline loop), `render.rs` (output surface).

pub mod agent;
pub mod commands;
pub mod config;
pub mod git;
pub mod render;
pub mod repl;

use clap::Parser;

pub use agent::{Item, Parsed, parse_items};
pub use config::Config;
pub use repl::{App, Control, Mode};

/// Rusta: a lean AI coding-agent harness for small, locally hosted LLMs.
#[derive(Debug, Parser)]
#[command(
    name = "rusta",
    version,
    about = "Lean AI coding-agent harness for small, locally hosted LLMs (8B-35B)",
    long_about = None
)]
pub struct Cli {
    /// One-shot: run this prompt and exit (§6.9 non-interactive; shell denied).
    #[arg(short = 'c', long = "command")]
    pub prompt: Option<String>,
    /// Explicit `rusta.toml` path (default: discover cwd → parents → ~/.rusta).
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,
    /// Session log path override (default: `~/.rusta/sessions/<slug>-<date>.jsonl`).
    #[arg(long)]
    pub session: Option<std::path::PathBuf>,
    /// Backend kind: "http" or "embedded" (§6.2).
    #[arg(long)]
    pub backend: Option<String>,
    /// Model name override (the §7 `[model]` name).
    #[arg(long)]
    pub model: Option<String>,
    /// HTTP base URL override, incl. /v1 (§6.2).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Start with auto-approve on (plan gate + shell, §6.12).
    #[arg(long)]
    pub auto: bool,
}

/// Parses arguments, wires config → backend → tools → session, and runs
/// either the REPL or the one-shot `-c` mode.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config_path = cli.config.clone().or_else(|| config::discover(&cwd));
    let config = match &config_path {
        Some(path) => {
            Config::load(path).map_err(|e| anyhow::anyhow!("loading {}: {e}", path.display()))?
        }
        None => Config::default(),
    };
    let overrides = config::Overrides {
        backend_kind: cli.backend.clone(),
        model: cli.model.clone(),
        base_url: cli.base_url.clone(),
    };

    // §6.12: the workspace root is the git toplevel when inside a repo.
    let git = git::Git::open(&cwd);
    let root = git.toplevel();

    let session_path = cli
        .session
        .clone()
        .or_else(|| config::session_path(&root))
        .unwrap_or_else(|| root.join(".rusta-session.jsonl"));
    if let Some(parent) = session_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mode = if cli.prompt.is_some() {
        Mode::Oneshot
    } else {
        Mode::Repl
    };
    let mut app = App::new(
        config,
        &overrides,
        root,
        session_path,
        render::Reporter::stdout(),
        mode,
    )
    .map_err(anyhow::Error::msg)?;
    if cli.auto {
        app.set_auto(true);
    }
    match &cli.prompt {
        Some(prompt) => app.run_once(prompt).await,
        None => app.run_repl().await,
    }
    Ok(())
}
