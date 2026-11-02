use std::sync::Arc;

use async_trait::async_trait;
use lm_bridge::{
    LlmError,
    generate::{ThinkingOnly, generate},
    message::{ContentPart, Message, StreamedMessagePart, ToolCall},
    provider::{ChatProvider, GenerateCallbacks, GenerateOptions, ThinkingConfig},
    tool::Tool,
};
use tokio_util::sync::CancellationToken;

use crate::completion_budget;

use super::types::StepResult;

/// Type alias for a streaming text/thinking callback.
pub type TextDeltaFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Type alias for a streaming tool-call delta callback.
pub type ToolCallDeltaFn = Arc<dyn Fn(&ToolCall) + Send + Sync>;
/// Type alias for the callback announcing that a model call will be retried.
pub type RetryFn = Arc<dyn Fn(&RetryNotice) + Send + Sync>;
/// Type alias for the callback reporting context compaction.
pub type CompactionFn = Arc<dyn Fn(&CompactionNotice) + Send + Sync>;

/// What prompted a context compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// The context crossed the proactive threshold (90 % of the window).
    Proactive,
    /// The provider rejected a request as too large for the window.
    Overflow,
    /// The user asked for it (`/compact`).
    Manual,
}

/// Progress of a context compaction, for front-ends to show: summarizing costs
/// a model call, and the history the model sees changes under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionNotice {
    /// A compaction began, with the context at roughly `tokens`.
    Started {
        trigger: CompactionTrigger,
        tokens: u64,
    },
    /// The history was replaced by a summary.
    Summarized {
        tokens_before: u64,
        tokens_after: u64,
    },
    /// No summary could be made, so the oldest messages were removed instead
    /// (the first and latest user messages are always kept).
    Evicted {
        removed: usize,
        tokens_before: u64,
        tokens_after: u64,
        reason: String,
    },
    /// Nothing could be summarized or removed.
    Unchanged { reason: String },
}

impl CompactionNotice {
    /// A one-line human-readable description.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Started { trigger, tokens } => {
                let why = match trigger {
                    CompactionTrigger::Proactive => "the context is nearly full",
                    CompactionTrigger::Overflow => "the provider reported a context overrun",
                    CompactionTrigger::Manual => "requested",
                };
                format!("compacting context (~{tokens} tokens; {why})…")
            }
            Self::Summarized {
                tokens_before,
                tokens_after,
            } => format!("context summarized: ~{tokens_before} → ~{tokens_after} tokens"),
            Self::Evicted {
                removed,
                tokens_before,
                tokens_after,
                reason,
            } => format!(
                "could not summarize ({reason}); removed {removed} older messages instead: \
                 ~{tokens_before} → ~{tokens_after} tokens"
            ),
            Self::Unchanged { reason } => format!("context left as is: {reason}"),
        }
    }
}

/// A model call failed and is about to be retried.
///
/// Whatever the failed attempt already streamed — text, reasoning, tool-call
/// headers — has reached the callbacks and is now superseded: the retry
/// re-requests the whole step from scratch. A front-end uses this notice to mark
/// that partial output as discarded.
#[derive(Debug, Clone)]
pub struct RetryNotice {
    /// The attempt that just failed, counting from 1.
    pub attempt: u32,
    /// The attempt budget, or `None` when retrying without limit.
    pub max_attempts: Option<u32>,
    /// How long until the next attempt.
    pub delay: std::time::Duration,
    /// Why the attempt failed.
    pub error: String,
}

impl RetryNotice {
    /// A one-line human-readable description of the retry.
    #[must_use]
    pub fn describe(&self) -> String {
        let budget = match self.max_attempts {
            Some(max) => format!("attempt {} of {max} failed", self.attempt),
            None => format!("attempt {} failed", self.attempt),
        };
        format!(
            "{budget}: {}; retrying in {:.1}s (output above is discarded)",
            self.error,
            self.delay.as_secs_f64()
        )
    }
}

/// Callbacks emitted by the LLM during streaming.
#[derive(Default, Clone)]
pub struct LlmCallbacks {
    pub on_text_delta: Option<TextDeltaFn>,
    pub on_think_delta: Option<TextDeltaFn>,
    pub on_tool_call_delta: Option<ToolCallDeltaFn>,
    /// Called before each retry of a failed model call.
    pub on_retry: Option<RetryFn>,
    /// Called when the context is compacted, before and after.
    pub on_compaction: Option<CompactionFn>,
}

