//! The `mcpls-core` adapter — **the only module in Rusta that names an
//! `mcpls_core` type**.
//!
//! That containment is not a style preference. `mcpls-core` is pre-1.0 with a
//! stated "no deprecation shim" policy, and v0.5.0 — released one day after
//! v0.4.0 — changed `Translator::handle_*` from two bare `u32` arguments to a
//! `Position` struct, renamed accessors, and removed four `Error` variants.
//! Keeping every mention inside one file makes a breaking upgrade a one-file
//! diff instead of a workspace-wide one.
//!
//! # Why the MCP server is not used
//!
//! `mcpls_core::bridge::Translator` is public and its `handle_*` methods
//! return typed Rust structs, so the LSP capability is reachable as a plain
//! library: no MCP client, no JSON-RPC hop, no second process beyond the
//! language server itself. ADR §4 fences MCP out of Rusta and §14 parks a
//! client post-v1; neither is needed here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Once;

use mcpls_core::bridge::{Position, ResourceLimits, Translator};
use mcpls_core::config::{LspServerConfig, ToolRouter};
use mcpls_core::lsp::{LspServer, ServerInitConfig};
use tokio::sync::Mutex;

use crate::{Config, TypeInfo, signature};

/// Handshake budget for `initialize`, in seconds.
///
/// Generous where the per-request timeout is not: a cold `rust-analyzer` has
/// to load the crate graph before it answers anything, and this is paid once
/// per session rather than once per drill.
const HANDSHAKE_TIMEOUT_SECONDS: u64 = 30;

/// Per-request budget handed to the LSP client, derived from the caller's
/// deadline.
///
/// Two constraints meet here. `mcpls-core`'s 30-second default is far too
/// generous — `LspClient` retries `-32801`/`-32802` up to four attempts with
/// backoff, and its own docs put the worst case at `4 × request_timeout +
/// 3.5s`, i.e. 123.5 seconds, for an annotation that is optional by
/// construction. But a *fixed* small value is wrong in the other direction: at
/// a hard-coded 2 s against the 1.5 s default deadline, a single request could
/// not finish inside the budget at all, so the deadline always fired first and
/// the feature returned nothing. Deriving it keeps one full attempt inside
/// whatever budget the operator set, and `mcpls`'s own ceiling caps the rest.
fn request_timeout_seconds(deadline: std::time::Duration) -> u64 {
    deadline
        .as_secs()
        .clamp(1, mcpls_core::config::MAX_TIMEOUT_SECONDS)
}

/// Whether an error might answer differently if the same request is repeated.
///
/// Only these two are worth waiting out; everything else in `mcpls_core`'s
/// `Error` describes a condition the next identical request will reproduce.
/// The enum is `#[non_exhaustive]`, so an unrecognised variant is treated as
/// permanent — failing fast on something retryable costs one missing
/// annotation, while polling something permanent costs the whole deadline on
/// every call.
fn is_transient(error: &mcpls_core::Error) -> bool {
    matches!(
        error,
        mcpls_core::Error::Timeout(_) | mcpls_core::Error::ServerInitializing { .. }
    )
}

/// File extension to LSP language identifier, for the languages this crate
/// serves.
///
/// **Not optional.** `mcpls_core`'s `detect_language` has no built-in
/// defaults: it is a pure lookup in this map, and anything missing resolves to
/// `"plaintext"`, which routes to no server and fails every call with
/// `no LSP server configured for language: plaintext`. `serve()` fills the map
/// from `WorkspaceConfig::language_extensions`, a path a library embedder does
/// not take — so supplying it here is what makes library mode work at all.
/// Found by the live `rust-analyzer` test; no amount of testing against a
/// disabled client would have surfaced it.
const LANGUAGE_EXTENSIONS: &[(&str, &str)] = &[("rs", "rust")];

/// Gap between hover attempts while a freshly spawned server is still
/// indexing.
///
/// A cold `rust-analyzer` answers *immediately* with "no hover information"
/// rather than blocking or erroring, so a single attempt right after spawn
/// reliably returns nothing — measured at ~357 ms to first useful answer even
/// on a four-line fixture crate. Polling turns that into a wait bounded by
/// [`Config::deadline`] instead of a guaranteed miss on the first drill of
/// every session.
const WARMUP_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Fraction of [`Config::max_documents`] at which the client is recycled.
///
/// The tracker's limit is a hard error with no recovery (see
/// [`crate::DEFAULT_MAX_DOCUMENTS`]), so Rusta must act strictly before it,
/// never on it.
const RECYCLE_AT: usize = 9;
const RECYCLE_OF: usize = 10;

