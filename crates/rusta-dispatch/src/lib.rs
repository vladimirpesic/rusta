//! Sub-coder dispatch for Rusta — development plan §6.8 (R9).
//!
//! `rusta-dispatch` spawns isolated, read-only sub-coders over Tokio. Each
//! research task runs in a fresh context (core prompt + task, own turn cap of
//! six, toolset restricted to `read`/`grep`/`glob`/`map_refresh`/`map_drill`);
//! only its ≤400-token labeled report re-enters the main context — full
//! sub-transcripts go to the session log alone. On the embedded backend (one
//! loaded model, one inference thread) requests serialize for correctness;
//! true parallelism exists only on the HTTP backend.
//!
//! Build status: milestone M0 (workspace skeleton). Actors land in M7 —
//! DEVELOPMENT_PLAN.md §8.
