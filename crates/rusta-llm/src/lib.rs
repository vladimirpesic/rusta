//! Dual LLM backend abstraction for Rusta — development plan §6.2 (R2).
//!
//! Exactly two backends exist, selected at runtime via `rusta.toml` or
//! `--backend`:
//!
//! * [`HttpBackend`] — an OpenAI-compatible streaming client (llama.cpp
//!   `llama-server`, Ollama, LM Studio, vLLM). Always compiled; the default.
//! * `EmbeddedBackend` — in-process llama.cpp via `llama-cpp-2`, behind the
//!   `embedded` cargo feature (milestone M1.5).
//!
//! The choice is invisible to every layer above: both consume the same
//! [`ChatRequest`] and emit the same [`StreamEvent`]s through enum dispatch on
//! [`Backend`] — no trait objects, no async-trait (plan §10). Token estimation
//! lives in `tokens.rs` (heuristic `ceil(chars / 3)`; exact counts when embedded).

pub mod error;
pub mod http;
pub mod tokens;
pub mod types;

pub use error::Error;
pub use http::{HttpBackend, HttpConfig};
pub use types::{ChatRequest, FinishReason, Message, Role, StreamEvent};

use tokio::sync::mpsc;

/// Runtime-selectable backend — development plan §6.2.
///
/// Enum dispatch: a closed set, so adding the embedded variant in M1.5 is a
/// compile-checked extension, not a vtable.
pub enum Backend {
    /// OpenAI-compatible HTTP backend; the default.
    Http(HttpBackend),
    // `Embedded` arrives in milestone M1.5 behind the `embedded` feature.
}

impl Backend {
    /// Builds the HTTP backend from `config`.
    pub fn http(config: HttpConfig) -> Result<Self, Error> {
        Ok(Self::Http(HttpBackend::new(config)?))
    }

    /// Starts a streaming completion; returns the event channel.
    pub async fn stream(&self, request: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>, Error> {
        match self {
            Backend::Http(backend) => backend.stream(request).await,
        }
    }

    /// Non-streaming completion — summaries, sub-coder wrap-ups (plan §6.2).
    pub async fn complete(&self, request: ChatRequest) -> Result<String, Error> {
        match self {
            Backend::Http(backend) => backend.complete(request).await,
        }
    }

    /// Token count for `text` on this backend.
    pub fn count_tokens(&self, text: &str) -> u64 {
        match self {
            Backend::Http(backend) => backend.count_tokens(text),
        }
    }

    /// The backend's context window in tokens.
    pub fn context_window(&self) -> u64 {
        match self {
            Backend::Http(backend) => backend.context_window(),
        }
    }
}
