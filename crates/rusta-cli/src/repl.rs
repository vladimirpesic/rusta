//! The REPL host — ADR §6.9 (R3).
//!
//! [`App`] owns one session's live state: the phase machine, the tool
//! registry, the context-manager pieces (§6.6), the validation gate (§6.7),
//! the git side (§6.9), and the append-only session log (§6.10). The reedline
//! loop feeds every line to [`App::handle_line`]; slash commands (§6.9) go to
//! `commands.rs`, everything else to the agent (`agent.rs`). Tests drive
//! `handle_line` directly — no TTY anywhere in the logic.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use reedline::{DefaultPrompt, FileBackedHistory, Reedline, Signal};
use rusta_core::session::Reconstructed;
use rusta_core::{CardDeck, Compressor, Event, LoopGuard, Machine};
use rusta_llm::Backend;
use rusta_llm::Message;
use rusta_lsp::CodeIntel;
#[cfg(feature = "lsp")]
use rusta_lsp::Config as LspConfig;
use rusta_repomap::RepoMap;
use rusta_tools::ShellPolicy;
use rusta_tools::Tools;
use rusta_validate::Gate;

use crate::agent::{AutoGate, PlanGate, TerminalApprover, TerminalGate, TerminalResponder};
use crate::config::{Config, Overrides, home_dir};
use crate::git::Git;
use crate::render::Reporter;

/// How the session is hosted — decides which interactive hooks are installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The reedline REPL: terminal plan gate, shell approval, and `ask`.
    Repl,
    /// `rusta -c "prompt"` (§6.9): plan auto-approved, shell denied by
    /// default (§6.12), `ask` answered headless.
    Oneshot,
}

/// One applied edit batch — the `/undo` unit (§6.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Undo-journal entries pushed by the batch (one per applied edit).
    pub entries: usize,
    /// Deduplicated repo-relative paths the batch touched.
    pub paths: Vec<String>,
    /// The `rusta: …` commit sha, when git committed the batch.
    pub sha: Option<String>,
}

/// A live session — everything one `rusta` process hosts.
pub struct App {
    /// Effective configuration (§7).
    pub config: Config,
    /// The workspace root (`git toplevel`, else cwd).
    pub root: PathBuf,
    /// The git side (§6.9); absent git degrades to journal-only undo.
    pub git: Git,
    /// The append-only session log + sidecar (§6.10).
    pub session: rusta_core::Session,
    /// The phase machine (§6.4).
    pub machine: Machine,
    /// The phase-gated tool registry (§6.4) and its shared backend.
    pub tools: Arc<Tools>,
    /// The model-visible history (observations included).
    pub history: Vec<Message>,
    /// Loop mitigation (§6.6).
    pub guard: LoopGuard,
    /// The Reflexion repair gate (§6.7).
    pub gate: Gate,
    /// History compression (§6.6).
    pub compressor: Compressor,
    /// JIT skill cards (§6.6): the embedded starter deck plus any
    /// project cards under `<root>/.rusta/skills/`.
    pub deck: CardDeck,
    /// Applied edit batches, oldest first — the `/undo` stack.
    pub batches: Vec<Batch>,
    /// Where output goes.
    pub reporter: Reporter,
    /// The §6.4 plan gate.
    pub plan_gate: Box<dyn PlanGate>,
    /// The shared `/auto` flag (plan gate + shell approval, §6.12).
    pub auto: Arc<AtomicBool>,
    /// Skill-card cues from the last turn (§6.6 JIT selection).
    pub card_cues: Vec<String>,
    /// The most recent user request, for §6.5 mention-steered rendering.
    pub last_request: String,
    /// Whether a session-log write has already failed. Journaling is
    /// best-effort by design — a failed append must never abort a turn — but
    /// it was also *silent*, so a full disk stopped the audit trail with no
    /// sign until `/resume` came back short. Reported once per session.
    pub journal_broken: bool,
}

