use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};

use crate::{
    errors::LlmError,
    media::{MediaPolicy, VideoSupport},
    message::{ContentPart, Message, Role, ToolCall, ToolCallKind},
    provider::{ChatProvider, GenerateOptions, StreamedMessage},
    tool::Tool,
};

use super::{
    chat_completions_stream::stream_chat_completions,
    openai_common::{OpenAiResponse, OpenAiTool, classify_api_error, messages_to_openai},
    reasoning_key::{ReasoningDialect, ReasoningKey},
    tool_call_id::sanitize_tool_call_id,
};

/// Check whether the provider explicitly asked us to retry the request.
fn should_retry(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get("x-should-retry")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Longest server-requested backoff we will honor. A `Retry-After` of an hour
/// (or a hostile huge value) must not wedge an interactive turn, so it is
/// clamped — beyond this the local exponential backoff is a better bet.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Parse a `Retry-After` response header into a backoff duration.
///
/// The header is either a count of seconds (`Retry-After: 30`) or an HTTP date.
/// The delta-seconds form is what rate-limited APIs send; the rarely-used date
/// form is ignored (returns `None`, falling back to computed backoff). The
/// result is clamped to [`MAX_RETRY_AFTER`].
fn parse_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let raw = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    parse_retry_after_value(raw)
}
/// How long an HTTP request to a model endpoint may take.
///
/// Two independent bounds, because one number cannot serve both jobs on a
/// streaming chat completion. Kept as a named pair so the two can never be
/// swapped at a call site — they are both `Duration`, and transposing them
/// silently produces the opposite policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpTimeouts {
    /// Cap on the whole request, response body included. `None` leaves it
    /// uncapped, which is the right default for streaming: a long answer near a
    /// model's output limit is slow because it is working, not because it is
    /// stuck.
    pub total: Option<Duration>,
    /// How long the connection may go silent. The clock resets on every chunk
    /// received, so this catches a stalled connection — the case that otherwise
    /// hangs with no output and no error — without penalising a healthy stream.
    pub idle: Option<Duration>,
}

impl HttpTimeouts {
    /// An idle bound only, with no cap on total response time.
    #[must_use]
    pub fn idle(idle: Duration) -> Self {
        Self {
            total: None,
            idle: Some(idle),
        }
    }

    /// A total cap only. Rarely what a streaming client wants.
    #[must_use]
    pub fn total(total: Duration) -> Self {
        Self {
            total: Some(total),
            idle: None,
        }
    }
}

