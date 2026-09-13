//! Forgiving plain-text edit protocol for Rusta — development plan §6.3 (R4).
//!
//! `rusta-edit` owns every file mutation. Edits are plain-text Aider-style
//! SEARCH/REPLACE blocks — never JSON. The crate provides:
//!
//! * `parser`: a single-pass parser that survives missing fences, stray
//!   markdown, chained dividers, spurious blank SEARCH lines, DeepSeek-style
//!   fenced filenames, and CRLF; it never panics — malformed input degrades
//!   to prose plus a corrective note.
//! * `apply`: the proven apply chain — exact match → whitespace-flexible →
//!   blank-line retry → `...` elision piece-matching → cross-file retry →
//!   structured failure feedback (the repair loop). No edit-distance
//!   matching: a clear retry request beats a wrong-guess apply (DECIDED).
//!   Every applied block is journaled to the undo stack *before* the file
//!   write, and the read-before-edit ledger auto-injects unread files.
//! * `ledger`: the session file-set backing filename resolution,
//!   cross-file retry, and the read-before-edit rule.
//!
//! A fenced ` ```bash/sh/shell ` block outside any edit is surfaced as a
//! suggested command ([`ParsedResponse::commands`]) — never auto-executed.

mod apply;
mod ledger;
mod parser;

pub use apply::{
    AppliedBlock, ApplyReport, Editor, FailedBlock, FailureReason, UndoEntry, UndoStack,
};
pub use ledger::Ledger;
pub use parser::{EditBlock, ParsedResponse, parse_response};
