//! Dual LLM backend abstraction for Rusta — development plan §6.2 (R2).
//!
//! Exactly two backends exist, selected at runtime via `rusta.toml` or
//! `--backend`:
//!
//! * [`HttpBackend`] — an OpenAI-compatible streaming client (llama.cpp
//!   `llama-server`, Ollama, LM Studio, vLLM). Always compiled; the default.
//! * `EmbeddedBackend` — in-process llama.cpp via `llama-cpp-2`, behind the
//!   `embedded` cargo feature (milestone M1.5). Exact token counts; one
//!   dedicated inference thread serializes completions.
//!
//! The choice is invisible to every layer above: both consume the same
//! [`ChatRequest`] and emit the same [`StreamEvent`]s through enum dispatch on
//! [`Backend`] — no trait objects, no async-trait (plan §10). Token estimation
//! lives in `tokens.rs` (heuristic `ceil(chars / 3)`; exact counts when
//! embedded).

pub mod error;
pub mod http;
pub mod tokens;
pub mod types;

#[cfg(feature = "embedded")]
pub mod embedded;

#[cfg(feature = "embedded")]
pub use embedded::{EmbeddedBackend, EmbeddedConfig};
pub use error::Error;
pub use http::{HttpBackend, HttpConfig};
pub use types::{ChatRequest, FinishReason, Message, Role, StreamEvent};

use tokio::sync::mpsc;

/// Runtime-selectable backend — development plan §6.2.
///
/// Enum dispatch: a closed set, so the embedded variant (M1.5) is a
/// compile-checked extension, not a vtable.
pub enum Backend {
    /// OpenAI-compatible HTTP backend; the default.
    Http(HttpBackend),
    /// In-process llama.cpp; feature `embedded` (milestone M1.5).
    #[cfg(feature = "embedded")]
    Embedded(EmbeddedBackend),
}

/// Which backend is loaded — a discriminator for layers that must adapt to
/// backend capabilities without matching internals. Used by dispatch (§6.8):
/// the embedded backend runs one model on one inference thread, so sub-coder
/// requests serialize; the HTTP backend serves them in parallel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// OpenAI-compatible HTTP backend.
    Http,
    /// In-process llama.cpp; feature `embedded`.
    #[cfg(feature = "embedded")]
    Embedded,
}

impl Backend {
    /// Builds the HTTP backend from `config`.
    pub fn http(config: HttpConfig) -> Result<Self, Error> {
        Ok(Self::Http(HttpBackend::new(config)?))
    }

    /// Which backend this is.
    pub fn kind(&self) -> BackendKind {
        match self {
            Backend::Http(_) => BackendKind::Http,
            #[cfg(feature = "embedded")]
            Backend::Embedded(_) => BackendKind::Embedded,
        }
    }

    /// Loads the embedded GGUF backend (feature `embedded`).
    #[cfg(feature = "embedded")]
    pub fn embedded(config: EmbeddedConfig) -> Result<Self, Error> {
        Ok(Self::Embedded(EmbeddedBackend::new(config)?))
    }

    /// Asks the backend to stop the generation in flight (§6.2 cancellation).
    ///
    /// The embedded backend checks its shutdown flag between tokens; the
    /// HTTP backend has no such flag — dropping the event receiver ends its
    /// SSE pump — so this is a no-op there. Callers abort by dropping the
    /// receiver *and* calling this, which covers both backends: without it
    /// the embedded worker only notices the drop at its next token send, and
    /// §6.2's documented flag had no caller at all.
    pub fn stop(&self) {
        match self {
            Backend::Http(_) => {}
            #[cfg(feature = "embedded")]
            Backend::Embedded(backend) => backend.stop(),
        }
    }

    /// Starts a streaming completion; returns the event channel.
    pub async fn stream(&self, request: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>, Error> {
        match self {
            Backend::Http(backend) => backend.stream(request).await,
            #[cfg(feature = "embedded")]
            Backend::Embedded(backend) => backend.stream(request),
        }
    }

    /// Non-streaming completion — summaries, sub-coder wrap-ups (plan §6.2).
    pub async fn complete(&self, request: ChatRequest) -> Result<String, Error> {
        match self {
            Backend::Http(backend) => backend.complete(request).await,
            #[cfg(feature = "embedded")]
            Backend::Embedded(backend) => backend.complete(request).await,
        }
    }

    /// Token count for `text` on this backend: exact for embedded, the
    /// conservative §6.2 heuristic over HTTP.
    pub fn count_tokens(&self, text: &str) -> u64 {
        match self {
            Backend::Http(backend) => backend.count_tokens(text),
            #[cfg(feature = "embedded")]
            Backend::Embedded(backend) => backend.count_tokens(text),
        }
    }

    /// The backend's context window in tokens.
    pub fn context_window(&self) -> u64 {
        match self {
            Backend::Http(backend) => backend.context_window(),
            #[cfg(feature = "embedded")]
            Backend::Embedded(backend) => backend.context_window(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_backend_kind_is_http() {
        let backend = Backend::http(HttpConfig {
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            ..HttpConfig::default()
        })
        .expect("offline construct");
        assert_eq!(backend.kind(), BackendKind::Http);
    }
}