/// The delta-seconds parsing behind [`parse_retry_after`], split out so it is
/// testable without a live response.
fn parse_retry_after_value(raw: &str) -> Option<Duration> {
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// Shared OpenAI-compatible HTTP client.
pub struct OpenAiCompatProvider {
    name: String,
    model: String,
    base_url: String,
    api_key: Option<String>,
    default_headers: reqwest::header::HeaderMap,
    client: Client,
    /// The reasoning dialect this endpoint speaks. Shared so a key learned from
    /// one response is used by every request after it, and by the streaming
    /// parser, which detects it chunk by chunk.
    reasoning: Arc<ReasoningDialect>,
    /// Whether this model accepts media, when the wrapping provider knows
    /// better than the name-based guess below. Kimi and GLM both do: their
    /// catalogues say which models are multimodal, and guessing from an
    /// OpenAI-shaped name would answer `false` for every one of them.
    vision: Option<bool>,
    /// Whether stored structured `reasoning_details` arrays are sent back on
    /// their assistant messages. On for a generic OpenAI-compatible endpoint —
    /// the gateways that send the array ask for it back — and switched off by
    /// the Kimi and GLM providers, whose strict schemas reject the field.
    echo_reasoning_details: bool,
    /// Which media this model takes, when the wrapping provider knows (Kimi,
    /// GLM). Otherwise derived from [`supports_vision`](Self::supports_vision).
    media_policy: Option<MediaPolicy>,
    /// Whether a generic endpoint's model takes inline video. Nothing
    /// advertises this, so the operator opts in (`inline_video`).
    inline_video: bool,
    /// Whether messages may carry tool definitions (`messages[].tools`).
    /// Off, they are stripped before the request.
    message_tools: bool,
}

impl OpenAiCompatProvider {
    pub fn new(
        name: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
        timeouts: HttpTimeouts,
    ) -> Result<Self, LlmError> {
        let mut default_headers = reqwest::header::HeaderMap::new();
        default_headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        Ok(Self {
            name: name.into(),
            model: model.into(),
            base_url: base_url.into(),
            api_key,
            default_headers,
            client: {
                let mut builder = crate::http::client_builder();
                if let Some(total) = timeouts.total {
                    builder = builder.timeout(total);
                }
                if let Some(idle) = timeouts.idle {
                    builder = builder.read_timeout(idle);
                }
                builder.build()?
            },
            reasoning: Arc::new(ReasoningDialect::detecting()),
            vision: None,
            echo_reasoning_details: true,
            media_policy: None,
            inline_video: false,
            message_tools: false,
        })
    }

    /// Declare which media this model takes (formats, video), overriding the
    /// policy derived from [`supports_vision`](Self::supports_vision).
    #[must_use]
    pub fn with_media_policy(mut self, policy: MediaPolicy) -> Self {
        self.media_policy = Some(policy);
        self
    }

    /// Let a generic endpoint's vision model receive inline video.
    #[must_use]
    pub fn with_inline_video(mut self, inline_video: bool) -> Self {
        self.inline_video = inline_video;
        self
    }

    /// Send tool definitions carried by messages (`messages[].tools`), for a
    /// model that loads tools mid-conversation. Off by default: the field is
    /// not standard, and other endpoints reject or ignore it.
    #[must_use]
    pub fn with_message_tools(mut self, enabled: bool) -> Self {
        self.message_tools = enabled;
        self
    }

    /// Which media this model takes.
    #[must_use]
    pub fn media_policy(&self) -> MediaPolicy {
        if let Some(policy) = self.media_policy {
            return policy;
        }
        if !self.supports_vision() {
            return MediaPolicy::TEXT_ONLY;
        }
        MediaPolicy {
            image_mimes: crate::media::BASELINE_IMAGE_MIMES,
            video: if self.inline_video {
                VideoSupport::Inline
            } else {
                VideoSupport::Unsupported {
                    hint: "this endpoint is not known to take video; if its model does, set \
                           inline_video = true (RUSTA_INLINE_VIDEO=1)",
                }
            },
            max_image_edge_px: None,
        }
    }

    /// The HTTP client requests go through, with this provider's timeouts.
    #[must_use]
    pub fn http_client(&self) -> &Client {
        &self.client
    }

    /// The API key sent as a bearer token, if any.
    #[must_use]
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// The headers sent with every request, except `Content-Type`, which
    /// differs between JSON requests and uploads.
    #[must_use]
    pub fn extra_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = self.default_headers.clone();
        headers.remove(reqwest::header::CONTENT_TYPE);
        headers
    }

    /// Choose whether stored `reasoning_details` arrays are echoed back.
    #[must_use]
    pub fn with_reasoning_details_echo(mut self, echo: bool) -> Self {
        self.echo_reasoning_details = echo;
        self
    }

    /// Pin the reasoning dialect instead of detecting it.
    ///
    /// For an operator who knows what their gateway speaks — detection needs one
    /// reasoning-bearing response before it can adapt, and a pin skips that.
    #[must_use]
    pub fn with_reasoning_key(mut self, key: Option<ReasoningKey>) -> Self {
        if let Some(key) = key {
            self.reasoning = Arc::new(ReasoningDialect::pinned(key));
        }
        self
    }

    /// The reasoning dialect currently in force.
    #[must_use]
    pub fn reasoning_key(&self) -> ReasoningKey {
        self.reasoning.get()
    }

    /// Send `headers` with every request, below this provider's own: see
    /// [`crate::custom_headers`].
    #[must_use]
    pub fn with_custom_headers(mut self, headers: &reqwest::header::HeaderMap) -> Self {
        crate::custom_headers::add_below(&mut self.default_headers, headers);
        self
    }

    pub fn set_header(&mut self, key: &str, value: &str) -> Result<(), LlmError> {
        let key = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            .map_err(|e| LlmError::Config(e.to_string()))?;
        let value = value
            .parse()
            .map_err(|e| LlmError::Config(format!("{}", e)))?;
        self.default_headers.insert(key, value);
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Return a reasonable context-window size for the configured provider/model.
    pub fn context_window(&self) -> u64 {
        match self.name.as_str() {
            "openai" => match self.model.as_str() {
                "gpt-4o" | "gpt-4o-mini" | "gpt-4-turbo" | "gpt-4-turbo-preview" => 128_000,
                "gpt-4" | "gpt-4-32k" | "gpt-3.5-turbo" | "gpt-3.5-turbo-16k" => 32_768,
                _ => 128_000,
            },
            _ => 128_000,
        }
    }

    /// Whether the configured OpenAI-compatible model supports vision.
    /// Declare this model's media support, overriding the name-based guess.
    #[must_use]
    pub fn with_vision(mut self, supported: bool) -> Self {
        self.vision = Some(supported);
        self
    }

    pub fn supports_vision(&self) -> bool {
        if let Some(known) = self.vision {
            return known;
        }
        let name = self.model_name().to_lowercase();
        name.contains("gpt-4o")
            || name.contains("gpt-4-turbo")
            || name.contains("vision")
            || name.contains("claude-3")
    }

    /// Build the JSON request body. Subclasses can override via the closure.
    pub fn build_request_body(
        &self,
        system_prompt: Option<&str>,
        tools: &[Tool],
        history: &[Message],
        options: &GenerateOptions,
        extra_body: Value,
    ) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": messages_to_openai(
                system_prompt,
                history,
                self.reasoning.get(),
                self.supports_vision(),
                self.echo_reasoning_details,
            ),
            "stream": options.stream,
        });

        if !tools.is_empty() {
            let tools_json: Vec<Value> = tools
                .iter()
                .map(|t| serde_json::to_value(OpenAiTool::from_tool(t)).unwrap())
                .collect();
            body["tools"] = json!(tools_json);
            body["tool_choice"] = json!("auto");
        }

        if let Some(t) = options.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = options.top_p {
            body["top_p"] = json!(p);
        }
        if let Some(m) = options.max_completion_tokens.or(options.max_tokens) {
            body["max_completion_tokens"] = json!(m);
        }
        if let Some(do_sample) = options.do_sample {
            body["do_sample"] = json!(do_sample);
        }
        if options.stream {
            body["stream_options"] = json!({"include_usage": true});
        }

        // Merge extra body (provider-specific overrides).
        if let Value::Object(map) = extra_body {
            for (k, v) in map {
                body[k] = v;
            }
        }

        body
    }

    pub async fn generate(
        &self,
        system_prompt: Option<&str>,
        tools: &[Tool],
        history: &[Message],
        options: GenerateOptions,
        extra_body: Value,
    ) -> Result<StreamedMessage, LlmError> {
        // Tool definitions on messages go only to a model that takes them;
        // a history from one that did outlives a model switch.
        let without_tools;
        let history = if self.message_tools || history.iter().all(|m| m.tools.is_empty()) {
            history
        } else {
            without_tools = strip_message_tools(history);
            &without_tools[..]
        };
        // Media is re-checked against this model on every request: history
        // outlives a model switch, and a rejected request would be resent with
        // the same history forever.
        let prepared;
        let history = if history.iter().any(|m| {
            m.content
                .iter()
                .any(|p| crate::media::media_url(p).is_some())
        }) {
            let mut messages = history.to_vec();
            crate::media::normalize_for_policy(&mut messages, &self.media_policy());
            crate::media::apply_request_media_budget(&mut messages);
            prepared = messages;
            &prepared[..]
        } else {
            history
        };
        let body = self.build_request_body(system_prompt, tools, history, &options, extra_body);
        tracing::debug!(provider = %self.name, model = %self.model, body = %body, "sending chat request");
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut request = self
            .client
            .post(&url)
            .headers(self.default_headers.clone())
            .json(&body);

        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        if options.stream {
            let response = request.send().await?;
            let status = response.status();
            if !status.is_success() {
                let retry_hint = should_retry(&response);
                let retry_after = parse_retry_after(&response);
                let text = response.text().await.unwrap_or_default();
                tracing::debug!(status = %status, body = %text, "chat request failed");
                return Err(
                    classify_api_error(status, &text, retry_hint).with_retry_after(retry_after)
                );
            }
            let stream = response.bytes_stream();
            Ok(stream_chat_completions(stream, self.reasoning.clone()))
        } else {
            let response = request.send().await?;
            let status = response.status();
            let retry_hint = should_retry(&response);
            let retry_after = parse_retry_after(&response);
            let text = response.text().await?;
            if !status.is_success() {
                tracing::debug!(status = %status, body = %text, "chat request failed");
                return Err(
                    classify_api_error(status, &text, retry_hint).with_retry_after(retry_after)
                );
            }
            tracing::debug!(response = %text, "chat response received");
            let parsed: OpenAiResponse = serde_json::from_str(&text)?;
            Ok(self.response_to_streamed(parsed))
        }
    }

    fn response_to_streamed(&self, response: OpenAiResponse) -> StreamedMessage {
        use super::openai_common::{extract_usage, parse_finish_reason};

        let choice = match response.choices.into_iter().next() {
            Some(c) => c,
            None => {
                return StreamedMessage {
                    id: Some(response.id),
                    finish_reason: None,
                    raw_finish_reason: None,
                    usage: None,
                    stream: Box::pin(futures::stream::empty()),
                };
            }
        };

        let msg = choice.message;
        let mut content: Vec<ContentPart> = Vec::new();
        if let Some(text) = msg.content.as_ref().and_then(|c| c.as_str())
            && !text.is_empty()
        {
            content.push(ContentPart::Text {
                text: text.to_string(),
            });
        }
        // Learn the endpoint's reasoning dialect from what it just sent, so the
        // next request echoes thinking under a key it will actually read.
        if let Some((key, think)) = msg.inbound_reasoning() {
            self.reasoning.observe(key);
            if !think.is_empty() {
                content.push(ContentPart::Think {
                    think: think.to_string(),
                });
            }
        } else if let Some(items) = msg.reasoning_detail_items() {
            // Only the structured array came back: surface its readable text.
            let text = super::openai_common::reasoning_details_text(items);
            if !text.is_empty() {
                content.push(ContentPart::Think { think: text });
            }
        }
        if let Some(items) = msg.reasoning_detail_items() {
            content.push(ContentPart::ReasoningDetails {
                details: items.to_vec(),
            });
        }

        let tool_calls: Vec<_> = msg
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| ToolCall {
                id: sanitize_tool_call_id(&tc.id),
                kind: if tc.r#type == "builtin_function" {
                    ToolCallKind::BuiltinFunction
                } else {
                    ToolCallKind::Function
                },
                name: tc.function.name,
                arguments: Some(tc.function.arguments),
                extras: None,
                stream_index: None,
            })
            .collect();

        let assistant_msg = Message {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        };

        let mut parts = Vec::new();
        for part in assistant_msg.content {
            parts.push(crate::message::StreamedMessagePart::Content(part));
        }
        for tc in assistant_msg.tool_calls {
            parts.push(crate::message::StreamedMessagePart::ToolCall(tc));
        }

        StreamedMessage {
            id: Some(response.id),
            finish_reason: parse_finish_reason(choice.finish_reason.as_deref()),
            raw_finish_reason: choice.finish_reason,
            usage: Some(extract_usage(
                choice.usage.as_ref().or(response.usage.as_ref()),
            )),
            stream: Box::pin(futures::stream::iter(parts.into_iter().map(Ok))),
        }
    }
}

