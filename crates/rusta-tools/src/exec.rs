//! Tool execution — the §6.4 phase-gated dispatch matrix.
//!
//! [`Tools::exec`] is the single entry point the agent loop calls: it maps a
//! model request `(state, name, input)` to a bounded observation, applying
//! three gates in order — unknown tool, state availability (§6.4 table, via
//! `rusta-core`), then the handler with its §6.1 caps. Successful reads
//! return a `read_credit` that this layer feeds to the read-before-edit
//! ledger (§6.3); the sub-coder entry point skips that credit deliberately.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusta_core::State;
use rusta_core::session::Status;
use rusta_core::state::{Tool, corrective_note};
use rusta_edit::Editor;
use rusta_llm::Backend;
use rusta_repomap::RepoMap;
use serde_json::Value;

use crate::ask::Responder;
use crate::shell::{Approver, ShellPolicy};

/// Observation caps — development plan §6.1, normative.
pub mod caps {
    /// `read`: maximum lines per slice.
    pub const READ_LINES: usize = 2_000;
    /// `read`: maximum bytes per slice.
    pub const READ_BYTES: usize = 64 * 1024;
    /// `grep`: maximum matches.
    pub const GREP_MATCHES: usize = 200;
    /// `glob`: maximum paths.
    pub const GLOB_ENTRIES: usize = 1_000;
    /// `shell`: maximum combined stdout+stderr bytes.
    pub const SHELL_BYTES: usize = 16 * 1024;
    /// `grep`: per-line clip (keeps one match on one line).
    pub const LINE_CHARS: usize = 200;
}

/// Outcome of one tool execution, shaped for the §6.1 observation contract
/// and the §6.10 `ToolResult` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// `Ok` or `Error` (§6.10 `ToolResult.status`).
    pub status: Status,
    /// The (already capped) observation content.
    pub content: String,
    /// Whether `content` was truncated at a §6.1 cap.
    pub truncated: bool,
    /// Repo-relative path this execution read; the main-loop caller credits
    /// it to the read-before-edit ledger (§6.3). Sub-coder executions leave
    /// it `None` (§6.8 isolation — the auto-inject still protects edits).
    pub read_credit: Option<String>,
}

impl ToolOutcome {
    /// A successful, untruncated observation.
    pub(crate) fn ok(content: impl Into<String>) -> Self {
        Self {
            status: Status::Ok,
            content: content.into(),
            truncated: false,
            read_credit: None,
        }
    }

    /// A failed observation; `content` must carry an actionable remedy.
    pub(crate) fn error(content: impl Into<String>) -> Self {
        Self {
            status: Status::Error,
            content: content.into(),
            truncated: false,
            read_credit: None,
        }
    }

    /// The §6.1 observation message for this outcome.
    pub fn observation(&self, name: &str) -> rusta_llm::Message {
        rusta_core::prompt::observation(name, self.status, &self.content)
    }
}

/// Poison-tolerant mutex lock: a panicked holder must not wedge the agent.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Repo-relative path fence shared by every path-taking tool: no absolute
/// paths, no `..`, `.` components dropped. Mirrors the shell policy's
/// repo-root confinement (§6.12) for the file tools.
pub(crate) fn safe_rel(raw: &str) -> Result<PathBuf, ToolOutcome> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ToolOutcome::error(
            "\"path\" must be a non-empty, repo-relative path",
        ));
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(ToolOutcome::error(format!(
            "{trimmed}: absolute paths are not allowed; use a repo-relative path"
        )));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ToolOutcome::error(format!(
            "{trimmed}: \"..\" is not allowed; use a repo-relative path"
        )));
    }
    Ok(path
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect())
}

/// Required string key (may be empty — e.g. `search`/`replace`).
pub(crate) fn req_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolOutcome> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolOutcome::error(format!("missing required key \"{key}\" (a string)")))
}

/// Required non-empty string key.
pub(crate) fn req_nonempty<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolOutcome> {
    req_str(input, key).and_then(|value| {
        if value.trim().is_empty() {
            Err(ToolOutcome::error(format!(
                "\"{key}\" must be a non-empty string"
            )))
        } else {
            Ok(value)
        }
    })
}

/// Optional positive-integer key.
pub(crate) fn opt_usize(input: &Value, key: &str) -> Result<Option<usize>, ToolOutcome> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| ToolOutcome::error(format!("\"{key}\" must be a positive integer")))
            .map(Some),
    }
}

/// Char-safe horizontal clip for one output line.
pub(crate) fn clip_chars(line: &str, max_chars: usize) -> &str {
    if line.chars().count() <= max_chars {
        return line;
    }
    let mut end = line
        .char_indices()
        .nth(max_chars)
        .map_or(line.len(), |(i, _)| i);
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// Byte-safe clip with an explicit marker (§6.1 caps).
pub(crate) fn clip_bytes(text: &str, max_bytes: usize, marker: &str) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{}", &text[..end], marker), true)
}

