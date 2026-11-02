use serde::{Deserialize, Serialize};

use crate::{provider::FinishReason, usage::TokenUsage};

/// Role of a message in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single part of message content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Think {
        think: String,
    },
    ImageUrl {
        image_url: ImageUrl,
    },
    AudioUrl {
        audio_url: UrlRef,
    },
    VideoUrl {
        video_url: UrlRef,
    },
    /// A gateway's structured reasoning payload (`reasoning_details`), kept
    /// verbatim so it can be echoed back on the next request.
    ///
    /// OpenRouter-style gateways stream reasoning as an array of detail objects
    /// — summaries, plain text, encrypted blobs — and expect the array back
    /// unchanged to continue the model's reasoning across tool calls. The
    /// readable text is also surfaced as a [`ContentPart::Think`]; this part only
    /// carries the opaque original, and every consumer other than the
    /// OpenAI-compatible serializer ignores it.
    ReasoningDetails {
        details: Vec<serde_json::Value>,
    },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn think(think: impl Into<String>) -> Self {
        Self::Think {
            think: think.into(),
        }
    }

    pub fn is_text(&self) -> bool {
        matches!(self, Self::Text { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UrlRef {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// A fully assembled tool call produced by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ToolCallKind,
    pub name: String,
    /// JSON-encoded arguments, or `None` if the model only emitted the header.
    pub arguments: Option<String>,
    /// Provider-specific extras (e.g., `reasoning_content`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extras: Option<serde_json::Map<String, serde_json::Value>>,
    /// Internal stream index used for routing deltas.
    #[serde(skip)]
    pub stream_index: Option<serde_json::Number>,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: ToolCallKind::Function,
            name: name.into(),
            arguments: None,
            extras: None,
            stream_index: None,
        }
    }

    pub fn with_arguments(mut self, arguments: impl Into<String>) -> Self {
        self.arguments = Some(arguments.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallKind {
    Function,
    /// Moonshot native builtin function (e.g. `$web_search`).
    BuiltinFunction,
}

/// A streaming delta for a tool call argument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallPart {
    /// Stream index that this delta belongs to.
    pub index: serde_json::Number,
    /// Delta of JSON-encoded arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments_part: Option<String>,
}

/// Any part that can arrive while streaming.
pub type StreamedMessagePart = StreamedPart;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamedPart {
    Content(ContentPart),
    ToolCall(ToolCall),
    ToolCallPart(ToolCallPart),
    Usage(TokenUsage),
    /// Streaming finish-reason marker.
    Finish(Option<FinishReason>, Option<String>),
}

/// A message in the conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Message-level tool declarations used by providers that support
    /// mid-conversation schema loading (e.g. Kimi `messages[].tools`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<crate::tool::Tool>,
    #[serde(skip)]
    pub partial: bool,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentPart::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentPart::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentPart::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: Vec::new(),
            tool_calls,
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentPart::text(content)],
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            tools: Vec::new(),
            partial: false,
        }
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn think_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Think { think } => Some(think.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Whether this is a reply of reasoning alone: thinking with something in
    /// it, and neither answer text nor a tool call.
    #[must_use]
    pub fn is_thinking_only(&self) -> bool {
        self.tool_calls.is_empty()
            && self.text_content().trim().is_empty()
            && !self.think_content().trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(content: Vec<ContentPart>, tool_calls: Vec<ToolCall>) -> Message {
        Message {
            content,
            tool_calls,
            ..Message::assistant_text("")
        }
    }

    /// Reasoning alone is a thinking-only reply; text or a tool call beside
    /// it, or reasoning with nothing in it, is not.
    #[test]
    fn a_thinking_only_reply_has_reasoning_and_nothing_else() {
        let think = || ContentPart::think("Nothing to change.");
        assert!(assistant(vec![think()], Vec::new()).is_thinking_only());
        assert!(assistant(vec![think(), ContentPart::text("  ")], Vec::new()).is_thinking_only());
        assert!(
            !assistant(vec![think(), ContentPart::text("Done.")], Vec::new()).is_thinking_only()
        );
        assert!(!assistant(vec![think()], vec![ToolCall::new("tc", "bash")]).is_thinking_only());
        assert!(!assistant(vec![ContentPart::think(" ")], Vec::new()).is_thinking_only());
        assert!(!assistant(Vec::new(), Vec::new()).is_thinking_only());
    }
}
