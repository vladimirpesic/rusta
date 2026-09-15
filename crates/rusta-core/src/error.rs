//! Error taxonomy for `rusta-core` — ADR §6.11.

/// Errors surfaced by the core crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Session log I/O failure.
    #[error("session I/O error: {0}")]
    Session(#[from] std::io::Error),
    /// The session log could not be parsed; the remedy is in the message.
    #[error("corrupt session log at {path}: {cause}")]
    Corrupt {
        /// The offending log file.
        path: String,
        /// What could not be parsed.
        cause: String,
    },
    /// A scaffold event fired in a phase where it is not legal. The agent
    /// loop only fires phase-legal events, so this is a loud programming
    /// guard — never model-facing.
    #[error("invalid state transition: {event} cannot fire in {from}")]
    InvalidTransition {
        /// The phase the machine was in.
        from: String,
        /// The illegal scaffold event.
        event: String,
    },
    /// A JIT skill card could not be loaded; the remedy is in the message
    /// (ADR §6.6 — cards fail loudly at load, never silently at injection).
    #[error("skill card {path}: {cause}")]
    SkillCard {
        /// The offending card file.
        path: String,
        /// What could not be parsed or validated.
        cause: String,
    },
}