/// The session tool registry: one instance per agent session, shareable
/// (`Arc<Tools>`) because all mutation lives behind interior mutability.
pub struct Tools {
    root: PathBuf,
    repomap: Arc<Mutex<RepoMap>>,
    editor: Arc<Mutex<Editor>>,
    backend: Arc<Backend>,
    shell: ShellPolicy,
    approver: Mutex<Box<dyn Approver>>,
    responder: Mutex<Box<dyn Responder>>,
}

impl Tools {
    /// Registry for the repo at `root` against `backend`, with the given
    /// §6.12 shell policy. Approval defaults to [`crate::DenyAll`] (the
    /// non-interactive default) and `ask` to [`crate::Headless`]; the CLI
    /// installs interactive implementations.
    pub fn new(
        root: impl Into<PathBuf>,
        backend: Arc<Backend>,
        shell: ShellPolicy,
    ) -> Result<Self, crate::Error> {
        let root = root.into();
        Ok(Self {
            repomap: Arc::new(Mutex::new(RepoMap::new(root.clone()))),
            editor: Arc::new(Mutex::new(Editor::new(root.clone()))),
            root,
            backend,
            shell,
            approver: Mutex::new(Box::new(crate::shell::DenyAll)),
            responder: Mutex::new(Box::new(crate::ask::Headless)),
        })
    }

    /// Replace the shell approver (M8: the y/n/always prompt).
    #[must_use]
    pub fn with_approver(mut self, approver: Box<dyn Approver>) -> Self {
        self.approver = Mutex::new(approver);
        self
    }

    /// Replace the `ask` responder (M8: the terminal prompt).
    #[must_use]
    pub fn with_responder(mut self, responder: Box<dyn Responder>) -> Self {
        self.responder = Mutex::new(responder);
        self
    }

    /// The workspace root every relative path resolves against.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The shared backend (dispatch schedules against its kind, §6.8).
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub(crate) fn backend_arc(&self) -> &Arc<Backend> {
        &self.backend
    }

    /// The session editor: ledger, apply chain, undo journal (§6.3).
    pub fn editor(&self) -> MutexGuard<'_, Editor> {
        lock(&self.editor)
    }

    /// The repo map (§6.5).
    pub fn repomap(&self) -> MutexGuard<'_, RepoMap> {
        lock(&self.repomap)
    }

    pub(crate) fn repomap_arc(&self) -> &Arc<Mutex<RepoMap>> {
        &self.repomap
    }

    /// Execute one model tool request in `state`.
    ///
    /// Ordering of the gates is normative (§6.4): unknown tool → state
    /// availability (the corrective note) → handler. Mutation handlers are
    /// unreachable in read-only states because `available_in` consults the
    /// same closed table the core prompt is generated from.
    pub async fn exec(&self, state: State, name: &str, input: &Value) -> ToolOutcome {
        let Some(tool) = Tool::parse(name) else {
            let registered: Vec<&str> = state.tools().iter().map(|t| t.as_str()).collect();
            return ToolOutcome::error(format!(
                "unknown tool \"{name}\". Registered tools in {state}: {}.",
                registered.join(", ")
            ));
        };
        if !tool.available_in(state) {
            return ToolOutcome::error(corrective_note(state, tool));
        }
        let outcome = match tool {
            Tool::Read => crate::read::read(&self.root, input),
            Tool::Grep => crate::search::grep(&self.root, input),
            Tool::Glob => crate::glob::glob(&self.root, input),
            Tool::MapRefresh => {
                let chat_files: Vec<String> = self
                    .editor()
                    .ledger()
                    .read_set()
                    .map(|path| path.display().to_string())
                    .collect();
                let mut map = self.repomap();
                crate::map::refresh(&mut map, &chat_files)
            }
            Tool::MapDrill => crate::map::drill(&self.root, input),
            Tool::Dispatch => crate::dispatch::dispatch(self, input).await,
            Tool::Ask => crate::ask::ask(&self.responder, input),
            Tool::Edit => crate::edit::edit(&self.editor, input),
            Tool::Write => crate::edit::write(&self.editor, input),
            Tool::Shell => crate::shell::run(&self.shell, &self.approver, &self.root, input).await,
        };
        // §6.3: main-loop reads credit the read-before-edit ledger. (The
        // sub-coder entry point in `dispatch.rs` bypasses `exec`, so its
        // reads never reach this credit.)
        if outcome.status == Status::Ok {
            if let Some(rel) = &outcome.read_credit {
                self.editor().record_read(rel);
            }
        }
        outcome
    }
}