/// Builds the runtime backend from config + CLI overrides (§6.2: config or
/// `--backend`, invisible to every layer above).
pub fn build_backend(config: &Config, overrides: &Overrides) -> Result<Backend, String> {
    match config.backend_kind(overrides).as_str() {
        "http" => Backend::http(config.http(overrides)).map_err(|e| e.to_string()),
        "embedded" => {
            #[cfg(feature = "embedded")]
            {
                let section = &config.backend.embedded;
                let path = section
                    .model_path
                    .clone()
                    .unwrap_or_else(|| "~/models/model.gguf".to_owned());
                let mut embedded = rusta_llm::EmbeddedConfig::new(expand_tilde(&path));
                embedded.ctx_size = (section.ctx_size != 0).then_some(section.ctx_size);
                embedded.gpu_layers = section.gpu_layers;
                Backend::embedded(embedded).map_err(|e| e.to_string())
            }
            #[cfg(not(feature = "embedded"))]
            {
                let _ = &config.backend.embedded;
                Err(
                    "backend kind \"embedded\" requires a rusta-full build (cargo build \
                     --features embedded)"
                        .to_owned(),
                )
            }
        }
        other => Err(format!(
            "unknown backend kind {other:?} — use \"http\" or \"embedded\""
        )),
    }
}

/// Expands a leading `~/` against the home directory (§7 config paths).
#[cfg_attr(not(feature = "embedded"), allow(dead_code))]
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    home_dir()
        .map(|home| home.join(rest))
        .unwrap_or_else(|| PathBuf::from(path))
}

/// A one-line description of the backend for the `SessionStart` event
/// (§6.10) — kind and endpoint, never secrets.
fn backend_description(config: &Config, overrides: &Overrides) -> String {
    match config.backend_kind(overrides).as_str() {
        "http" => format!("http {}", config.http(overrides).base_url),
        _ => {
            let path = config
                .backend
                .embedded
                .model_path
                .clone()
                .unwrap_or_else(|| "(unset)".to_owned());
            format!("embedded {path}")
        }
    }
}

