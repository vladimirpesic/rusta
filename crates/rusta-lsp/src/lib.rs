//! Opt-in LSP type enrichment for Rusta — ADR §0 rule 4, §14.
//!
//! Annotates a drilled definition with its **resolved type signature**, which
//! is the one thing tree-sitter structurally cannot supply: it parses syntax,
//! so it cannot tell you what a name resolves to. The repo map (§6.5) stays
//! the index; this answers a narrow follow-up question about a position the
//! scaffold already knows.
//!
//! # Shape of the contract
//!
//! * **The scaffold owns the coordinate.** [`rusta_repomap::definition_anchor`]
//!   derives line and column from the tree-sitter tag the drill already
//!   resolved. A model never supplies a position — §17.1 records a real run in
//!   which `qwen2.5-coder:7b` passed an identifier where a line number was
//!   specified, and LSP is a coordinate API. That edge is never exposed.
//! * **No resident cost.** No tool is registered, no prompt text changes. R7's
//!   500-token core-prompt budget is untouched because nothing is added to it.
//! * **Failure is indistinguishable from absence.** Feature off, no
//!   `rust-analyzer`, still indexing, timed out, crashed, limit reached — every
//!   one returns `None`, and `map_drill` renders byte-identically to a build
//!   without this crate. That property is pinned by tests on both sides of the
//!   feature (§16.6's rule about matrix cells).
//!
//! # Structure
//!
//! [`CodeIntel`] exists as a type in **both** feature graphs, so callers need
//! no `cfg` and the disabled path is exercised by the default CI job. Only
//! `mcpls` (compiled under the `lsp` feature) names an `mcpls_core` type, which
//! keeps a breaking upgrade of that pre-1.0 dependency to a one-file diff.

#![forbid(unsafe_code)]

use std::time::Duration;

#[cfg(feature = "lsp")]
mod mcpls;
mod signature;

pub use signature::extract as extract_signature;

/// Hard per-call deadline. Bounds the whole enrichment — acquiring the
/// language server, spawning it if needed, the round trip, and any retry the
/// LSP client performs internally — so a slow, wedged or still-starting
/// language server costs at most this much of a turn.
pub const DEFAULT_DEADLINE: Duration = Duration::from_millis(1500);

/// Documents held open before the client is recycled.
///
/// `mcpls-core`'s `DocumentTracker` returns a hard `DocumentLimitExceeded`
/// once its limit is reached — there is no LRU eviction, no `didClose` is ever
/// sent, and `Translator` exposes no way to close a document — so a session
/// that drilled enough distinct files would fail every subsequent call
/// forever. Rusta therefore keeps its own budget below the tracker's and
/// recycles the whole client before that error can be reached.
pub const DEFAULT_MAX_DOCUMENTS: usize = 64;

/// Largest file offered to the language server, in bytes.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1 << 20;

/// Tuning for [`CodeIntel`]. Defaults are the `DEFAULT_*` constants in this
/// crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Hard per-call deadline.
    pub deadline: Duration,
    /// Documents held open before recycling.
    pub max_documents: usize,
    /// Largest file offered to the language server.
    pub max_file_size: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_DEADLINE,
            max_documents: DEFAULT_MAX_DOCUMENTS,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

/// A resolved type signature, already normalised to one line and clipped to
/// the repo map's own line width. Callers render it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeInfo(pub String);

impl std::fmt::Display for TypeInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Optional LSP-backed type enrichment.
///
/// Every method is best-effort and infallible by degradation: a caller cannot
/// distinguish "disabled" from "failed", by design — both mean *render the
/// drill exactly as you would have anyway*.
#[derive(Debug)]
pub struct CodeIntel {
    #[cfg(feature = "lsp")]
    backend: Option<mcpls::Backend>,
}

impl CodeIntel {
    /// A permanently disabled instance. Always available, in both feature
    /// graphs, and always returns `None`.
    ///
    /// This is the value sub-coders (§6.8) get: their contexts are isolated
    /// and their returns summarised, and giving each parallel actor its own
    /// language server would multiply the cost of the thing this feature
    /// exists to make cheap.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            #[cfg(feature = "lsp")]
            backend: None,
        }
    }

    /// An enabled instance rooted at `root`.
    ///
    /// Starts `rust-analyzer` on a background task and awaits nothing, so
    /// startup is never blocked by a cold index but indexing still overlaps
    /// the model's first turns rather than colliding with its first drill.
    /// A session that never drills still pays one process spawn; measured
    /// against the alternative — the first several drills of every session
    /// returning nothing on a real repository — that is the cheaper side.
    #[cfg(feature = "lsp")]
    #[must_use]
    pub fn enabled(root: std::path::PathBuf, config: Config) -> Self {
        let backend = mcpls::Backend::new(root, config);
        // Start the language server now, in the background. Nothing is
        // awaited: indexing runs while the model takes its first turns, so
        // the first drill is not the one that pays for a cold crate graph.
        backend.start_warmup();
        Self {
            backend: Some(backend),
        }
    }

    /// Whether this instance can ever return a signature.
    ///
    /// Callers use it to skip resolving an anchor they would not use; it is
    /// never a promise that a given call will succeed.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        #[cfg(feature = "lsp")]
        {
            self.backend.is_some()
        }
        #[cfg(not(feature = "lsp"))]
        {
            false
        }
    }

    /// Resolved signature of the definition at (`line`, `character`) of `rel`,
    /// a repo-relative path. Positions are 1-based, with the column in UTF-16
    /// code units — exactly what [`rusta_repomap::Anchor`] produces.
    ///
    /// Returns `None` on every failure path.
    pub async fn signature(&self, rel: &str, line: u32, character: u32) -> Option<TypeInfo> {
        #[cfg(feature = "lsp")]
        {
            self.backend.as_ref()?.signature(rel, line, character).await
        }
        #[cfg(not(feature = "lsp"))]
        {
            let _ = (rel, line, character);
            None
        }
    }

    /// Shut the language server down gracefully. Idempotent; call once at
    /// session end.
    ///
    /// Worth calling rather than relying on process teardown: `mcpls-core`
    /// keeps `Translator::shutdown_servers` crate-private, so this crate holds
    /// the `LspServer` itself specifically to retain a graceful path instead
    /// of leaving `kill_on_drop` to it.
    pub async fn shutdown(&self) {
        #[cfg(feature = "lsp")]
        if let Some(backend) = self.backend.as_ref() {
            backend.shutdown().await;
        }
    }
}

impl Default for CodeIntel {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_intel_never_returns_a_signature() {
        // T-L1, and the invariant behind I6: this runs in both feature
        // graphs, so "feature on but no server" and "feature off" are pinned
        // to the same observable behaviour.
        let intel = CodeIntel::disabled();
        assert!(!intel.is_enabled());
        assert!(intel.signature("src/lib.rs", 1, 1).await.is_none());
        // Idempotent, and a no-op on a disabled instance.
        intel.shutdown().await;
        intel.shutdown().await;
    }

    #[tokio::test]
    async fn default_is_disabled() {
        assert!(!CodeIntel::default().is_enabled());
    }

    #[test]
    fn config_defaults_match_the_documented_constants() {
        let config = Config::default();
        assert_eq!(config.deadline, DEFAULT_DEADLINE);
        assert_eq!(config.max_documents, DEFAULT_MAX_DOCUMENTS);
        assert_eq!(config.max_file_size, DEFAULT_MAX_FILE_SIZE);
    }
}