/// Agent-facing LLM contract.
#[async_trait]
pub trait Llm: Send + Sync {
    fn model_name(&self) -> &str;

    fn is_retryable_error(&self, error: &LlmError) -> bool {
        error.is_retryable()
    }

    fn supports_vision(&self) -> bool {
        false
    }

    /// Which media the model takes: the image formats it accepts and how video
    /// reaches it. The default follows
    /// [`supports_vision`](Self::supports_vision), with the baseline formats
    /// and no video.
    fn media_policy(&self) -> lm_bridge::media::MediaPolicy {
        if self.supports_vision() {
            lm_bridge::media::MediaPolicy::BASELINE_VISION
        } else {
            lm_bridge::media::MediaPolicy::TEXT_ONLY
        }
    }

    /// The model's context window in tokens, when known. Sub-agents are sized
    /// to it; `None` leaves them on the conservative default.
    fn context_limit(&self) -> Option<u64> {
        None
    }

    /// Output tokens the model keeps free of the conversation, which the
    /// context budget compacts ahead of. See
    /// [`lm_bridge::provider::ChatProvider::output_reservation`].
    fn output_reservation(&self) -> Option<u64> {
        None
    }

    /// The most tools one request may declare, where the endpoint caps it.
    /// The tool table is trimmed to it, and `select_tools` loads nothing past
    /// it.
    fn max_tools(&self) -> Option<usize> {
        None
    }

    /// Whether the model takes tools loaded mid-conversation on a system
    /// message, so the top-level tool list stays fixed. See
    /// [`lm_bridge::provider::ChatProvider::supports_dynamic_tools`].
    fn supports_dynamic_tools(&self) -> bool {
        false
    }

    /// One step of a conversation. A reply that finished normally with
    /// reasoning alone ([`Message::is_thinking_only`]) is returned as it is,
    /// for the turn to ask for the answer; a caller that needs the text checks
    /// for it.
    async fn chat(
        &self,
        messages: Vec<Message>,
        tools: Vec<Tool>,
        callbacks: LlmCallbacks,
        cancel: &CancellationToken,
    ) -> Result<StepResult, LlmError>;

    /// Ask for a compaction summary: `messages` ends with the handoff
    /// instruction, and the answer's text is the summary. A reply of
    /// reasoning alone fails as an empty response.
    ///
    /// `tools` is the conversation's own tool table. The instruction forbids
    /// calling tools, but declaring the same ones keeps the request's prefix
    /// identical to the conversation's, so a provider-side prompt cache still
    /// applies. `max_output_tokens` caps the reply where the implementation
    /// supports it; the default implementation ignores it. Nothing is streamed
    /// to the front-end.
    async fn summarize(
        &self,
        messages: Vec<Message>,
        tools: Vec<Tool>,
        _max_output_tokens: Option<u32>,
        cancel: &CancellationToken,
    ) -> Result<StepResult, LlmError> {
        self.chat(messages, tools, LlmCallbacks::default(), cancel)
            .await
    }
}

/// Adapter from `lm_bridge::ChatProvider` to agent `Llm`.
pub struct ProviderLlm {
    provider: Box<dyn ChatProvider>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    thinking: Option<ThinkingConfig>,
    prompt_cache_key: Option<String>,
    reasoning_effort: Option<String>,
    thinking_keep: Option<bool>,
    max_tokens: Option<u32>,
    prompt_cache_ttl: Option<lm_bridge::provider::PromptCacheTtl>,
    completion_budget: completion_budget::CompletionBudget,
}

impl ProviderLlm {
    pub fn new(provider: Box<dyn ChatProvider>) -> Self {
        Self {
            provider,
            temperature: None,
            top_p: None,
            thinking: None,
            prompt_cache_key: None,
            reasoning_effort: None,
            thinking_keep: None,
            max_tokens: None,
            prompt_cache_ttl: None,
            completion_budget: completion_budget::CompletionBudget::resolve(None),
        }
    }

