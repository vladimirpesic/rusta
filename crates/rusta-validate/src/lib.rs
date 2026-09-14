//! Auto-validation for Rusta — development plan §6.7 (R8), the Reflexion gate.
//!
//! `rusta-validate` runs the project's configured validators (e.g.
//! `cargo check --workspace`, `cargo clippy --workspace -- -D warnings`,
//! `cargo test --workspace`) after each applied edit batch. Output is
//! formatted for the model — deduplicated, first-to-last error, ≤ 30 lines,
//! clickable `file:line` locations — and fed back *before* the user sees
//! results: the model gets first repair attempts, bounded at three
//! ([`Gate`]); then the failure is surfaced to the user. Zero-test
//! detection (`0 tests` in test output) attaches a capsule prompting real
//! tests (the SmallCTL lesson).
//!
//! The Verifying exit gate wiring is [`Verdict::event`]: green fires
//! [`rusta_core::PhaseEvent::ValidationPassed`] (Verifying → Exploring), any
//! failure fires [`rusta_core::PhaseEvent::ValidationFailed`] (Verifying →
//! Editing) — the agent loop only has to fire the returned event on the
//! phase machine.
//!
//! Validators are subprocesses and never abort the agent: a command that
//! cannot start, times out, or prints garbage becomes a failing
//! [`Report`] with an actionable message, not an error. The only
//! [`Error`] is configuration misuse, caught at load time.

pub mod validators;

pub use validators::{
    Error, Gate, Outcome, REPAIR_BOUND, Report, ValidateConfig, Verdict, ZERO_TEST_CAPSULE,
    capped_read,
};
