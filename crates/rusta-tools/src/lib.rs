//! Phase-gated tool registry for Rusta — development plan §6.4 (R5).
//!
//! `rusta-tools` implements the ten canonical tools — `read`, `grep`, `glob`,
//! `map_refresh`, `map_drill`, `dispatch`, `ask`, `edit`, `write`, `shell` —
//! exactly as specified in the plan's tool reference (input keys, behavior,
//! result caps). The registry filters itself by state: `edit`/`write`/`shell`
//! handlers are not registered in read-only states, so file mutation is
//! unreachable there by construction, not by prompt. The `edit`/`write` tool
//! calls funnel into the same apply chain and read-before-edit ledger as text
//! SEARCH/REPLACE blocks — one edit mechanism, two syntaxes.
//!
//! Build status: milestone M0 (workspace skeleton). The registry lands in M7
//! alongside dispatch — DEVELOPMENT_PLAN.md §8.
