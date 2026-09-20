//! Core agent machinery for Rusta — ADR §6.4, §6.6, §6.10 (R5, R7).
//!
//! `rusta-core` hosts:
//!
//! * the phase-gated state machine ([`state`]): `Exploring → Planning →
//!   Editing → Verifying` with a closed transition-event enum — invalid
//!   transitions are impossible by construction, and mutation tools are
//!   simply not registered in read-only states;
//! * the prompt compiler ([`prompt`]): a sub-500-token core prompt (CI
//!   invariant) plus the §6.1 observation formatter;
//! * the context manager ([`context`]): JIT skill cards, episodic history
//!   compression, and FAMA-lite loop mitigation (§6.6) — everything the
//!   model might need that must *not* be resident in the core prompt;
//! * append-only JSONL session persistence ([`session`]) with `/resume`
//!   replay that reconstructs messages, ledger, undo stack, and phase.

pub mod context;
pub mod error;
pub mod proc;
pub mod prompt;
pub mod session;
pub mod state;
pub mod stream_guard;

pub use context::{
    CARD_TOKEN_BUDGET, Capsule, CardDeck, CardKind, Compression, Compressor, Escalation, LoopGuard,
    MAX_INJECTED_CARDS, MIN_KEPT_OBSERVATION, SkillCard, SummaryPlan, Trip, capsule, error_cues,
};
pub use error::Error;
pub use prompt::{CORE_PROMPT_TOKEN_BUDGET, core_prompt, core_prompt_tokens, observation};
pub use session::{Event, Reconstructed, Session, Status, TranscriptLine};
pub use state::{
    Machine, PHASE_EVENTS, PhaseEvent, STATES, State, TOOLS, Tool, Transition, corrective_note,
};
pub use stream_guard::StreamGuard;