impl App {
    /// Assembles a live session. An existing session file is resumed
    /// automatically (history, ledger, undo journal, phase — §6.10).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        overrides: &Overrides,
        root: PathBuf,
        session_path: PathBuf,
        mut reporter: Reporter,
        mode: Mode,
    ) -> Result<Self, String> {
        let backend = build_backend(&config, overrides)?;
        let auto = Arc::new(AtomicBool::new(config.agent.auto_approve));

        let shell = ShellPolicy::new(
            config.shell.timeout_secs,
            &config.shell.allow,
            &config.shell.deny,
            &config.shell.env,
        )
        .map_err(|e| format!("rusta.toml [shell]: {e}"))?;
        let mut tools = Tools::new(root.clone(), Arc::new(backend), shell)
            .map_err(|e| e.to_string())?
            .with_repomap(
                RepoMap::new(root.clone()).with_budget(config.repomap.max_tokens as usize),
            )
            .with_code_intel(code_intel(&config, &root, &mut reporter));
        let plan_gate: Box<dyn PlanGate> = match mode {
            Mode::Repl => {
                tools = tools
                    .with_approver(Box::new(TerminalApprover {
                        auto: Arc::clone(&auto),
                        always: AtomicBool::new(false),
                    }))
                    .with_responder(Box::new(TerminalResponder));
                Box::new(TerminalGate {
                    auto: Arc::clone(&auto),
                })
            }
            // §6.12: DenyAll shell + Headless `ask` are the Tools defaults.
            Mode::Oneshot => Box::new(AutoGate),
        };

        let session = rusta_core::Session::open(&session_path).map_err(|e| e.to_string())?;
        let fresh = session.events().is_empty();
        // The starter deck ships with the binary; `<root>/.rusta/skills/*.md`
        // extends it per project (§6.6).
        let deck = CardDeck::load_for_repo(&root).map_err(|e| e.to_string())?;
        let window = tools.backend().context_window();
        let git = Git::open(root.clone());

        let mut app = Self {
            config,
            root,
            git,
            session,
            machine: Machine::new(),
            tools: Arc::new(tools),
            history: Vec::new(),
            guard: LoopGuard::new(),
            gate: Gate::new(),
            compressor: Compressor::new(window),
            deck,
            batches: Vec::new(),
            reporter,
            plan_gate,
            auto,
            card_cues: Vec::new(),
            last_request: String::new(),
            journal_broken: false,
        };
        if fresh {
            let summary = app.config.summary(overrides);
            let backend_line = backend_description(&app.config, overrides);
            app.journal(Event::SessionStart {
                config: summary,
                backend: backend_line,
            });
        } else {
            let rc = app.session.replay_context().map_err(|e| e.to_string())?;
            app.resume_reconstructed(rc)?;
        }
        Ok(app)
    }

    /// Replaces the plan gate — how tests script §6.4 approvals.
    #[must_use]
    pub fn with_plan_gate(mut self, gate: Box<dyn PlanGate>) -> Self {
        self.plan_gate = gate;
        self
    }

    /// Adopts a replayed session: history, ledger, undo journal (installed
    /// into the live editor), the final phase, and the batch stack rebuilt
    /// from `EditApplied`/`Commit` events so `/undo` keeps working (§6.10).
    pub(crate) fn resume_reconstructed(&mut self, rc: Reconstructed) -> Result<(), String> {
        let Reconstructed {
            messages,
            ledger,
            undo,
            state,
            skipped_edits,
        } = rc;
        // §6.12: edits naming paths outside the workspace are dropped from
        // the journal rather than restored by `/undo`. Say so — silently
        // shorter undo depth is worse than a line of output.
        if !skipped_edits.is_empty() {
            self.reporter.line(&format!(
                "note: {} journaled edit(s) name paths outside this repo and were \
                 skipped — /undo will not restore them: {}",
                skipped_edits.len(),
                skipped_edits.join(", ")
            ));
        }
        {
            let mut editor = self.tools.editor();
            for path in ledger.read_set() {
                editor.record_read(&path.display().to_string());
            }
            editor.install_undo(undo.pending().to_vec());
        }
        self.history = messages;
        self.machine = Machine::resume_at(state);
        self.batches = rebuild_batches(self.session.events());
        Ok(())
    }

    /// `/resume <file>` (§6.10): swap the session log for `path` and adopt
    /// its replayed state.
    pub(crate) fn resume_from(&mut self, path: &Path) -> Result<(), String> {
        let session = rusta_core::Session::open(path).map_err(|e| e.to_string())?;
        let rc = session.replay_context().map_err(|e| e.to_string())?;
        self.session = session;
        self.resume_reconstructed(rc)
    }

    /// Flips `/auto` (plan gate + shell approval, §6.12).
    pub(crate) fn set_auto(&mut self, on: bool) {
        self.auto.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether `/auto` is currently on.
    pub(crate) fn auto(&self) -> bool {
        self.auto.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Appends one event to the session log, reporting the first failure.
    ///
    /// Journaling never aborts a turn: a broken log costs the audit trail and
    /// `/resume` fidelity, not the user's work in progress. But it must not
    /// be invisible, which is what discarding every `Result` made it.
    pub(crate) fn journal(&mut self, event: rusta_core::Event) {
        if let Err(err) = self.session.record(event) {
            self.report_journal_failure(&err);
        }
    }

    /// Reports the first session-log write failure of the run, once.
    pub(crate) fn report_journal_failure(&mut self, err: &rusta_core::Error) {
        if self.journal_broken {
            return;
        }
        self.journal_broken = true;
        self.reporter.line(&format!(
            "! session log write failed: {err}. The session continues, but this run will \
             not fully /resume — and /undo depth may be short. Check disk space and \
             permissions on {}.",
            self.session.path().display()
        ));
    }

    /// Records `SessionEnd` — the clean close of the log (§6.10).
    pub(crate) async fn end_session(&mut self) {
        // Before journaling: `mcpls-core` keeps its own server shutdown
        // crate-private, so `rusta-lsp` holds the `LspServer` itself to keep
        // a graceful path. A no-op when enrichment was never enabled or
        // never spawned.
        self.tools.code_intel().shutdown().await;
        self.journal(Event::SessionEnd);
    }

    /// Dispatches one REPL line: a `/command` (§6.9) or a user request.
    pub async fn handle_line(&mut self, line: &str) -> Control {
        let line = line.trim();
        if line.is_empty() {
            return Control::Continue;
        }
        if let Some(command) = line.strip_prefix('/') {
            return crate::commands::run(self, command).await;
        }
        self.submit(line).await;
        Control::Continue
    }

    /// The `-c` mode (§6.9): one agent request, then the process exits.
    pub async fn run_once(&mut self, prompt: &str) {
        self.submit(prompt).await;
        self.reporter
            .line(&format!("session: {}", self.session.path().display()));
        self.end_session().await;
    }

    /// The interactive reedline loop (§6.9).
    pub async fn run_repl(&mut self) {
        let history = home_dir()
            .map(|home| home.join(".rusta").join("history.txt"))
            .and_then(|path| FileBackedHistory::with_file(1_000, path).map(Box::new).ok());
        let mut editor = match history {
            Some(history) => Reedline::create().with_history(history),
            None => Reedline::create(),
        };
        let prompt = DefaultPrompt::default();
        self.reporter
            .line("rusta — /help lists commands, /exit quits");
        // Ctrl-C at an idle prompt cancels; only a second, consecutive press
        // exits (§6.1 step 5 "preserves session state", and the Aider-style
        // convention R3 cites). One press used to end the session outright,
        // which is a reflex users carry over from cancelling a running turn.
        let mut interrupt_armed = false;
        loop {
            let signal = tokio::task::block_in_place(|| editor.read_line(&prompt));
            match signal {
                Ok(Signal::Success(buffer)) => {
                    interrupt_armed = false;
                    if self.handle_line(&buffer).await == Control::Exit {
                        break;
                    }
                }
                Ok(Signal::CtrlC) => {
                    if interrupt_armed {
                        self.reporter.line("(exit)");
                        break;
                    }
                    interrupt_armed = true;
                    self.reporter
                        .line("(^C — press Ctrl-C again to exit, or /exit)");
                }
                Ok(Signal::CtrlD) => {
                    self.reporter.line("(exit)");
                    break;
                }
                _ => continue,
            }
        }
        self.end_session().await;
    }
}

/// Build the `map_drill` type-enrichment handle from `[lsp]`.
///
/// Three outcomes, and the middle one is the reason this is a function rather
/// than an expression: a `rusta.toml` shared between a plain build and an
/// `lsp` build must stay portable, so asking for a feature this binary does
/// not have is a warning, never an error — but it is also never silent, since
/// a request that quietly does nothing is worse than one that fails loudly.
fn code_intel(config: &Config, root: &Path, reporter: &mut Reporter) -> CodeIntel {
    if !config.lsp.enabled {
        return CodeIntel::disabled();
    }
    #[cfg(feature = "lsp")]
    {
        // Nothing to report: the enabled path either works or degrades
        // silently to no annotation, which is its documented contract.
        let _ = reporter;
        CodeIntel::enabled(
            root.to_path_buf(),
            LspConfig {
                deadline: std::time::Duration::from_millis(config.lsp.deadline_ms),
                max_documents: config.lsp.max_documents as usize,
                ..LspConfig::default()
            },
        )
    }
    #[cfg(not(feature = "lsp"))]
    {
        let _ = root;
        reporter.line(
            "note: rusta.toml [lsp] enabled = true, but this build has no `lsp` feature — \
             drills will carry no type annotations. Rebuild with `--features lsp`, or set \
             `enabled = false` to silence this.",
        );
        CodeIntel::disabled()
    }
}

/// What `handle_line` wants the REPL loop to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Keep reading lines.
    Continue,
    /// `/exit` — leave the loop and record `SessionEnd`.
    Exit,
}

