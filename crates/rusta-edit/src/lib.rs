//! Forgiving plain-text edit protocol for Rusta — development plan §6.3 (R4).
//!
//! `rusta-edit` owns every file mutation. Edits are plain-text Aider-style
//! SEARCH/REPLACE blocks — never JSON. The crate provides:
//!
//! * a single-pass parser that survives missing fences, stray markdown,
//!   chained dividers, spurious blank lines, and DeepSeek-style fenced filenames;
//! * the proven apply chain: exact match → whitespace-flexible → blank-line retry
//!   → `...` elision piece-matching → cross-file retry → structured
//!   failure feedback (the repair loop). No edit-distance matching — DECIDED;
//! * an undo journal written *before* every file write, and the read-before-edit
//!   ledger that guarantees the model has seen a file before mutating it.
//!
//! Build status: milestone M0 (workspace skeleton). The parser, apply chain, and
//! ledger land in M2 — DEVELOPMENT_PLAN.md §8.
