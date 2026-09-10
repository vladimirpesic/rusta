//! Wire types shared by both backends — development plan §6.2.

use serde::{Deserialize, Serialize};

/// Conversation role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System message: core prompt, capsules, skill cards (plan §6.6).
    System,
    /// User input and tool observations (plan §6.1 observation contract).
    User,
    /// Model output.
    Assistant,
}

impl Role {
    /// Lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// A single chat message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Sender role.
    pub role: Role,
    /// Message text.
    pub content: String,
}

impl Message {
    /// Builds a message with the given role.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }

    /// A `system` message.
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    /// A `user` message.
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    /// An `assistant` message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }
}

/// One completion request — identical for both backends (plan §6.2).
///
/// Rusta never sends `tools`/`functions` request parameters (DECIDED, plan §6.1);
/// native `tool_calls` in *responses* are passed through, never requested.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    /// Conversation so far.
    pub messages: Vec<Message>,
    /// Sampling budget; plan §7 default 4096.
    pub max_tokens: u32,
    /// Sampling temperature; plan §7 default 0.2.
    pub temperature: f32,
    /// Stop sequences; empty by default.
    pub stop: Vec<String>,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            max_tokens: 4096,
            temperature: 0.2,
            stop: Vec::new(),
        }
    }
}

impl ChatRequest {
    /// A request with the given messages and default sampling settings.
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Self::default()
        }
    }
}

/// Why a stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// Natural stop.
    Stop,
    /// `max_tokens` reached.
    Length,
    /// The model emitted native tool calls.
    ToolCalls,
    /// Filtered by the server.
    ContentFilter,
    /// Server-specific reason, kept verbatim.
    Other(String),
}

impl FinishReason {
    /// Maps an OpenAI `finish_reason` string to a reason.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" | "function_call" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// Events emitted while a completion streams (plans §6.2, §6.1).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Incremental assistant text.
    Delta(String),
    /// A fully assembled native tool call — OpenAI `tool_calls` passthrough
    /// (plan §6.1). Emitted after the last fragment, before [`StreamEvent::Finish`].
    ToolCall {
        /// Server-assigned call id, when present.
        id: Option<String>,
        /// Tool name.
        name: String,
        /// Raw JSON-arguments text, exactly as generated.
        arguments: String,
    },
    /// The stream ended normally; always the last successful event.
    Finish(FinishReason),
    /// The stream failed after partial output; the text carries a remedy
    /// (plan §6.11). Always the final event.
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trips_lowercase() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        let message: Message =
            serde_json::from_str("{\"role\":\"user\",\"content\":\"hi\"}").unwrap();
        assert_eq!(message, Message::user("hi"));
    }

    #[test]
    fn finish_reason_parses_known_values() {
        assert_eq!(FinishReason::parse("stop"), FinishReason::Stop);
        assert_eq!(FinishReason::parse("length"), FinishReason::Length);
        assert_eq!(FinishReason::parse("tool_calls"), FinishReason::ToolCalls);
        assert_eq!(
            FinishReason::parse("function_call"),
            FinishReason::ToolCalls
        );
        assert_eq!(
            FinishReason::parse("custom"),
            FinishReason::Other("custom".to_owned())
        );
    }
}
