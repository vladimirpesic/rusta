//! Dual LLM backend abstraction for Rusta — development plan §6.2 (R2).
//!
//! `rusta-llm` owns every conversation with a language model. Exactly two backends
//! exist, selected at runtime via `rusta.toml` or `--backend`:
//!
//! * `HttpBackend` — an OpenAI-compatible streaming client (llama.cpp `llama-server`,
//!   Ollama, LM Studio, vLLM). Always compiled; the default.
//! * `EmbeddedBackend` — in-process llama.cpp via `llama-cpp-2`, behind the
//!   `embedded` cargo feature.
//!
//! The choice is invisible to every layer above: both consume the same `ChatRequest`
//! and emit the same stream of `StreamEvent`s. Token estimation lives here in
//! `tokens.rs` (heuristic `ceil(chars / 3)` for HTTP; exact counts when embedded).
//!
//! Build status: milestone M0 (workspace skeleton). The streaming implementations
//! land in M1 (HTTP) and M1.5 (embedded) — DEVELOPMENT_PLAN.md §8.
