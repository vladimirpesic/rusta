//! Phase-gated tool registry for Rusta — development plan §6.4 (R5).
//!
//! `rusta-tools` implements the ten canonical tools — `read`, `grep`, `glob`,
//! `map_refresh`, `map_drill`, `dispatch`, `ask`, `edit`, `write`, `shell` —
//! exactly as specified in the plan's tool reference (input keys, behavior,
//! §6.1 result caps). The registry filters itself by state: `edit`/`write`
//! handlers are unreachable in read-only states and `dispatch` is
//! unreachable in `Verifying` — by construction via the §6.4 state table,
//! not by prompt. The `edit`/`write` tools funnel into the same apply chain
//! and read-before-edit ledger as text SEARCH/REPLACE blocks — one edit
//! mechanism, two syntaxes.
//!
//! Host interaction is injected, not implemented here: shell approval
//! ([`Approver`]) and user replies ([`Responder`]) are traits the CLI
//! (M8) implements; this crate ships the headless defaults (`/auto`
//! approves everything, non-interactive runs deny shell and answer `ask`
//! with a fixed notice).
//!
//! Sub-coder execution (§6.8) reuses the same handler functions through
//! [`rusta_dispatch::RunTool`]; those reads deliberately do not credit the
//! main ledger (isolation — the auto-inject still protects edits).

mod ask;
mod dispatch;
mod edit;
mod exec;
mod glob;
mod map;
mod read;
mod search;
mod shell;

pub use ask::{Headless, Responder};
pub use exec::{ToolOutcome, Tools};
pub use shell::{Approver, AutoApprove, DEFAULT_DENY, Decision, DenyAll, ShellPolicy, Verdict};

/// §6.11 error taxonomy. Tool *misuse* never appears here — it surfaces as
/// error observations with remedies; this enum is host-side wiring failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A configured `[shell].deny` regex does not compile.
    #[error("shell policy: invalid deny regex: {0}")]
    InvalidDenyRegex(String),
}