/// One live language server and the translator routing to it.
#[derive(Debug)]
struct Session {
    translator: Translator,
    server: LspServer,
    /// Set once this server has answered with a usable signature.
    ///
    /// Separates the two reasons a hover comes back empty: *still indexing*,
    /// which is worth waiting out once per session, and *nothing to report at
    /// this position*, which is not worth waiting for at all. Without it,
    /// every hover over a comment would spend the whole deadline.
    warm: bool,
}

/// Lifecycle state of the language server.
#[derive(Debug)]
enum Slot {
    /// Never spawned, or recycled and awaiting a fresh spawn.
    Cold,
    /// Spawned and routable.
    Live(Box<Session>),
    /// A spawn failed. Not retried for the rest of the process.
    ///
    /// Spawn failure is overwhelmingly persistent — `rust-analyzer` absent
    /// from `PATH`, or a handshake that timed out on a project it cannot
    /// load — and each retry costs another [`HANDSHAKE_TIMEOUT_SECONDS`].
    /// Retrying per drill would turn a missing optional feature into a
    /// per-turn stall, which is precisely the cost this feature exists to
    /// avoid.
    Unavailable,
}

/// State shared between the handle and the background warm-up task.
#[derive(Debug)]
struct Shared {
    root: PathBuf,
    config: Config,
    /// Serialises every LSP call. Enrichment is one annotation on one drill
    /// on the main agent loop — sub-coders are handed a disabled
    /// [`crate::CodeIntel`] — so there is no concurrency to preserve here,
    /// and a single lock is both the simplest correct thing and a hard cap of
    /// one in-flight request against the language server.
    slot: Mutex<Slot>,
}

/// The `lsp`-feature implementation behind [`crate::CodeIntel`].
#[derive(Debug, Clone)]
pub(crate) struct Backend {
    shared: std::sync::Arc<Shared>,
}

impl Backend {
    pub(crate) fn new(root: PathBuf, config: Config) -> Self {
        Self {
            shared: std::sync::Arc::new(Shared {
                root,
                config,
                slot: Mutex::new(Slot::Cold),
            }),
        }
    }

    /// Start the language server now, in the background, without blocking.
    ///
    /// Measured on a real workspace, this is the difference between a feature
    /// that works and one that does not. `rust-analyzer` must load the crate
    /// graph before it answers anything, and on Rusta's own tree that takes
    /// far longer than any per-call deadline an optional annotation can
    /// justify — so a server started lazily, by the first drill, guarantees
    /// that drill returns nothing, and likely the several after it too.
    /// Started here instead, indexing overlaps the model's first turns, which
    /// each take seconds of their own.
    ///
    /// Nothing is awaited and no failure propagates: if there is no Tokio
    /// runtime — a library embedder constructing outside one — the spawn is
    /// simply skipped and the server starts lazily, as before.
    pub(crate) fn start_warmup(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let backend = self.clone();
        tokio::spawn(async move {
            let mut slot = backend.shared.slot.lock().await;
            let _ = backend.ensure_live(&mut slot).await;
        });
    }

    /// Resolve the signature at a 1-based, UTF-16-column position.
    pub(crate) async fn signature(&self, rel: &str, line: u32, character: u32) -> Option<TypeInfo> {
        // `validate_path_against_roots` canonicalises whatever it is given,
        // so a relative path would resolve against the process CWD rather
        // than the repo. Always hand it an absolute path.
        let absolute = self.shared.root.join(rel);
        let path = absolute.to_str()?.to_owned();

        // One bound for the *whole* call, and it has to start here. Acquiring
        // the slot and spawning a server are both waits a drill can be made
        // to sit through: a background warm-up holds the lock across an
        // `initialize` handshake budgeted at HANDSHAKE_TIMEOUT_SECONDS, so
        // with either one outside the timeout a drill could cost 30 s against
        // a documented bound of 1.5 s. Inside it are the lock, any spawn, the
        // LSP round trip, `LspClient`'s own internal retries — whose budget is
        // mcpls's to change and must not be relied on — and warm-up polling.
        tokio::time::timeout(self.shared.config.deadline, async {
            let mut slot = self.shared.slot.lock().await;
            let session = self.ensure_live(&mut slot).await?;
            Self::resolve(session, path, Position { line, character }).await
        })
        .await
        .ok()?
    }

