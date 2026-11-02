use serde_json::{Value, json};

use super::reasoning_key::{self, ReasoningKey};
use crate::{
    LlmError,
    message::{ContentPart, Message, Role, ToolCallKind},
    provider::FinishReason,
    tool::Tool,
    usage::TokenUsage,
};

/// OpenAI-compatible request/response types.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenAiTool {
    pub r#type: String,
    pub function: OpenAiFunction,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenAiFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

/// One chat-completions message, in either direction.
///
/// Reasoning is the asymmetric part. On the way *in* it may arrive under any of
/// three keys, so all three are parsed separately — collapsing them with serde
/// aliases would lose which one the endpoint actually used, and that is exactly
/// what has to be echoed back. On the way *out* exactly one key is written,
/// named by `reasoning_key`. See [`super::reasoning_key`].
#[derive(Debug, serde::Deserialize)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Moonshot/Kimi, Z.AI GLM, DeepSeek, older vLLM.
    #[serde(default, deserialize_with = "de_reasoning_string")]
    pub reasoning_content: Option<String>,
    /// OpenAI GPT-OSS guidance, current vLLM.
    #[serde(default, deserialize_with = "de_reasoning_string")]
    pub reasoning: Option<String>,
    /// OpenRouter-style gateways: a string on some, an array of detail objects
    /// on others. Kept raw so the array form can be echoed back verbatim.
    #[serde(default)]
    pub reasoning_details: Option<Value>,
    #[serde(default)]
    pub tools: Option<Vec<OpenAiTool>>,
    /// The key to write reasoning under. Never deserialized — it describes the
    /// outbound shape, and is set by whoever builds the request.
    #[serde(skip)]
    pub reasoning_key: ReasoningKey,
    /// Structured `reasoning_details` items to echo back verbatim on this
    /// (outbound, assistant) message. Never deserialized.
    #[serde(skip)]
    pub reasoning_details_echo: Option<Vec<Value>>,
}

impl OpenAiMessage {
    /// The reasoning text this message carries, and the key that carried it.
    #[must_use]
    pub fn inbound_reasoning(&self) -> Option<(ReasoningKey, &str)> {
        reasoning_key::pick(
            self.reasoning_content.as_deref(),
            self.reasoning_details.as_ref().and_then(Value::as_str),
            self.reasoning.as_deref(),
        )
    }

    /// The reasoning text, whichever dialect holds it.
    #[must_use]
    pub fn reasoning_text(&self) -> Option<&str> {
        self.inbound_reasoning().map(|(_, text)| text)
    }

    /// The structured `reasoning_details` array, when the endpoint sent one.
    #[must_use]
    pub fn reasoning_detail_items(&self) -> Option<&[Value]> {
        match &self.reasoning_details {
            Some(Value::Array(items)) if !items.is_empty() => Some(items),
            _ => None,
        }
    }
}

// Serialization is hand-written because the reasoning *key* is dynamic, which
// `#[serde(rename)]` cannot express. Everything else follows the derive's
// skip-if-none behaviour, except `content`, which is deliberately allowed to
// serialize as an explicit null — see `message_to_openai`.
impl serde::Serialize for OpenAiMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let reasoning = self.reasoning_text();
        let echo = self
            .reasoning_details_echo
            .as_ref()
            .filter(|items| !items.is_empty());
        // The echoed array occupies `reasoning_details`, so a string that would
        // otherwise go there moves to `reasoning_content` rather than
        // colliding on one key.
        let reasoning_key = match (echo, self.reasoning_key) {
            (Some(_), ReasoningKey::ReasoningDetails) => ReasoningKey::ReasoningContent,
            (_, key) => key,
        };
        let mut len = 1; // role
        len += usize::from(self.content.is_some());
        len += usize::from(self.tool_calls.is_some());
        len += usize::from(self.tool_call_id.is_some());
        len += usize::from(reasoning.is_some());
        len += usize::from(echo.is_some());
        len += usize::from(self.tools.is_some());

        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("role", &self.role)?;
        if let Some(content) = &self.content {
            map.serialize_entry("content", content)?;
        }
        if let Some(tool_calls) = &self.tool_calls {
            map.serialize_entry("tool_calls", tool_calls)?;
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", tool_call_id)?;
        }
        if let Some(reasoning) = reasoning {
            map.serialize_entry(reasoning_key.as_str(), reasoning)?;
        }
        if let Some(items) = echo {
            map.serialize_entry(ReasoningKey::ReasoningDetails.as_str(), items)?;
        }
        if let Some(tools) = &self.tools {
            map.serialize_entry("tools", tools)?;
        }
        map.end()
    }
}

/// Deserialize the reasoning field leniently: a JSON string becomes `Some(text)`,
/// anything else (null, a structured `reasoning_details` array, etc.) becomes
/// `None`. This lets [`OpenAiMessage`] accept every OpenAI-compatible reasoning
/// dialect without a stray non-string shape failing the whole message parse.
pub(crate) fn de_reasoning_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Value as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.as_str().map(str::to_string))
}