    /// Set the sampling temperature.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Set the nucleus-sampling top-p value.
    pub fn with_top_p(mut self, top_p: f32) -> Self {
        self.top_p = Some(top_p);
        self
    }

    /// Set the standalone thinking/reasoning configuration.
    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// Set a default `prompt_cache_key` to use when the caller does not
    /// override it (e.g. derived from the session id).
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(key.into());
        self
    }

    /// Set the reasoning-effort level: `none`, `minimal`, `low`, `medium`,
    /// `high`, `xhigh`, or `max`. Providers drop it for models that do not
    /// accept it.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Set whether to keep reasoning/thinking content in the response.
    pub fn with_thinking_keep(mut self, keep: bool) -> Self {
        self.thinking_keep = Some(keep);
        self
    }

    /// Set the maximum number of tokens to generate.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set how long a written prompt-cache entry should live, for a provider
    /// that bills cache writes by lifetime.
    pub fn with_prompt_cache_ttl(mut self, ttl: lm_bridge::provider::PromptCacheTtl) -> Self {
        self.prompt_cache_ttl = Some(ttl);
        self
    }

    /// Context-window size in tokens for the configured model.
    pub fn context_window(&self) -> u64 {
        self.provider.context_window()
    }

    /// The provider's name, e.g. `kimi` or `glm`.
    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    /// The models the provider knows: its live catalogue where it has one,
    /// otherwise its built-in table.
    pub fn known_models(&self) -> Vec<lm_bridge::provider::ModelInfo> {
        self.provider.known_models()
    }
}

#[async_trait]
impl Llm for ProviderLlm {
    fn model_name(&self) -> &str {
        self.provider.model_name()
    }

    fn supports_vision(&self) -> bool {
        self.provider.supports_vision()
    }

    fn media_policy(&self) -> lm_bridge::media::MediaPolicy {
        self.provider.media_policy()
    }

    fn context_limit(&self) -> Option<u64> {
        Some(self.provider.context_window())
    }

    fn output_reservation(&self) -> Option<u64> {
        self.provider.output_reservation()
    }

    fn max_tools(&self) -> Option<usize> {
        self.provider.max_tools()
    }

    fn supports_dynamic_tools(&self) -> bool {
        self.provider.supports_dynamic_tools()
    }

    async fn chat(
        &self,
        messages: Vec<Message>,
        tools: Vec<Tool>,
        callbacks: LlmCallbacks,
        cancel: &CancellationToken,
    ) -> Result<StepResult, LlmError> {
        self.chat_with_options(
            messages,
            tools,
            callbacks,
            cancel,
            None,
            ThinkingOnly::AcceptCompleted,
        )
        .await
    }

    async fn summarize(
        &self,
        messages: Vec<Message>,
        tools: Vec<Tool>,
        max_output_tokens: Option<u32>,
        cancel: &CancellationToken,
    ) -> Result<StepResult, LlmError> {
        self.chat_with_options(
            messages,
            tools,
            LlmCallbacks::default(),
            cancel,
            max_output_tokens,
            ThinkingOnly::Reject,
        )
        .await
    }
}