    /// Hover at `position`, waiting out a cold server's indexing window.
    ///
    /// Cancelled by the caller's deadline, which is the only bound: on a large
    /// repository the first drills of a session may each spend it and return
    /// nothing, which is the documented cost of `enabled = true` and is what
    /// `deadline_ms` exists to let an operator tune. Once any hover resolves,
    /// the session is warm and never polls again.
    async fn resolve(session: &mut Session, path: String, position: Position) -> Option<TypeInfo> {
        loop {
            let hover = match session
                .translator
                .handle_hover(path.clone(), position)
                .await
            {
                Ok(hover) => hover,
                // A *permanent* error answers the same however long it is
                // asked — a path outside the workspace, an unsupported
                // capability, a file too large. Polling one would burn the
                // whole deadline to reach a result already in hand.
                Err(error) if !is_transient(&error) => return None,
                // A transient one is the indexing window showing through: a
                // busy `rust-analyzer` simply does not answer, and the
                // request times out. On a real repository this is what the
                // first drills of a session hit.
                Err(_) if session.warm => return None,
                Err(_) => {
                    tokio::time::sleep(WARMUP_POLL).await;
                    continue;
                }
            };

            if let Some(text) = signature::extract(&hover.contents) {
                session.warm = true;
                return Some(TypeInfo(text));
            }

            if session.warm {
                // A warm server with nothing to say means this position has no
                // type to report. Retrying would only spend the budget.
                return None;
            }

            tokio::time::sleep(WARMUP_POLL).await;
        }
    }

    /// Shut the language server down gracefully and leave the slot cold.
    pub(crate) async fn shutdown(&self) {
        let mut slot = self.shared.slot.lock().await;
        Self::retire(&mut slot).await;
    }

