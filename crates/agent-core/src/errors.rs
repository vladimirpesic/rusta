use lm_bridge::errors::LlmError;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),

    #[error("Unknown tool: {0}")]
    UnknownTool(String),

    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),

    #[error("Invalid tool schema: {0}")]
    InvalidSchema(String),

    #[error("Permission denied for tool: {0}")]
    PermissionDenied(String),

    #[error("Tool execution failed: {0}")]
    ToolExecution(String),

    #[error("Agent loop interrupted")]
    Interrupted,

    #[error("Max turns exceeded")]
    MaxTurnsExceeded,

    /// A stopped turn's handoff step produced no text, so the turn has no
    /// final message to report.
    #[error("The turn was stopped ({0}) and the model gave no final message")]
    NoFinalMessage(crate::agent_loop::types::StopReason),

    #[error("Configuration error: {0}")]
    Config(String),

    /// A blocking user hook (PreToolUse / UserPromptSubmit / Stop) stopped the
    /// operation; the reason is the hook's stderr or its JSON deny reason.
    #[error("Blocked by hook: {0}")]
    HookBlocked(String),

    /// The model's tool calls were cut off before they were complete — by the
    /// output-token limit or the provider's content filter — several steps in a
    /// row, so none of them could be run.
    #[error(
        "The model's tool calls were cut off ({reason}) {count} times in a row and could not be run. \
         Raise the output limit (--max-tokens) or ask for smaller steps."
    )]
    TruncatedToolCalls { reason: String, count: u32 },

    /// Another process is writing the session ([`crate::session_lock`]).
    #[error(transparent)]
    SessionOccupied(#[from] crate::session_lock::SessionOccupied),
}

impl AgentError {
    /// Whether the turn ended because its cancellation token fired — observed
    /// between steps ([`AgentError::Interrupted`]) or mid-request
    /// ([`LlmError::Cancelled`]). Both are a cut-short turn, and both are what
    /// a front-end's cancel produces; timeouts and limits are neither.
    pub fn is_cancellation(&self) -> bool {
        matches!(self, Self::Interrupted | Self::Llm(LlmError::Cancelled))
    }

    /// Whether the error is fatal and should be surfaced to the user rather than
    /// retried or reported back to the model as a tool result.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::Llm(e) => e.is_fatal(),
            Self::UnknownTool(_)
            | Self::InvalidArguments(_)
            | Self::InvalidSchema(_)
            | Self::PermissionDenied(_)
            | Self::Config(_)
            | Self::HookBlocked(_)
            | Self::MaxTurnsExceeded
            | Self::NoFinalMessage(_)
            | Self::TruncatedToolCalls { .. }
            | Self::SessionOccupied(_) => true,
            Self::ToolExecution(_) | Self::Interrupted => false,
        }
    }

    /// A short machine name for the error, used as the StopFailure hook's
    /// matcher input.
    #[must_use]
    pub fn hook_error_kind(&self) -> &'static str {
        match self {
            Self::Llm(LlmError::Cancelled) => "cancelled",
            Self::Llm(_) => "llm_error",
            Self::UnknownTool(_) => "unknown_tool",
            Self::InvalidArguments(_) => "invalid_arguments",
            Self::InvalidSchema(_) => "invalid_schema",
            Self::PermissionDenied(_) => "permission_denied",
            Self::ToolExecution(_) => "tool_execution",
            Self::Interrupted => "interrupted",
            Self::MaxTurnsExceeded => "max_turns",
            Self::NoFinalMessage(_) => "no_final_message",
            Self::Config(_) => "config",
            Self::HookBlocked(_) => "hook_blocked",
            Self::TruncatedToolCalls { .. } => "truncated_tool_calls",
            Self::SessionOccupied(_) => "session_occupied",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_denied_is_fatal() {
        assert!(AgentError::PermissionDenied("read_file".to_string()).is_fatal());
    }

    #[test]
    fn test_tool_execution_is_not_fatal() {
        assert!(!AgentError::ToolExecution("oops".to_string()).is_fatal());
    }

    #[test]
    fn test_llm_fatal_delegates() {
        assert!(AgentError::Llm(LlmError::Config("bad".to_string())).is_fatal());
        assert!(!AgentError::Llm(LlmError::ApiTimeout).is_fatal());
    }
}
