use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::{
    errors::LlmError,
    message::{Message, StreamedMessagePart, ToolCall},
    tool::Tool,
    usage::TokenUsage,
};

/// Reason the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    /// The provider itself detected that the prompt overran the context window.
    /// Z.AI reports this as `model_context_window_exceeded`; it is a successful
    /// HTTP response, not an error body, so it must be recovered from here.
    ContextWindowExceeded,
    /// The provider cut the generation short and said so: DeepSeek's
    /// `insufficient_system_resource` and `aborted`, Z.AI's `network_error`.
    /// What streamed is a fragment, so [`crate::generate::generate`] fails
    /// such a stream with a retryable error instead of returning it.
    Interrupted,
}

impl FinishReason {
    /// Whether the model intends to keep going. Everything else ends the turn:
    /// treating an unrecognized reason as "keep going" spins the loop until the
    /// turn cap.
    #[must_use]
    pub fn expects_continuation(self) -> bool {
        matches!(self, Self::ToolCalls)
    }
}

/// Options passed to `generate()`.
#[derive(Debug, Clone, Default)]
pub struct GenerateOptions {
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Top-p nucleus sampling.
    pub top_p: Option<f32>,
    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,
    /// Maximum completion tokens (OpenAI-compatible alias).
    pub max_completion_tokens: Option<u32>,
    /// Force `do_sample` for GLM-style providers.
    pub do_sample: Option<bool>,
    /// Whether to stream the response.
    pub stream: bool,
    /// Provider-specific extra body fields.
    pub extra_body: Option<serde_json::Value>,
    /// Whether to enable / force reasoning/thinking mode.
    pub thinking: Option<ThinkingConfig>,
    /// Moonshot prompt cache key.
    pub prompt_cache_key: Option<String>,
    /// Reasoning effort level (e.g. `low`, `medium`, `high`).
    pub reasoning_effort: Option<String>,
    /// Whether to keep reasoning/thinking content in the response.
    pub thinking_keep: Option<bool>,
    /// How long a written prompt-cache entry should live, where the
    /// provider bills cache writes by lifetime; `None` takes its default.
    pub prompt_cache_ttl: Option<PromptCacheTtl>,
}

/// How long a provider keeps a written prompt-cache entry: Moonshot's
/// `prompt_cache_options.ttl`. Each hit renews it for as long again, and an
/// entry keeps the lifetime it was first written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheTtl {
    /// Five minutes, Moonshot's default: enough when requests follow each
    /// other closely, as a headless run's steps do.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// An hour, at twice the write price: for gaps past five minutes, such
    /// as a person reading and typing between turns.
    #[serde(rename = "1h")]
    OneHour,
}

impl PromptCacheTtl {
    /// The wire value: `5m` or `1h`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

impl std::str::FromStr for PromptCacheTtl {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_lowercase().as_str() {
            "5m" => Ok(Self::FiveMinutes),
            "1h" => Ok(Self::OneHour),
            other => Err(format!(
                "unknown prompt cache TTL {other:?}: expected 5m or 1h"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    Enabled,
    Disabled,
    /// GLM-style `{ type: "enabled" }`.
    EnabledExplicit,
}

/// A streaming response from a provider.
pub struct StreamedMessage {
    pub id: Option<String>,
    pub finish_reason: Option<FinishReason>,
    pub raw_finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,
    pub stream: Pin<Box<dyn Stream<Item = Result<StreamedMessagePart, LlmError>> + Send>>,
}

impl StreamedMessage {
    pub fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<StreamedMessagePart, LlmError>> + Send + 'static,
    {
        Self {
            id: None,
            finish_reason: None,
            raw_finish_reason: None,
            usage: None,
            stream: Box::pin(stream),
        }
    }
}

/// Callbacks invoked while `generate()` consumes a stream.
#[derive(Default)]
pub struct GenerateCallbacks {
    pub on_message_part: Option<Box<dyn Fn(StreamedMessagePart) + Send + Sync>>,
    pub on_tool_call: Option<Box<dyn Fn(ToolCall) + Send + Sync>>,
}

impl GenerateCallbacks {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn on_message_part<F>(mut self, f: F) -> Self
    where
        F: Fn(StreamedMessagePart) + Send + Sync + 'static,
    {
        self.on_message_part = Some(Box::new(f));
        self
    }

    pub fn on_tool_call<F>(mut self, f: F) -> Self
    where
        F: Fn(ToolCall) + Send + Sync + 'static,
    {
        self.on_tool_call = Some(Box::new(f));
        self
    }
}

/// Metadata for a model known to a provider.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    /// The vendor's name for the model, where its catalogue gives one —
    /// `kimi-for-coding` is "K2.8 Preview".
    pub display_name: Option<String>,
    pub context_window: u64,
    pub supports_vision: bool,
}

/// Abstraction over any LLM provider.
#[async_trait::async_trait]
pub trait ChatProvider: Send + Sync {
    /// Provider name, e.g. "kimi" or "glm".
    fn name(&self) -> &str;

    /// Model identifier as understood by the provider.
    fn model_name(&self) -> &str;

    /// Context-window size in tokens for the configured model.
    fn context_window(&self) -> u64;

    /// Whether the configured model natively supports image/video input.
    fn supports_vision(&self) -> bool {
        false
    }

    /// Which media the configured model takes, and in which forms: the image
    /// formats it accepts and how video reaches it. The default follows
    /// [`supports_vision`](Self::supports_vision) with the baseline formats
    /// and no video.
    fn media_policy(&self) -> crate::media::MediaPolicy {
        if self.supports_vision() {
            crate::media::MediaPolicy::BASELINE_VISION
        } else {
            crate::media::MediaPolicy::TEXT_ONLY
        }
    }

    /// Curated list of models this provider supports.
    fn known_models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    /// The most functions one request may declare, where the endpoint caps it
    /// (Z.AI: 128). `None` when no cap is documented.
    fn max_tools(&self) -> Option<usize> {
        None
    }

    /// Whether the model takes tool definitions loaded mid-conversation on a
    /// system message (`messages[].tools`), so the top-level tool list, and
    /// with it the prompt cache's prefix, stays fixed (Kimi's
    /// `supports_dynamic_tools`).
    fn supports_dynamic_tools(&self) -> bool {
        false
    }

    /// Output tokens the configured model keeps free of the conversation:
    /// proactive compaction runs before the context grows into them, so a
    /// long reply still fits the window. `None`, the default, leaves the
    /// generic trigger at 90 % of the window.
    fn output_reservation(&self) -> Option<u64> {
        None
    }

    /// The completion-token cap to send when the request itself carries no
    /// explicit cap: providers that document an output limit supply one here.
    /// The default sends nothing,
    /// which strict generic endpoints require. `context_window` of `0` means
    /// the window is unknown.
    fn completion_tokens_when_unset(
        &self,
        _input_tokens: u64,
        _context_window: u64,
    ) -> Option<u32> {
        None
    }

    /// Generate a response for the given conversation.
    async fn generate(
        &self,
        system_prompt: Option<&str>,
        tools: &[Tool],
        history: &[Message],
        options: GenerateOptions,
    ) -> Result<StreamedMessage, LlmError>;

    /// Optional provider-native builtin tools (e.g. Kimi `$web_search`).
    /// The CLI/agent can register these alongside regular tools.
    fn builtin_tools(&self) -> Vec<Tool> {
        Vec::new()
    }
}