/// The readable text inside a structured `reasoning_details` array.
///
/// Items carry their text under `text` (plain reasoning) or `summary`
/// (summarized reasoning); encrypted items carry none and are skipped. Used only
/// for display when no string reasoning field came with the array.
#[must_use]
pub(crate) fn reasoning_details_text(items: &[Value]) -> String {
    items
        .iter()
        .filter_map(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .or_else(|| item.get("summary").and_then(Value::as_str))
        })
        .collect()
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenAiStreamChoice {
    pub delta: OpenAiMessage,
    pub finish_reason: Option<String>,
    pub index: u32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenAiStreamResponse {
    pub id: String,
    pub choices: Vec<OpenAiStreamChoice>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct OpenAiUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<OpenAiTokenDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<OpenAiTokenDetails>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct OpenAiTokenDetails {
    pub cached_tokens: Option<u64>,
    /// Prompt tokens written to the cache by this request (Moonshot's K3
    /// series, which bills cache writes separately).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u64>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenAiResponse {
    pub id: String,
    pub choices: Vec<OpenAiChoice>,
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenAiChoice {
    pub message: OpenAiMessage,
    pub finish_reason: Option<String>,
    pub usage: Option<OpenAiUsage>,
}

impl OpenAiTool {
    #[must_use]
    pub fn from_tool(tool: &Tool) -> Self {
        if tool.is_builtin_function() {
            // Moonshot Kimi builtin functions (e.g. `$web_search`) must be sent with
            // type `builtin_function` and no description/parameters.
            return Self {
                r#type: "builtin_function".to_string(),
                function: OpenAiFunction {
                    name: tool.name.clone(),
                    description: None,
                    parameters: None,
                },
            };
        }

        Self {
            r#type: "function".to_string(),
            function: OpenAiFunction {
                name: tool.name.clone(),
                description: Some(tool.description.clone()),
                parameters: Some(tool.parameters.clone()),
            },
        }
    }
}

/// Convert a normalized [`Message`] history to OpenAI-compatible messages.
#[must_use]
/// Convert history into the endpoint's message shape.
///
/// `supports_media` reports whether the target model can accept image, audio and
/// video parts. When it cannot, each such part is replaced with a short text
/// note rather than sent and rejected: history is durable and the model is not,
/// so a conversation that read an image on a vision-capable model outlives the
/// switch to a text-only one, and every later request would otherwise carry
/// media the endpoint answers with a 400. The turn that fails is not the turn
/// that read the image, which leaves nothing to undo and no way out.
///
/// `echo_reasoning_details` controls whether a stored structured
/// `reasoning_details` array is sent back on its assistant message. Only the
/// generic OpenAI-compatible provider sets it: that array comes from a gateway
/// that asks for it back, and Moonshot and Z.AI reject properties their schemas
/// do not declare — which, after a mid-session `/provider` switch, a stored
/// array would otherwise become.
pub fn messages_to_openai(
    system_prompt: Option<&str>,
    messages: &[Message],
    reasoning_key: ReasoningKey,
    supports_media: bool,
    echo_reasoning_details: bool,
) -> Vec<OpenAiMessage> {
    let mut out = Vec::with_capacity(messages.len() + system_prompt.is_some() as usize);
    if let Some(sp) = system_prompt {
        out.push(OpenAiMessage {
            role: "system".to_string(),
            content: Some(json!(sp)),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_details: None,
            tools: None,
            reasoning_key,
            reasoning_details_echo: None,
        });
    }

    for msg in messages {
        let mut converted = message_to_openai(msg, supports_media);
        converted.reasoning_key = reasoning_key;
        if !echo_reasoning_details {
            converted.reasoning_details_echo = None;
        }
        out.push(converted);
    }
    out
}

/// Stand-ins for media a model cannot accept. Worded so the model knows
/// something was there and why it cannot see it, rather than reading a gap in
/// the conversation as the user having said nothing.
const MEDIA_DROPPED_IMAGE: &str = "[image omitted: the current model does not support image input]";
const MEDIA_DROPPED_AUDIO: &str = "[audio omitted: the current model does not support audio input]";
const MEDIA_DROPPED_VIDEO: &str = "[video omitted: the current model does not support video input]";

fn message_to_openai(msg: &Message, supports_media: bool) -> OpenAiMessage {
    let role = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
    .to_string();

    let mut text_content: Vec<String> = Vec::new();
    let mut multimodal: Vec<Value> = Vec::new();
    let mut reasoning_content: Option<String> = None;
    let mut reasoning_details: Vec<Value> = Vec::new();

    for part in &msg.content {
        match part {
            ContentPart::ReasoningDetails { details } => {
                reasoning_details.extend(details.iter().cloned());
            }
            ContentPart::Text { text } => {
                text_content.push(text.clone());
                multimodal.push(json!({"type": "text", "text": text}));
            }
            ContentPart::Think { think } => {
                // Reasoning is stored separately when replaying to Kimi/GLM.
                if let Some(rc) = reasoning_content.as_mut() {
                    rc.push_str(think);
                } else {
                    reasoning_content = Some(think.clone());
                }
            }
            ContentPart::ImageUrl { image_url } => {
                if supports_media {
                    multimodal.push(json!({
                        "type": "image_url",
                        "image_url": { "url": image_url.url }
                    }));
                } else {
                    // Degrade rather than send-and-be-rejected. Recorded the
                    // same way a text part is, so the `content` shape chosen
                    // below stays consistent and a message whose only part was
                    // media does not serialize to an empty one.
                    text_content.push(MEDIA_DROPPED_IMAGE.to_string());
                    multimodal.push(json!({"type": "text", "text": MEDIA_DROPPED_IMAGE}));
                }
            }
            ContentPart::AudioUrl { audio_url } => {
                if supports_media {
                    multimodal.push(json!({
                        "type": "audio_url",
                        "audio_url": { "url": audio_url.url }
                    }));
                } else {
                    // Degrade rather than send-and-be-rejected. Recorded the
                    // same way a text part is, so the `content` shape chosen
                    // below stays consistent and a message whose only part was
                    // media does not serialize to an empty one.
                    text_content.push(MEDIA_DROPPED_AUDIO.to_string());
                    multimodal.push(json!({"type": "text", "text": MEDIA_DROPPED_AUDIO}));
                }
            }
            ContentPart::VideoUrl { video_url } => {
                if supports_media {
                    multimodal.push(json!({
                        "type": "video_url",
                        "video_url": { "url": video_url.url }
                    }));
                } else {
                    // Degrade rather than send-and-be-rejected. Recorded the
                    // same way a text part is, so the `content` shape chosen
                    // below stays consistent and a message whose only part was
                    // media does not serialize to an empty one.
                    text_content.push(MEDIA_DROPPED_VIDEO.to_string());
                    multimodal.push(json!({"type": "text", "text": MEDIA_DROPPED_VIDEO}));
                }
            }
        }
    }

    let content = if multimodal.len() == 1 && text_content.len() == 1 {
        Some(json!(text_content.join("")))
    } else if multimodal.is_empty() {
        None
    } else {
        Some(json!(multimodal))
    };

    let tool_calls = if msg.tool_calls.is_empty() {
        None
    } else {
        Some(
            msg.tool_calls
                .iter()
                .map(|t| OpenAiToolCall {
                    id: t.id.clone(),
                    r#type: match t.kind {
                        ToolCallKind::BuiltinFunction => "builtin_function".to_string(),
                        ToolCallKind::Function => "function".to_string(),
                    },
                    function: OpenAiToolCallFunction {
                        name: t.name.clone(),
                        arguments: t.arguments.clone().unwrap_or_default(),
                    },
                })
                .collect(),
        )
    };

    let tools = if msg.tools.is_empty() {
        None
    } else {
        Some(msg.tools.iter().map(OpenAiTool::from_tool).collect())
    };

    // OpenAI emits `content: null` alongside `tool_calls`, and strict
    // chat-completions validators — LiteLLM and gateway proxies among them —
    // reject an assistant message that omits the key entirely. The rejection is
    // a 422 on *every* resend of the history, so a single such message
    // permanently poisons the session rather than failing one turn. An
    // assistant turn that produced only tool calls therefore sends an explicit
    // null instead of nothing.
    //
    // Only that shape changes. A message with neither content nor tool calls is
    // left alone, because a null would not rescue it: Moonshot and Z.AI reject an
    // empty assistant message whichever way it is spelled, which is why
    // `context_projector` drops it before it ever reaches a provider.
    let content = match (content, &tool_calls) {
        (None, Some(_)) if msg.role == Role::Assistant => Some(Value::Null),
        (content, _) => content,
    };

    OpenAiMessage {
        role,
        content,
        tool_calls,
        tool_call_id: msg.tool_call_id.clone(),
        reasoning_content,
        reasoning: None,
        reasoning_details: None,
        tools,
        // Overwritten by `messages_to_openai`, which knows the endpoint's
        // dialect. On its own a converted message assumes the default, which is
        // what Kimi and GLM require.
        reasoning_key: ReasoningKey::default(),
        reasoning_details_echo: (msg.role == Role::Assistant && !reasoning_details.is_empty())
            .then_some(reasoning_details),
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenAiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(rename = "function")]
    pub function: OpenAiToolCallFunction,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenAiToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[must_use]
pub fn parse_finish_reason(raw: Option<&str>) -> Option<FinishReason> {
    match raw {
        Some("stop") => Some(FinishReason::Stop),
        Some("length") => Some(FinishReason::Length),
        Some("tool_calls") => Some(FinishReason::ToolCalls),
        Some("content_filter") => Some(FinishReason::ContentFilter),
        // Z.AI's spellings. `sensitive` is its content filter;
        // `model_context_window_exceeded` arrives on a 200, so nothing in the
        // error path sees it.
        Some("sensitive") => Some(FinishReason::ContentFilter),
        Some("model_context_window_exceeded") => Some(FinishReason::ContextWindowExceeded),
        // The provider cut the generation short: Z.AI's `network_error`, and
        // DeepSeek's `insufficient_system_resource` and `aborted`
        // (https://api-docs.deepseek.com/api/create-chat-completion).
        Some("network_error" | "insufficient_system_resource" | "aborted") => {
            Some(FinishReason::Interrupted)
        }
        Some(other) => {
            tracing::warn!("Unrecognized finish_reason {other:?}; treating the turn as finished");
            None
        }
        None => None,
    }
}

#[must_use]
pub fn extract_usage(usage: Option<&OpenAiUsage>) -> TokenUsage {
    usage.map_or_else(TokenUsage::empty, |u| {
        let input_cache_read = u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens);
        TokenUsage {
            input_tokens: u.prompt_tokens.unwrap_or(0),
            output_tokens: u.completion_tokens.unwrap_or(0),
            total_tokens: u.total_tokens.unwrap_or(0),
            cached_tokens: input_cache_read,
            reasoning_tokens: u
                .completion_tokens_details
                .as_ref()
                .and_then(|d| d.reasoning_tokens),
            input_cache_read,
            input_cache_creation: u
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cache_write_tokens),
        }
    })
}

/// Z.AI error codes meaning the caller's balance, package, or usage allowance is
/// spent. `1113` is "insufficient balance or no resource package"; `1308`–`1321`
/// are the Coding Plan's expiry and usage-limit failures. Retrying any of them
/// re-sends a request that cannot succeed.
///
/// `1302` (rate limit) and `1305` (temporarily overloaded) are deliberately
/// absent: those *are* worth retrying.
fn zai_quota_exhausted_code(body: &str) -> Option<String> {
    const EXHAUSTED: [&str; 15] = [
        "1113", "1308", "1309", "1310", "1311", "1312", "1313", "1314", "1315", "1316", "1317",
        "1318", "1319", "1320", "1321",
    ];
    EXHAUSTED
        .iter()
        .find(|code| error_code_is(body, code))
        .map(|code| (*code).to_string())
}

/// Moonshot signals a spent balance or token quota with an error *type* rather
/// than a numeric code, on an HTTP 429 it shares with the retryable rate-limit
/// and overload errors.
fn moonshot_quota_exhausted(body: &str) -> bool {
    error_field_is(body, "type", "exceeded_current_quota_error")
}

/// Whether the body reports a spent balance or allowance. Returns the vendor's
/// code, or a stand-in for Moonshot's type-only errors.
fn quota_exhausted_code(body: &str) -> Option<String> {
    zai_quota_exhausted_code(body).or_else(|| {
        moonshot_quota_exhausted(body).then(|| "exceeded_current_quota_error".to_string())
    })
}

#[must_use]
pub fn classify_api_error(status: reqwest::StatusCode, body: &str, retry_hint: bool) -> LlmError {
    let lower = body.to_lowercase();
    let is_context_overflow = lower.contains("context length")
        || lower.contains("too long")
        || lower.contains("maximum context")
        // Moonshot phrases the same failure without any of the words above:
        // "prompt tokens + max_tokens exceeds the model specification".
        || lower.contains("exceeds the model specification")
        || lower.contains("exceeded model token limit")
        // Z.AI code 1261, "Prompt too long".
        || body.contains("1261")
        || error_code_is_context_overflow(body);

    if is_context_overflow {
        return LlmError::ApiContextOverflow;
    }

    // Too many *bytes*, not too many tokens: Kimi's 2 MiB body limit
    // ("total message size 5943865 exceeds limit 2097152"), or a 413 that did
    // not blame the context above. Compacting would not help — the media would
    // stay.
    if is_message_size_error(&lower)
        || lower.contains("request entity too large")
        || lower.contains("payload too large")
        || status == reqwest::StatusCode::PAYLOAD_TOO_LARGE
    {
        return LlmError::ApiRequestTooLarge(
            error_message(body).unwrap_or_else(|| body.to_string()),
        );
    }

    // A spent balance or allowance is a business error: fatal, never retried.
    if let Some(code) = quota_exhausted_code(body) {
        return LlmError::ApiBusinessError {
            code,
            message: error_message(body).unwrap_or_else(|| body.to_string()),
        };
    }

    LlmError::ApiStatus {
        status,
        message: body.to_string(),
        retry_hint,
        // Populated by the caller via `with_retry_after` from the response
        // headers, which are not available here.
        retry_after: None,
    }
}

/// Kimi's 2 MiB body limit, which puts the size between the words: "total
/// message size 5943865 exceeds limit 2097152".
fn is_message_size_error(lower: &str) -> bool {
    lower
        .find("message size")
        .is_some_and(|at| lower[at..].contains("exceeds limit"))
}

/// Pull `error.message` out of a JSON error body.
fn error_message(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .pointer("/error/message")
        .and_then(|m| m.as_str())
        .map(String::from)
}

/// Whether `error.<field>` equals `expected`.
fn error_field_is(body: &str, field: &str, expected: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    value
        .get("error")
        .and_then(|e| e.get(field))
        .and_then(|c| c.as_str())
        == Some(expected)
}

/// Parse a JSON error body and check whether the error code equals `expected`.
fn error_code_is(body: &str, expected: &str) -> bool {
    error_field_is(body, "code", expected)
}

/// Parse a JSON error body and detect known context-overflow error codes.
fn error_code_is_context_overflow(body: &str) -> bool {
    error_code_is(body, "1261")
        || error_code_is(body, "context_length_exceeded")
        || error_code_is(body, "context_overflow")
        || error_code_is(body, "max_tokens")
}

#[cfg(test)]
mod tests {
    use crate::message::ImageUrl;
    use reqwest::StatusCode;

    use super::*;

    /// Moonshot's K3 series reports cache reads and cache writes apart; the
    /// writes become the cache-creation count.
    #[test]
    fn cache_writes_are_read_from_the_prompt_details() {
        let usage: OpenAiUsage = serde_json::from_str(
            r#"{"prompt_tokens":1000,"completion_tokens":5,"total_tokens":1005,
                "prompt_tokens_details":{"cached_tokens":600,"cache_write_tokens":384}}"#,
        )
        .unwrap();
        let usage = extract_usage(Some(&usage));
        assert_eq!(usage.input_cache_read, Some(600));
        assert_eq!(usage.input_cache_creation, Some(384));

        let without: OpenAiUsage = serde_json::from_str(
            r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":0}}"#,
        )
        .unwrap();
        assert_eq!(extract_usage(Some(&without)).input_cache_creation, None);
    }

    fn reasoning_of(json: &str) -> Option<String> {
        serde_json::from_str::<OpenAiMessage>(json)
            .expect("message must parse")
            .reasoning_text()
            .map(str::to_string)
    }

    /// A user message with explicit content parts.
    fn user_with(parts: Vec<ContentPart>) -> Message {
        let mut msg = Message::user("");
        msg.content = parts;
        msg
    }

    fn wire_of(msg: &Message) -> serde_json::Map<String, Value> {
        match serde_json::to_value(message_to_openai(msg, true)).expect("must serialize") {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    fn assistant_calling(name: &str) -> Message {
        Message::assistant_tool_calls(vec![
            crate::message::ToolCall::new("call_1", name).with_arguments("{}"),
        ])
    }

    /// An assistant turn that produced only tool calls must carry an explicit
    /// `content: null`. Omitting the key is a 422 on strict chat-completions
    /// validators, and because the whole history is resent every turn, one such
    /// message poisons the session permanently.
    #[test]
    fn tool_call_only_assistant_message_sends_an_explicit_null_content() {
        let wire = wire_of(&assistant_calling("read_file"));
        assert!(
            wire.contains_key("content"),
            "the `content` key must be present, not omitted"
        );
        assert_eq!(wire["content"], Value::Null);
        assert!(wire.contains_key("tool_calls"));
    }

    /// Reasoning rides in its own field, so a thinking turn that ended in tool
    /// calls needs the same null.
    #[test]
    fn thinking_plus_tool_calls_still_sends_a_null_content() {
        let mut msg = assistant_calling("grep");
        msg.content = vec![ContentPart::Think {
            think: "considering".to_string(),
        }];
        let wire = wire_of(&msg);
        assert_eq!(wire["content"], Value::Null);
        assert_eq!(wire["reasoning_content"], json!("considering"));
    }

    /// Real content is never overwritten by the null.
    #[test]
    fn text_alongside_tool_calls_is_left_alone() {
        let mut msg = assistant_calling("bash");
        msg.content = vec![ContentPart::Text {
            text: "running it".to_string(),
        }];
        assert_eq!(wire_of(&msg)["content"], json!("running it"));
    }

    /// The null is scoped to assistant tool-call messages: a contentless message
    /// with no tool calls stays omitted, because a null would not rescue it.
    #[test]
    fn the_null_content_is_scoped_to_assistant_tool_calls() {
        let empty_assistant = Message {
            role: Role::Assistant,
            content: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        };
        assert!(!wire_of(&empty_assistant).contains_key("content"));
        assert_eq!(
            wire_of(&Message::tool_result("call_1", "output"))["content"],
            json!("output")
        );
    }

    #[test]
    fn reasoning_read_from_every_dialect() {
        // Primary providers (Kimi/GLM/DeepSeek): `reasoning_content`.
        assert_eq!(
            reasoning_of(r#"{"role":"assistant","reasoning_content":"kimi think"}"#),
            Some("kimi think".to_string())
        );
        // OpenAI GPT-OSS / current vLLM: `reasoning`.
        assert_eq!(
            reasoning_of(r#"{"role":"assistant","reasoning":"vllm think"}"#),
            Some("vllm think".to_string())
        );
        // `reasoning_details` string form.
        assert_eq!(
            reasoning_of(r#"{"role":"assistant","reasoning_details":"detail think"}"#),
            Some("detail think".to_string())
        );
        // Absent → None: plain messages are unchanged.
        assert_eq!(reasoning_of(r#"{"role":"assistant","content":"hi"}"#), None);
    }

    #[test]
    fn non_string_reasoning_is_ignored_not_a_parse_error() {
        // An OpenRouter-style structured array (or null) must not fail the message
        // parse — it degrades to no captured reasoning, exactly as before.
        assert_eq!(
            reasoning_of(
                r#"{"role":"assistant","content":"hi","reasoning_details":[{"text":"x"}]}"#
            ),
            None
        );
        assert_eq!(
            reasoning_of(r#"{"role":"assistant","content":"hi","reasoning":null}"#),
            None
        );
    }

    #[test]
    fn reasoning_serializes_back_as_reasoning_content() {
        // Outbound must stay byte-identical for Kimi/GLM: always `reasoning_content`.
        let out = serde_json::to_string(&reasoning_message(ReasoningKey::default())).unwrap();
        assert!(
            out.contains(r#""reasoning_content":"echo me""#),
            "got: {out}"
        );
    }

    fn reasoning_message(key: ReasoningKey) -> OpenAiMessage {
        OpenAiMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some("echo me".into()),
            reasoning: None,
            reasoning_details: None,
            tools: None,
            reasoning_key: key,
            reasoning_details_echo: None,
        }
    }

    /// A structured array is kept whole for the echo, and its readable text is
    /// extractable for display.
    #[test]
    fn a_reasoning_details_array_is_kept_verbatim() {
        let parsed: OpenAiMessage = serde_json::from_str(
            r#"{"role":"assistant","content":"hi","reasoning_details":[{"type":"reasoning.text","text":"step one"},{"type":"reasoning.encrypted","data":"AAA="}]}"#,
        )
        .unwrap();
        let items = parsed.reasoning_detail_items().expect("array kept");
        assert_eq!(items.len(), 2);
        assert_eq!(reasoning_details_text(items), "step one");
    }

    #[test]
    fn reasoning_details_text_reads_text_then_summary() {
        let items = vec![
            json!({"type": "reasoning.summary", "summary": "sum "}),
            json!({"type": "reasoning.text", "text": "txt"}),
            json!({"type": "reasoning.encrypted", "data": "zzz"}),
        ];
        assert_eq!(reasoning_details_text(&items), "sum txt");
    }

    /// A stored array goes back out verbatim under `reasoning_details`; the
    /// string copy moves to `reasoning_content` rather than colliding on the
    /// same key.
    #[test]
    fn a_stored_details_array_is_echoed_and_the_string_moves_aside() {
        let mut msg = reasoning_message(ReasoningKey::ReasoningDetails);
        msg.reasoning_details_echo = Some(vec![json!({"type": "reasoning.text", "text": "t"})]);
        let out: Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(out["reasoning_details"][0]["text"], json!("t"));
        assert_eq!(out["reasoning_content"], json!("echo me"));
    }

    /// The echo is opt-in per endpoint: Kimi and GLM never receive it.
    #[test]
    fn details_are_echoed_only_when_the_endpoint_asks_for_them() {
        let mut assistant = Message::assistant_text("answer");
        assistant.content.push(ContentPart::ReasoningDetails {
            details: vec![json!({"type": "reasoning.text", "text": "t"})],
        });
        let history = vec![Message::user("q"), assistant];

        let echoed = messages_to_openai(None, &history, ReasoningKey::default(), false, true);
        assert!(echoed[1].reasoning_details_echo.is_some());

        let plain = messages_to_openai(None, &history, ReasoningKey::default(), false, false);
        assert!(plain[1].reasoning_details_echo.is_none());
        let wire = serde_json::to_string(&plain).unwrap();
        assert!(!wire.contains("reasoning_details"), "{wire}");
    }

    /// Reasoning must go back out under the *endpoint's* key. Echoed under a
    /// name it does not read, it is dropped — which silently defeats Preserved
    /// Thinking.
    #[test]
    fn reasoning_serializes_under_the_endpoints_own_key() {
        let out = serde_json::to_string(&reasoning_message(ReasoningKey::Reasoning)).unwrap();
        assert!(out.contains(r#""reasoning":"echo me""#), "got: {out}");
        assert!(
            !out.contains("reasoning_content"),
            "only one reasoning key may be written: {out}"
        );
    }

    /// Whichever dialect the text arrived under is reported, so the provider can
    /// remember it.
    #[test]
    fn the_inbound_dialect_is_reported() {
        let parsed: OpenAiMessage =
            serde_json::from_str(r#"{"role":"assistant","reasoning":"vllm think"}"#).unwrap();
        assert_eq!(
            parsed.inbound_reasoning(),
            Some((ReasoningKey::Reasoning, "vllm think"))
        );
    }

    #[test]
    fn test_openai_tool_from_builtin_function() {
        let tool = Tool::new("$web_search", "Search the web", serde_json::json!({}));
        let openai = OpenAiTool::from_tool(&tool);
        assert_eq!(openai.r#type, "builtin_function");
        assert_eq!(openai.function.name, "$web_search");
        assert!(openai.function.description.is_none());
        assert!(openai.function.parameters.is_none());
    }

    #[test]
    fn test_openai_tool_from_regular_function() {
        let tool = Tool::new(
            "read_file",
            "Read a file",
            serde_json::json!({"type": "object"}),
        );
        let openai = OpenAiTool::from_tool(&tool);
        assert_eq!(openai.r#type, "function");
        assert_eq!(openai.function.name, "read_file");
        assert_eq!(openai.function.description.as_deref(), Some("Read a file"));
        assert_eq!(
            openai.function.parameters,
            Some(serde_json::json!({"type": "object"}))
        );
    }

    /// A 413 is a context overrun only when the body says so; otherwise the
    /// request had too many bytes, which compaction would not fix.
    #[test]
    fn test_classify_payload_too_large() {
        let err = classify_api_error(StatusCode::PAYLOAD_TOO_LARGE, "too big", false);
        assert!(matches!(err, LlmError::ApiRequestTooLarge(_)), "{err:?}");
        let err = classify_api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "This model's maximum context length is 8192 tokens",
            false,
        );
        assert!(matches!(err, LlmError::ApiContextOverflow), "{err:?}");
    }

    /// Kimi's 2 MiB body limit comes back as a 400, worded as its error
    /// reference shows, with the size between the words.
    #[test]
    fn test_classify_kimi_message_size_limit() {
        for message in [
            "total message size 5943865 exceeds limit 2097152",
            "message size exceeds limit 2097152",
        ] {
            let body = json!({"error": {"message": message, "type": "invalid_request_error"}});
            let err = classify_api_error(StatusCode::BAD_REQUEST, &body.to_string(), false);
            assert!(
                matches!(&err, LlmError::ApiRequestTooLarge(m) if m == message),
                "{err:?}"
            );
            assert!(!err.is_retryable());
        }
        // The same words apart from the size are not this error.
        let err = classify_api_error(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"batch exceeds limit 20"}}"#,
            false,
        );
        assert!(!matches!(err, LlmError::ApiRequestTooLarge(_)), "{err:?}");
    }

    #[test]
    fn test_classify_glm_error_code_1261_as_overflow() {
        let body = r#"{"error":{"code":"1261","message":"context length exceeded"}}"#;
        let err = classify_api_error(StatusCode::BAD_REQUEST, body, false);
        assert!(matches!(err, LlmError::ApiContextOverflow));
    }

    #[test]
    fn test_classify_context_length_text_as_overflow() {
        let err = classify_api_error(StatusCode::BAD_REQUEST, "Context length exceeded", false);
        assert!(matches!(err, LlmError::ApiContextOverflow));
    }

    #[test]
    fn test_classify_other_error_as_api_status() {
        let err = classify_api_error(StatusCode::UNAUTHORIZED, "Invalid API key", false);
        assert!(
            matches!(err, LlmError::ApiStatus { status, .. } if status == StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn test_classify_glm_error_code_1113_as_business_error() {
        let body = r#"{"error":{"code":"1113","message":"coding plan eligibility failed"}}"#;
        let err = classify_api_error(StatusCode::BAD_REQUEST, body, false);
        assert!(matches!(err, LlmError::ApiBusinessError { ref code, .. } if code == "1113"));
        assert!(
            matches!(err, LlmError::ApiBusinessError { ref message, .. } if message == "coding plan eligibility failed")
        );
        assert!(!err.is_retryable());
        assert!(err.is_fatal());
    }

    /// Moonshot phrases a token overflow without any of the words the original
    /// matcher looked for, so this 400 fell through to `ApiStatus` and the turn
    /// died instead of compacting and retrying.
    #[test]
    fn moonshot_token_limit_message_is_a_context_overflow() {
        let body = r#"{"error":{"message":"Your request exceeded model token limit: prompt tokens + max_tokens exceeds the model specification","type":"invalid_request_error"}}"#;
        let err = classify_api_error(StatusCode::BAD_REQUEST, body, false);
        assert!(err.is_context_overflow(), "got {err:?}");
    }

    #[test]
    fn moonshot_input_too_long_message_is_a_context_overflow() {
        let body =
            r#"{"error":{"message":"Input token length too long","type":"invalid_request_error"}}"#;
        assert!(classify_api_error(StatusCode::BAD_REQUEST, body, false).is_context_overflow());
    }

    /// Z.AI code 1261, "Prompt too long".
    #[test]
    fn zai_1261_is_a_context_overflow() {
        let body = r#"{"error":{"code":"1261","message":"Prompt too long"}}"#;
        assert!(classify_api_error(StatusCode::BAD_REQUEST, body, false).is_context_overflow());
    }

    /// A spent balance is fatal. Retrying re-sends a request that cannot
    /// succeed, and both vendors return it on a 429 they share with the
    /// genuinely transient rate-limit errors.
    #[test]
    fn zai_quota_exhaustion_codes_are_fatal_not_retryable() {
        for code in [
            "1113", "1308", "1309", "1310", "1311", "1312", "1313", "1314", "1315", "1316", "1317",
            "1318", "1319", "1320", "1321",
        ] {
            let body =
                format!(r#"{{"error":{{"code":"{code}","message":"Usage limit reached"}}}}"#);
            let err = classify_api_error(StatusCode::TOO_MANY_REQUESTS, &body, true);
            assert!(
                matches!(&err, LlmError::ApiBusinessError { code: c, .. } if c == code),
                "{code} must be a business error, got {err:?}"
            );
            assert!(!err.is_retryable(), "{code} must not be retried");
            assert!(err.is_fatal(), "{code} must be fatal");
        }
    }

    #[test]
    fn moonshot_quota_exhaustion_is_fatal_not_retryable() {
        let body =
            r#"{"error":{"message":"Insufficient balance","type":"exceeded_current_quota_error"}}"#;
        let err = classify_api_error(StatusCode::TOO_MANY_REQUESTS, body, true);
        assert!(matches!(&err, LlmError::ApiBusinessError { .. }), "{err:?}");
        assert!(!err.is_retryable());
        assert!(err.is_fatal());
    }

    /// Transient 429s must stay retryable: z.ai 1302 (rate limit) and 1305
    /// (overloaded), and Moonshot's rate-limit and overload types.
    #[test]
    fn transient_rate_limits_remain_retryable() {
        let cases = [
            r#"{"error":{"code":"1302","message":"Rate limit reached for requests"}}"#,
            r#"{"error":{"code":"1305","message":"The service may be temporarily overloaded"}}"#,
            r#"{"error":{"message":"too many concurrent requests","type":"engine_overloaded_error"}}"#,
            r#"{"error":{"message":"RPM limit exceeded","type":"rate_limit_reached_error"}}"#,
        ];
        for body in cases {
            let err = classify_api_error(StatusCode::TOO_MANY_REQUESTS, body, false);
            assert!(err.is_retryable(), "must stay retryable: {body}");
            assert!(!err.is_fatal(), "must not be fatal: {body}");
        }
    }

    /// A quota code must not be mistaken for an overflow, nor the reverse.
    #[test]
    fn quota_and_overflow_do_not_collide() {
        let quota =
            r#"{"error":{"code":"1113","message":"Insufficient balance or no resource package"}}"#;
        assert!(
            !classify_api_error(StatusCode::TOO_MANY_REQUESTS, quota, true).is_context_overflow()
        );

        let overflow = r#"{"error":{"code":"1261","message":"Prompt too long"}}"#;
        let err = classify_api_error(StatusCode::BAD_REQUEST, overflow, false);
        assert!(!matches!(err, LlmError::ApiBusinessError { .. }));
    }

    /// Z.AI's `finish_reason` enum carries three values Rusta never mapped.
    /// Unmapped they became `None`, and the turn loop read `None` as "keep
    /// going" and span until the turn cap.
    #[test]
    fn zai_finish_reasons_are_mapped() {
        assert_eq!(
            parse_finish_reason(Some("sensitive")),
            Some(FinishReason::ContentFilter)
        );
        assert_eq!(
            parse_finish_reason(Some("model_context_window_exceeded")),
            Some(FinishReason::ContextWindowExceeded)
        );
        assert_eq!(
            parse_finish_reason(Some("network_error")),
            Some(FinishReason::Interrupted)
        );
    }

    /// DeepSeek's two reasons for a generation the server cut short.
    #[test]
    fn deepseek_interruptions_are_mapped() {
        for raw in ["insufficient_system_resource", "aborted"] {
            assert_eq!(
                parse_finish_reason(Some(raw)),
                Some(FinishReason::Interrupted),
                "{raw}"
            );
        }
    }

    #[test]
    fn openai_finish_reasons_still_map() {
        assert_eq!(parse_finish_reason(Some("stop")), Some(FinishReason::Stop));
        assert_eq!(
            parse_finish_reason(Some("length")),
            Some(FinishReason::Length)
        );
        assert_eq!(
            parse_finish_reason(Some("tool_calls")),
            Some(FinishReason::ToolCalls)
        );
        assert_eq!(
            parse_finish_reason(Some("content_filter")),
            Some(FinishReason::ContentFilter)
        );
        assert_eq!(parse_finish_reason(None), None);
        assert_eq!(parse_finish_reason(Some("who_knows")), None);
    }

    /// Only a tool-call turn expects another round trip.
    #[test]
    fn only_tool_calls_expect_a_continuation() {
        assert!(FinishReason::ToolCalls.expects_continuation());
        for fr in [
            FinishReason::Stop,
            FinishReason::Length,
            FinishReason::ContentFilter,
            FinishReason::ContextWindowExceeded,
            FinishReason::Interrupted,
        ] {
            assert!(!fr.expects_continuation(), "{fr:?}");
        }
    }

    /// **A session must not be wedged by switching to a text-only model.**
    ///
    /// Read an image on a vision-capable model and the history holds an
    /// `ImageUrl` part forever. Switch to a text-only one — `/model`, `/provider`,
    /// or resuming the session under a different default — and every subsequent
    /// request carries media the endpoint rejects with a 400. The turn that
    /// wedges is not the one that read the image, so there is nothing to undo
    /// and no way out but abandoning the session.
    #[test]
    fn media_is_degraded_to_text_for_a_model_without_vision() {
        let history = vec![user_with(vec![
            ContentPart::text("what is in this screenshot?"),
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,AAAA".to_string(),
                    id: None,
                },
            },
        ])];

        let out = messages_to_openai(None, &history, ReasoningKey::ReasoningContent, false, false);
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            !json.contains("image_url"),
            "no media may reach a model that cannot accept it: {json}"
        );
        assert!(
            !json.contains("AAAA"),
            "the payload must not be sent either: {json}"
        );
        assert!(
            json.contains("what is in this screenshot?"),
            "the user's words must survive: {json}"
        );
        assert!(
            json.to_lowercase().contains("image"),
            "the model must be told something was dropped, not silently shown less: {json}"
        );
    }

    /// The degradation must not fire on a model that *can* see.
    #[test]
    fn media_is_preserved_for_a_model_with_vision() {
        let history = vec![user_with(vec![ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: "data:image/png;base64,AAAA".to_string(),
                id: None,
            },
        }])];
        let json = serde_json::to_string(&messages_to_openai(
            None,
            &history,
            ReasoningKey::ReasoningContent,
            true,
            false,
        ))
        .unwrap();
        assert!(
            json.contains("image_url") && json.contains("AAAA"),
            "{json}"
        );
    }

    /// A message whose only part was an image must not serialize to an empty
    /// one — "content or tool_calls must be set" is its own 400.
    #[test]
    fn an_image_only_message_does_not_become_empty() {
        let history = vec![user_with(vec![ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: "data:image/png;base64,AAAA".to_string(),
                id: None,
            },
        }])];
        let out = messages_to_openai(None, &history, ReasoningKey::ReasoningContent, false, false);
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            json.contains("image"),
            "an image-only message must still carry text: {json}"
        );
    }
}