impl ProviderLlm {
    async fn chat_with_options(
        &self,
        mut messages: Vec<Message>,
        tools: Vec<Tool>,
        callbacks: LlmCallbacks,
        cancel: &CancellationToken,
        max_tokens_override: Option<u32>,
        thinking_only: ThinkingOnly,
    ) -> Result<StepResult, LlmError> {
        if !self.supports_vision() {
            messages = downgrade_unsupported_media(messages);
        }

        let mut options = GenerateOptions {
            stream: true,
            temperature: self.temperature.or_else(env_temperature),
            top_p: self.top_p.or_else(env_top_p),
            thinking: self.thinking.clone(),
            prompt_cache_key: self.prompt_cache_key.clone(),
            reasoning_effort: self.reasoning_effort.clone().or_else(env_thinking_effort),
            thinking_keep: self.thinking_keep.or_else(env_thinking_keep),
            max_tokens: max_tokens_override.or(self.max_tokens),
            prompt_cache_ttl: self.prompt_cache_ttl,
            ..Default::default()
        };

        let input_tokens = completion_budget::estimate_input_tokens(&messages, &tools);
        let context_window = self.context_window();
        options.max_tokens = completion_budget::clamp_max_tokens(
            options.max_tokens,
            &self.completion_budget,
            input_tokens,
            context_window,
            self.provider
                .completion_tokens_when_unset(input_tokens, context_window),
        );

        let mut bridge_callbacks = GenerateCallbacks::empty();
        let on_text = callbacks.on_text_delta.clone();
        let on_think = callbacks.on_think_delta.clone();
        let on_tool = callbacks.on_tool_call_delta.clone();
        if on_text.is_some() || on_think.is_some() || on_tool.is_some() {
            bridge_callbacks = bridge_callbacks.on_message_part(move |part| match part {
                StreamedMessagePart::Content(ContentPart::Text { text }) => {
                    if let Some(cb) = &on_text {
                        cb(&text);
                    }
                }
                StreamedMessagePart::Content(ContentPart::Think { think }) => {
                    if let Some(cb) = &on_think {
                        cb(&think);
                    }
                }
                StreamedMessagePart::ToolCall(tc) => {
                    if let Some(cb) = &on_tool {
                        cb(&tc);
                    }
                }
                _ => {}
            });
        }

        let streamed = tokio::select! {
            res = self.provider.generate(None, &tools, &messages, options) => res?,
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
        };

        let result = generate(streamed, bridge_callbacks, thinking_only).await?;

        Ok(StepResult {
            message: result.message,
            usage: result.usage.unwrap_or_default(),
            finish_reason: result.finish_reason,
            response_id: result.id,
            stop_reason: None,
        })
    }
}

/// Media replaced with notes, for a model that takes none.
///
/// The note names a web address, which the model can still fetch, but never
/// an inline `data:` payload — that would paste the whole file into the
/// prompt as text — nor an `ms://` id, which means nothing to the model.
fn downgrade_unsupported_media(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|mut message| {
            for part in &mut message.content {
                let Some(url) = lm_bridge::media::media_url(part) else {
                    continue;
                };
                let kind = lm_bridge::media::media_kind(part);
                let source = if url.starts_with("http://") || url.starts_with("https://") {
                    format!(" ({url})")
                } else {
                    String::new()
                };
                *part = ContentPart::text(format!(
                    "[{kind} omitted{source}: the current model does not take {kind} input]"
                ));
            }
            message
        })
        .collect()
}

fn env_temperature() -> Option<f32> {
    std::env::var("KIMI_MODEL_TEMPERATURE")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&v| (0.0..=2.0).contains(&v))
}

fn env_top_p() -> Option<f32> {
    std::env::var("KIMI_MODEL_TOP_P")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&v| (0.0..=1.0).contains(&v))
}

fn env_thinking_effort() -> Option<String> {
    std::env::var("KIMI_MODEL_THINKING_EFFORT")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Parses a `KIMI_MODEL_THINKING_KEEP` value.
///
/// `None` for an unrecognized value, so a typo falls through to the model's
/// default rather than silently switching Preserved Thinking off — which is
/// what a bare truthiness test would do.
fn parse_thinking_keep(raw: &str) -> Option<bool> {
    match raw.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "all" => Some(true),
        "0" | "false" | "no" | "off" | "none" => Some(false),
        other => {
            tracing::warn!(
                "Ignoring KIMI_MODEL_THINKING_KEEP={other:?}: expected on/off, \
                 true/false, yes/no, all/none, or 1/0"
            );
            None
        }
    }
}

