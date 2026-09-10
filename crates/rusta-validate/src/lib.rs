//! Auto-validation for Rusta — development plan §6.7 (R8).
//!
//! `rusta-validate` runs the project's configured validators (e.g.
//! `cargo check --workspace`, `cargo clippy --workspace -- -D warnings`,
//! `cargo test --workspace`) after each applied edit batch. Output is formatted
//! for the model — deduplicated, first and last error, ≤30 lines, clickable
//! `file:line` locations — and fed back *before* the user sees results: the
//! Reflexion repair gate, bounded at three attempts. Zero-test detection
//! ("0 tests" in output) triggers a capsule prompting real tests.
//!
//! Build status: milestone M0 (workspace skeleton). Validators land in M6 —
//! DEVELOPMENT_PLAN.md §8.