#[async_trait::async_trait]
impl ChatProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        self.name()
    }

    fn model_name(&self) -> &str {
        self.model_name()
    }

    fn context_window(&self) -> u64 {
        self.context_window()
    }

    fn supports_vision(&self) -> bool {
        self.supports_vision()
    }

    fn media_policy(&self) -> MediaPolicy {
        self.media_policy()
    }

    async fn generate(
        &self,
        system_prompt: Option<&str>,
        tools: &[Tool],
        history: &[Message],
        options: GenerateOptions,
    ) -> Result<StreamedMessage, LlmError> {
        self.generate(system_prompt, tools, history, options, json!({}))
            .await
    }
}

/// `history` without message-level tool definitions; a message that only
/// carried tools is dropped.
fn strip_message_tools(history: &[Message]) -> Vec<Message> {
    history
        .iter()
        .filter_map(|message| {
            if message.tools.is_empty() {
                return Some(message.clone());
            }
            let mut message = message.clone();
            message.tools.clear();
            let empty = message.tool_calls.is_empty()
                && message.content.iter().all(|part| match part {
                    ContentPart::Text { text } => text.trim().is_empty(),
                    _ => false,
                });
            (!empty).then_some(message)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::message::StreamedMessagePart;

    #[test]
    fn parses_delta_seconds_retry_after() {
        assert_eq!(parse_retry_after_value("30"), Some(Duration::from_secs(30)));
        assert_eq!(
            parse_retry_after_value("  5 "),
            Some(Duration::from_secs(5))
        );
        // Clamped to the ceiling.
        assert_eq!(parse_retry_after_value("100000"), Some(MAX_RETRY_AFTER));
        // The HTTP-date form is not delta-seconds, so it is ignored.
        assert_eq!(
            parse_retry_after_value("Wed, 21 Oct 2026 07:28:00 GMT"),
            None
        );
        assert_eq!(parse_retry_after_value(""), None);
    }

    #[test]
    fn with_retry_after_annotates_only_api_status() {
        let err = classify_api_error(reqwest::StatusCode::TOO_MANY_REQUESTS, "slow down", false)
            .with_retry_after(Some(Duration::from_secs(12)));
        assert_eq!(err.retry_after(), Some(Duration::from_secs(12)));

        // A non-ApiStatus error is left untouched.
        let other = LlmError::ApiTimeout.with_retry_after(Some(Duration::from_secs(12)));
        assert_eq!(other.retry_after(), None);
    }

    /// Strict generic endpoints reject a cap above the model's real output
    /// limit, so nothing is sent unless one is configured.
    #[test]
    fn a_generic_endpoint_sends_no_cap_unless_one_is_set() {
        let provider = OpenAiCompatProvider::new(
            "openai",
            "gpt-4o-mini",
            "https://api.openai.com/v1",
            None,
            HttpTimeouts::idle(Duration::from_secs(5)),
        )
        .unwrap();

        let body =
            provider.build_request_body(None, &[], &[], &GenerateOptions::default(), json!({}));
        assert!(
            body.get("max_completion_tokens").is_none(),
            "no cap configured, none sent: {body}"
        );

        let body = provider.build_request_body(
            None,
            &[],
            &[],
            &GenerateOptions {
                max_tokens: Some(123),
                ..Default::default()
            },
            json!({}),
        );
        assert_eq!(body["max_completion_tokens"], json!(123));
    }

    #[tokio::test]
    async fn test_generate_hits_openai_compat_endpoint() {
        let server = MockServer::start().await;
        let response = json!({
            "id": "chatcmpl-test",
            "choices": [{
                "message": { "role": "assistant", "content": "Hello from wiremock" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(
            "openai",
            "gpt-4o-mini",
            server.uri(),
            None,
            HttpTimeouts::idle(Duration::from_secs(5)),
        )
        .unwrap();

        let streamed = provider
            .generate(
                None,
                &[],
                &[],
                GenerateOptions {
                    stream: false,
                    ..Default::default()
                },
                json!({}),
            )
            .await
            .unwrap();

        let parts: Vec<Result<StreamedMessagePart, LlmError>> = streamed.stream.collect().await;
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            Ok(StreamedMessagePart::Content(ContentPart::Text { text })) => {
                assert_eq!(text, "Hello from wiremock");
            }
            other => panic!("unexpected part: {other:?}"),
        }
    }

    /// **Detection has to change the next request, not just the parse.**
    ///
    /// A vLLM-style endpoint answers under `reasoning` and accepts only
    /// `reasoning` on the request side. The first response teaches the dialect;
    /// the second request must echo the thinking back under that same key, or
    /// Preserved Thinking is silently dropped every turn.
    #[tokio::test]
    async fn a_reasoning_dialect_endpoint_gets_its_own_key_echoed_back() {
        use crate::message::{ContentPart, Message, Role};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "message": {"role": "assistant", "content": "hi", "reasoning": "vllm thinking"},
                    "finish_reason": "stop"
                }]
            })))
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(
            "openai",
            "gpt-oss",
            server.uri(),
            Some("sk-test".to_string()),
            HttpTimeouts::idle(Duration::from_secs(5)),
        )
        .unwrap();
        assert_eq!(provider.reasoning_key(), ReasoningKey::ReasoningContent);

        let streamed = provider
            .generate(
                None,
                &[],
                &[],
                GenerateOptions {
                    stream: false,
                    ..Default::default()
                },
                json!({}),
            )
            .await
            .unwrap();
        let _ = crate::generate::generate(
            streamed,
            crate::provider::GenerateCallbacks::empty(),
            crate::generate::ThinkingOnly::Reject,
        )
        .await;
        assert_eq!(
            provider.reasoning_key(),
            ReasoningKey::Reasoning,
            "the endpoint's dialect must be learned from its response"
        );

        let history = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::Think {
                    think: "vllm thinking".into(),
                },
                ContentPart::text("hi"),
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        }];
        let _ = provider
            .generate(
                None,
                &[],
                &history,
                GenerateOptions {
                    stream: false,
                    ..Default::default()
                },
                json!({}),
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(
            body["messages"][0]["reasoning"],
            json!("vllm thinking"),
            "thinking must be echoed under the endpoint's own key: {body}"
        );
        assert!(
            body["messages"][0].get("reasoning_content").is_none(),
            "the key the endpoint does not read must not be sent: {body}"
        );
    }

    /// Kimi and GLM speak `reasoning_content`, and their wire shape must not move.
    #[tokio::test]
    async fn a_reasoning_content_endpoint_is_left_exactly_as_it_was() {
        use crate::message::{ContentPart, Message, Role};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "message": {"role": "assistant", "content": "hi", "reasoning_content": "kimi thinking"},
                    "finish_reason": "stop"
                }]
            })))
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(
            "kimi",
            "kimi-for-coding",
            server.uri(),
            Some("sk-test".to_string()),
            HttpTimeouts::idle(Duration::from_secs(5)),
        )
        .unwrap();

        let streamed = provider
            .generate(
                None,
                &[],
                &[],
                GenerateOptions {
                    stream: false,
                    ..Default::default()
                },
                json!({}),
            )
            .await
            .unwrap();
        let _ = crate::generate::generate(
            streamed,
            crate::provider::GenerateCallbacks::empty(),
            crate::generate::ThinkingOnly::Reject,
        )
        .await;
        assert_eq!(provider.reasoning_key(), ReasoningKey::ReasoningContent);

        let history = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::Think {
                    think: "kimi thinking".into(),
                },
                ContentPart::text("hi"),
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        }];
        let _ = provider
            .generate(
                None,
                &[],
                &history,
                GenerateOptions {
                    stream: false,
                    ..Default::default()
                },
                json!({}),
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(
            body["messages"][0]["reasoning_content"],
            json!("kimi thinking")
        );
    }
}