/// Reads `KIMI_MODEL_THINKING_KEEP`. `None` when unset or unrecognized.
pub fn env_thinking_keep() -> Option<bool> {
    parse_thinking_keep(&std::env::var("KIMI_MODEL_THINKING_KEEP").ok()?)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::stream;

    use super::*;
    use lm_bridge::{
        message::StreamedMessagePart,
        provider::{FinishReason, StreamedMessage},
        usage::TokenUsage,
    };

    struct RecordingProvider {
        captured: Arc<Mutex<Option<GenerateOptions>>>,
    }

    #[async_trait]
    impl ChatProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }

        fn model_name(&self) -> &str {
            "recording-model"
        }

        fn context_window(&self) -> u64 {
            4096
        }

        async fn generate(
            &self,
            _system_prompt: Option<&str>,
            _tools: &[Tool],
            _history: &[Message],
            options: GenerateOptions,
        ) -> Result<StreamedMessage, LlmError> {
            *self.captured.lock().unwrap() = Some(options);
            let parts = vec![Ok(StreamedMessagePart::Content(ContentPart::Text {
                text: "ok".to_string(),
            }))];
            Ok(StreamedMessage {
                id: None,
                finish_reason: Some(FinishReason::Stop),
                raw_finish_reason: Some("stop".to_string()),
                usage: Some(TokenUsage::empty()),
                stream: Box::pin(stream::iter(parts)),
            })
        }
    }

    /// A provider whose every reply is reasoning alone, finished normally.
    struct ThinkingProvider;

    #[async_trait]
    impl ChatProvider for ThinkingProvider {
        fn name(&self) -> &str {
            "thinking"
        }

        fn model_name(&self) -> &str {
            "thinking-model"
        }

        fn context_window(&self) -> u64 {
            4096
        }

        async fn generate(
            &self,
            _system_prompt: Option<&str>,
            _tools: &[Tool],
            _history: &[Message],
            _options: GenerateOptions,
        ) -> Result<StreamedMessage, LlmError> {
            let parts = vec![Ok(StreamedMessagePart::Content(ContentPart::think(
                "Nothing to add.",
            )))];
            Ok(StreamedMessage {
                id: None,
                finish_reason: Some(FinishReason::Stop),
                raw_finish_reason: Some("stop".to_string()),
                usage: Some(TokenUsage::empty()),
                stream: Box::pin(stream::iter(parts)),
            })
        }
    }

    /// A conversation step takes a reply of reasoning alone, for the turn to
    /// ask for the answer; a summary, which is its text, does not.
    #[tokio::test]
    async fn a_step_takes_reasoning_alone_and_a_summary_does_not() {
        let llm = ProviderLlm::new(Box::new(ThinkingProvider));
        let cancel = CancellationToken::new();
        let step = llm
            .chat(
                vec![Message::user("hi")],
                Vec::new(),
                LlmCallbacks::default(),
                &cancel,
            )
            .await
            .expect("a step of reasoning alone");
        assert!(step.message.is_thinking_only());
        let err = llm
            .summarize(vec![Message::user("summarize")], Vec::new(), None, &cancel)
            .await
            .expect_err("a summary needs text");
        assert!(
            matches!(
                err,
                LlmError::ApiEmptyResponse {
                    think_only: true,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn test_prompt_cache_key_defaults_from_session() {
        let captured = Arc::new(Mutex::new(None));
        let provider = Box::new(RecordingProvider {
            captured: captured.clone(),
        });
        let llm = ProviderLlm::new(provider).with_prompt_cache_key("session-42");
        let cancel = CancellationToken::new();
        let result = llm
            .chat(
                vec![Message::user("hi")],
                vec![],
                LlmCallbacks::default(),
                &cancel,
            )
            .await;
        assert!(result.is_ok());
        let options = captured.lock().unwrap().take().expect("options captured");
        assert_eq!(options.prompt_cache_key.as_deref(), Some("session-42"));
    }

    /// Preserved Thinking now defaults on for Kimi models, so the env var's job
    /// is as much to *disable* it as to enable it. A bare truthiness test would
    /// read `off` as false and `banana` as false alike; only the first is meant.
    #[test]
    fn parse_thinking_keep_accepts_on_and_off_values() {
        for on in ["1", "true", "yes", "on", "all", "ON", " True "] {
            assert_eq!(super::parse_thinking_keep(on), Some(true), "{on:?}");
        }
        for off in ["0", "false", "no", "off", "none", "OFF", " false "] {
            assert_eq!(super::parse_thinking_keep(off), Some(false), "{off:?}");
        }
    }

    /// An unrecognized value must defer to the model default, not silently
    /// disable Preserved Thinking.
    #[test]
    fn parse_thinking_keep_ignores_garbage() {
        for bad in ["banana", "", "2", "enabled"] {
            assert_eq!(super::parse_thinking_keep(bad), None, "{bad:?}");
        }
    }
}
