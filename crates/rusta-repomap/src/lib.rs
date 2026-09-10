//! Tree-sitter repository map for Rusta — development plan §6.5 (R6).
//!
//! `rusta-repomap` gives the model AST-level awareness of the whole repository
//! within a token budget. The pipeline (semantics ported from Aider's
//! `repomap.py`; the plan's numbers are normative):
//!
//! 1. file discovery — git-tracked sources filtered to configured languages;
//! 2. tag extraction — tree-sitter `.scm` queries yielding def/ref tags per file;
//! 3. graph construction — edge weights `mul / (|D| · n_r)` with mention and
//!    multi-case boosts;
//! 4. personalized PageRank ranking (damping 0.85, personalization `100/N`
//!    plus chat/mention boosts);
//! 5. budget-fitted rendering — 8 context lines per definition, 100-char line
//!    truncation, 100-line sampling cost estimation, middle-drop fitting;
//! 6. an in-memory `(path, mtime, size, query_version)` cache.
//!
//! Build status: milestone M0 (workspace skeleton). The pipeline lands in M4 —
//! DEVELOPMENT_PLAN.md §8.