/// Rebuilds the `/undo` batch stack from replayed events: each `Commit`
/// closes the batch formed by the `EditApplied` events since the previous
/// commit; trailing edits without a commit form a final journal-only batch.
fn rebuild_batches(events: &[Event]) -> Vec<Batch> {
    let mut batches: Vec<Batch> = Vec::new();
    let mut open = false;
    for event in events {
        match event {
            // A request boundary closes the current batch. Inferring
            // boundaries from `Commit` merged consecutive batches whenever no
            // commit landed — no-git mode, or a failed commit — so one
            // `/undo` popped several requests' work.
            Event::BatchBoundary => open = false,
            Event::EditApplied { path, .. } => {
                // The same fence replay applies. Replay drops entries whose
                // path escapes the workspace; counting them here made a batch
                // claim more entries than the journal held, and `/undo` then
                // walked past the batch boundary into the previous one.
                if rusta_edit::confine(Path::new(path)).is_none() {
                    continue;
                }
                if !open {
                    batches.push(Batch {
                        entries: 0,
                        paths: Vec::new(),
                        sha: None,
                    });
                    open = true;
                }
                let batch = batches.last_mut().expect("non-empty by construction");
                batch.entries += 1;
                if !batch.paths.contains(path) {
                    batch.paths.push(path.clone());
                }
            }
            Event::Commit { sha, .. } => {
                if let Some(batch) = batches.last_mut() {
                    batch.sha = Some(sha.clone());
                }
                open = false;
            }
            // `/undo`'s tombstone: the batches it consumed are gone.
            Event::UndoApplied { .. } => {
                batches.pop();
                open = false;
            }
            _ => {}
        }
    }
    batches
}