    /// Return a live session, spawning or recycling as needed.
    ///
    /// `None` means no enrichment is available — the caller renders the drill
    /// unannotated, exactly as a build without this feature would.
    async fn ensure_live<'a>(&self, slot: &'a mut Slot) -> Option<&'a mut Session> {
        if matches!(slot, Slot::Unavailable) {
            return None;
        }

        if Self::should_recycle(slot, self.shared.config.max_documents) {
            Self::retire(slot).await;
        }

        if matches!(slot, Slot::Cold) {
            match self.spawn().await {
                Some(session) => *slot = Slot::Live(Box::new(session)),
                None => {
                    *slot = Slot::Unavailable;
                    warn_once_unavailable();
                    return None;
                }
            }
        }

        match slot {
            Slot::Live(session) => Some(session),
            // `Cold` is unreachable: the block above either made it `Live` or
            // returned. `Unavailable` was returned on at the top.
            Slot::Cold | Slot::Unavailable => None,
        }
    }

    /// Whether the live session must be replaced before the next request.
    ///
    /// Two independent reasons, both of which would otherwise surface as a
    /// permanent failure rather than a transient one:
    ///
    /// * the language server process has exited, leaving a client whose every
    ///   request will time out (`mcpls-core`'s own respawn path is
    ///   unreachable from a library embedder, since `register_server_config`
    ///   is crate-private);
    /// * the document budget is nearly spent, and the tracker's limit is a
    ///   hard error with no public way to close a document.
    fn should_recycle(slot: &mut Slot, max_documents: usize) -> bool {
        let Slot::Live(session) = slot else {
            return false;
        };

        // `has_exited` reaps the child, so a crashed server is detected here
        // rather than by waiting out a deadline on a dead pipe.
        if session.server.has_exited().unwrap_or(true) {
            return true;
        }

        let open = session.translator.open_document_paths().len();
        open.saturating_mul(RECYCLE_OF) >= max_documents.saturating_mul(RECYCLE_AT)
    }

    /// Shut down whatever is in `slot` and leave it cold.
    async fn retire(slot: &mut Slot) {
        if let Slot::Live(session) = std::mem::replace(slot, Slot::Cold) {
            // Errors here are not actionable: the process is being replaced
            // either way, and `kill_on_drop` is the backstop.
            drop(session.translator);
            let _ = session.server.shutdown().await;
        }
    }

    /// Spawn `rust-analyzer` and wire a translator to it.
    async fn spawn(&self) -> Option<Session> {
        let mut server_config = LspServerConfig::rust_analyzer();
        server_config.timeout_seconds = HANDSHAKE_TIMEOUT_SECONDS;
        server_config.request_timeout_seconds =
            request_timeout_seconds(self.shared.config.deadline);
        let id = server_config.id();

        let router = ToolRouter::from_configs(std::iter::once(&server_config)).ok()?;

        let init = ServerInitConfig {
            server_config,
            workspace_roots: vec![self.shared.root.clone()],
            initialization_options: None,
            // mcpls defines its own MCP positions in UTF-16 regardless of what
            // is negotiated here, and converts on the way out; this list only
            // decides what goes on the wire to the server.
            position_encodings: vec!["utf-8".to_owned(), "utf-16".to_owned()],
            // Push notifications are not used: Rusta reads diagnostics from
            // `cargo` (§6.7), which is the only source that carries clippy
            // lints and a completion signal.
            notification_tx: None,
        };

        let server = LspServer::spawn(init).await.ok()?;

        let mut translator = Translator::new()
            .with_router(router)
            .with_extensions(
                LANGUAGE_EXTENSIONS
                    .iter()
                    .map(|(ext, lang)| ((*ext).to_owned(), (*lang).to_owned()))
                    .collect::<HashMap<_, _>>(),
            )
            .with_resource_limits(ResourceLimits {
                max_documents: self.shared.config.max_documents,
                max_file_size: self.shared.config.max_file_size,
            });
        translator.set_workspace_roots(vec![self.shared.root.clone()]);

        // Register the *client* but deliberately not the server:
        // `Translator::shutdown_servers` is crate-private, so handing the
        // `LspServer` over would leave `kill_on_drop` as the only teardown.
        // Keeping it here preserves the public `LspServer::shutdown`. The
        // documented cost is that capability gating falls back to
        // "assume supported" — mcpls treats that as graceful degradation, and
        // an unsupported hover simply returns nothing, which is already this
        // crate's contract.
        translator.register_client(id, server.client().clone());

        Some(Session {
            translator,
            server,
            warm: false,
        })
    }
}

/// Tell the user once, on the first spawn failure, why no type annotations
/// are appearing.
///
/// Rusta has no logging framework, and a feature that degrades silently is a
/// feature nobody can diagnose. One line on stderr, in `main.rs`'s existing
/// `rusta:` idiom, and never again for the life of the process.
fn warn_once_unavailable() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        eprintln!(
            "rusta: LSP type enrichment unavailable — could not start `rust-analyzer`. \
             Install it and ensure it is on PATH, or set `[lsp] enabled = false` to \
             silence this. Drills continue without type annotations."
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deadline must bound the *whole* call, not just the hover.
    ///
    /// `signature` acquires the slot lock and may spawn a language server —
    /// a handshake budgeted at [`HANDSHAKE_TIMEOUT_SECONDS`] — before it ever
    /// reaches the hover. With both of those outside the timeout, a drill
    /// arriving while the background warm-up held the lock waited out the
    /// whole handshake: 30 s against a documented bound of 1.5 s, on the main
    /// agent loop. Needs no `rust-analyzer`: holding the lock reproduces the
    /// contention directly.
    #[tokio::test]
    async fn the_deadline_bounds_lock_contention_not_just_the_hover() {
        let deadline = std::time::Duration::from_millis(200);
        let backend = Backend::new(
            std::path::PathBuf::from("/nonexistent"),
            Config {
                deadline,
                ..Config::default()
            },
        );

        let held = backend.shared.slot.lock().await;

        let call = backend.signature("src/lib.rs", 1, 1);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), call).await;

        drop(held);

        match outcome {
            Ok(signature) => assert!(signature.is_none(), "a contended slot yields no annotation"),
            Err(_) => panic!(
                "signature() ignored its {deadline:?} deadline while the slot was held — \
                 the lock and the spawn handshake are outside the timeout"
            ),
        }
    }
}
