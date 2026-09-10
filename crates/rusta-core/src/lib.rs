//! Core agent machinery for Rusta — development plan §6.4, §6.6, §6.10 (R5, R7).
//!
//! `rusta-core` hosts:
//!
//! * the phase-gated state machine (`Exploring → Planning → Editing → Verifying`)
//!   with a closed transition-event enum — invalid transitions are impossible by
//!   construction, and mutation tools are simply not registered in read-only states;
//! * the context manager: a sub-500-token core prompt, JIT skill-card injection,
//!   episodic history compression, and FAMA-lite loop mitigation capsules;
//! * the prompt compiler;
//! * append-only JSONL session persistence with `/resume` replay.
//!
//! Build status: session persistence (§6.10) shipped with M1; the state machine
//! and prompt compiler land in M3 — DEVELOPMENT_PLAN.md §8.

pub mod error;
pub mod session;

pub use error::Error;
pub use session::{Event, Session, Status};
