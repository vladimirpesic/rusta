//! Core agent machinery for Rusta — development plan §6.4, §6.6, §6.10 (R5, R7).
//!
//! `rusta-core` hosts:
//!
//! * the phase-gated state machine ([`state`]): `Exploring → Planning →
//!   Editing → Verifying` with a closed transition-event enum — invalid
//!   transitions are impossible by construction, and mutation tools are
//!   simply not registered in read-only states;
//! * the prompt compiler ([`prompt`]): a sub-500-token core prompt (CI
//!   invariant) plus the §6.1 observation formatter;
//! * append-only JSONL session persistence ([`session`]) with `/resume`
//!   replay that reconstructs messages, ledger, undo stack, and phase.
//!
//! Context management (JIT cards, compression, loop mitigation) is §6.6's
//! second half and lands with milestone M5 — DEVELOPMENT_PLAN.md §8.

pub mod error;
pub mod prompt;
pub mod session;
pub mod state;

pub use error::Error;
pub use prompt::{CORE_PROMPT_TOKEN_BUDGET, core_prompt, core_prompt_tokens, observation};
pub use session::{Event, Reconstructed, Session, Status};
pub use state::{
    Machine, PHASE_EVENTS, PhaseEvent, STATES, State, TOOLS, Tool, Transition, corrective_note,
};
