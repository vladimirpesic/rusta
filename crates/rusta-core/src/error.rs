//! Error taxonomy for `rusta-core` — development plan §6.11.

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
}
