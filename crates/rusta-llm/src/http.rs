//! OpenAI-compatible HTTP backend — ADR §6.2.
//!
//! `POST {base_url}/chat/completions` with `stream: true`; SSE parsed from a
//! byte stream. Retries: 3 attempts, exponential backoff 250 ms doubling to a
//! 2 s cap, connection errors and 5xx only. A 404 produces a `base_url`
//! remedy hint; other 4xx surface clipped server detail. Native `tool_calls`
//! deltas are assembled into complete calls. `tools`/`functions` request
//! parameters are never sent (DECIDED, ADR §6.1).

use std::cmp::min;
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::error::{Error, clip_lines};
use crate::types::{ChatRequest, FinishReason, Message, StreamEvent};

/// Attempts per request (ADR §6.2).
const RETRY_ATTEMPTS: u32 = 3;
/// Base retry backoff; doubles per attempt up to [`MAX_BACKOFF`].
const DEFAULT_BACKOFF: Duration = Duration::from_millis(250);
/// Backoff ceiling (ADR §6.2: 250 ms → 2 s).
const MAX_BACKOFF: Duration = Duration::from_secs(2);
/// Upper bound on concurrently assembled native `tool_calls` (§6.1). The
/// index arrives from the server, so it bounds an allocation, not a policy;
/// no real completion carries anywhere near this many.
const MAX_TOOL_CALLS: usize = 64;

/// Lines of server text kept in [`Error::Http`] messages (ADR §6.11).
const SERVER_MESSAGE_LINES: usize = 10;
/// Cap on establishing a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on *starting* to receive a response. Deliberately not a whole-request
/// timeout: a long generation is normal and streams for as long as it needs,
/// but a server that accepts the connection and then says nothing would
/// otherwise hang the turn with no way out but Ctrl-C.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------- wire types

fn is_empty_slice(items: &[String]) -> bool {
    items.is_empty()
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "is_empty_slice")]
    stop: &'a [String],
}

/// One streamed chunk: `{"choices": [{"delta": {...}, "finish_reason": ...}]}`.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCallDelta>,
}

#[derive(Deserialize)]
struct WireToolCallDelta {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireToolFunction>,
}

#[derive(Deserialize, Default)]
struct WireToolFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Non-streaming response.
#[derive(Deserialize)]
struct CompletionResponse {
    #[serde(default)]
    choices: Vec<CompletionChoice>,
}

#[derive(Deserialize)]
struct CompletionChoice {
    #[serde(default)]
    message: CompletionMessage,
}

#[derive(Deserialize, Default)]
struct CompletionMessage {
    #[serde(default)]
    content: Option<String>,
}

// ------------------------------------------------------------- configuration

/// HTTP backend configuration — mirrors `rusta.toml` `[backend]`/`[model]` (ADR §7).
#[derive(Debug, Clone, PartialEq)]
pub struct HttpConfig {
    /// Full API root including `/v1` (ADR §6.2), e.g. `http://127.0.0.1:8080/v1`.
    pub base_url: String,
    /// Model name sent in the request body.
    pub model: String,
    /// Context window in tokens (configured for HTTP; exact when embedded).
    pub context_window: u32,
    /// Sampling budget per completion (ADR §7 default 4096).
    pub max_tokens: u32,
    /// Sampling temperature (ADR §7 default 0.2).
    pub temperature: f32,
    /// Environment variable holding the API key; read once at construction,
    /// never persisted (ADR §7).
    pub api_key_env: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080/v1".to_owned(),
            model: "local-model".to_owned(),
            context_window: 32_768,
            max_tokens: 4096,
            temperature: 0.2,
            api_key_env: None,
        }
    }
}

// ------------------------------------------------------------------- backend

/// The OpenAI-compatible streaming client (ADR §6.2).
#[derive(Debug)]
pub struct HttpBackend {
    url_root: String,
    config: HttpConfig,
    api_key: Option<String>,
    client: reqwest::Client,
    backoff: Duration,
}

/// Accumulates streamed `tool_calls` fragments for one call (ADR §6.2).
#[derive(Debug, Default, Clone)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl HttpBackend {
    /// Validates `config` and builds the backend.
    pub fn new(config: HttpConfig) -> Result<Self, Error> {
        let url_root = config.base_url.trim_end_matches('/').to_owned();
        if !url_root.starts_with("http://") && !url_root.starts_with("https://") {
            return Err(Error::Config {
                cause: format!(
                    "`base_url` must be the full API root starting with http:// or \
                     https://, including /v1 (got `{}`)",
                    config.base_url
                ),
            });
        }
        let api_key = config
            .api_key_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok())
            .filter(|key| !key.is_empty());
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(RESPONSE_TIMEOUT)
            .build()
            .map_err(|e| Error::Config {
                cause: e.to_string(),
            })?;
        Ok(Self {
            url_root,
            config,
            api_key,
            client,
            backoff: DEFAULT_BACKOFF,
        })
    }

    /// Builder hook: shortens retry backoff (used by tests for speed).
    pub fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// The effective configuration.
    pub fn config(&self) -> &HttpConfig {
        &self.config
    }

    /// Context window in tokens.
    pub fn context_window(&self) -> u64 {
        u64::from(self.config.context_window)
    }

    /// Heuristic token count for `text` (ADR §6.2: `ceil(chars / 3)`).
    pub fn count_tokens(&self, text: &str) -> u64 {
        crate::tokens::estimate_tokens(text)
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.url_root)
    }

    fn request_body<'a>(&'a self, request: &'a ChatRequest, stream: bool) -> WireRequest<'a> {
        WireRequest {
            model: &self.config.model,
            messages: &request.messages,
            stream,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stop: &request.stop,
        }
    }

    /// POSTs with retry: 3 attempts, exponential backoff (250 ms doubling to a
    /// 2 s cap) on connection errors and 5xx only; 404 yields a `base_url`
    /// remedy; other 4xx surface clipped detail (ADR §6.2, §6.11).
    ///
    /// The retry scope is deliberate: only *establishment* failures
    /// (`err.is_connect()`) replay — the request provably never reached the
    /// server, so resending is safe. A failure once the connection was up
    /// (timeout mid-body, TLS reset, DNS drop) fails fast as `Unreachable`
    /// with a remedy instead of blindly replaying a request that may
    /// already be in flight server-side.
    async fn post_with_retry(&self, body: &WireRequest<'_>) -> Result<reqwest::Response, Error> {
        let url = self.endpoint();
        let mut last_cause = String::from("connection error");
        for attempt in 1..=RETRY_ATTEMPTS {
            if attempt > 1 {
                let shift = min(attempt - 2, 20);
                let delay = min(self.backoff * (1u32 << shift), MAX_BACKOFF);
                tokio::time::sleep(delay).await;
            }
            let mut request = self.client.post(&url).json(body);
            if let Some(key) = &self.api_key {
                request = request.bearer_auth(key);
            }
            match request.send().await {
                Err(e) if e.is_connect() => {
                    last_cause = e.to_string();
                }
                Err(e) => {
                    return Err(Error::Unreachable {
                        attempts: attempt,
                        cause: e.to_string(),
                    });
                }
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(response);
                    }
                    if status.as_u16() == 404 {
                        return Err(Error::BaseUrl404 { url });
                    }
                    if status.is_server_error() {
                        last_cause = format!("HTTP {}", status.as_u16());
                        continue;
                    }
                    let detail = response.text().await.unwrap_or_default();
                    return Err(Error::Http {
                        status: status.as_u16(),
                        message: clip_lines(&detail, SERVER_MESSAGE_LINES),
                    });
                }
            }
        }
        Err(Error::Unreachable {
            attempts: RETRY_ATTEMPTS,
            cause: last_cause,
        })
    }

    /// Starts a streaming completion; returns the event channel (ADR §6.2).
    ///
    /// Establishment failures (connection refused, 5xx×3, 404, other 4xx) are
    /// returned here. Failures *after* the stream has begun are delivered as a
    /// final [`StreamEvent::Failed`] — retries never re-run mid-stream.
    pub async fn stream(&self, request: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>, Error> {
        let body = self.request_body(&request, true);
        let response = self.post_with_retry(&body).await?;
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            pump_stream(response, tx).await;
        });
        Ok(rx)
    }

    /// Non-streaming completion — summaries and sub-coder wrap-ups (ADR §6.2).
    pub async fn complete(&self, request: ChatRequest) -> Result<String, Error> {
        let body = self.request_body(&request, false);
        let response = self.post_with_retry(&body).await?;
        let parsed: CompletionResponse = response.json().await.map_err(|e| Error::Malformed {
            cause: e.to_string(),
        })?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .ok_or_else(|| Error::Malformed {
                cause: "response carried no choices/content".to_owned(),
            })
    }
}

// ------------------------------------------------------------------ SSE pump

/// Consumes the response stream, forwarding events to `tx`; on failure sends a
/// final [`StreamEvent::Failed`] with a remedy text (ADR §6.11).
async fn pump_stream(response: reqwest::Response, mut tx: mpsc::Sender<StreamEvent>) {
    if let Err(error) = pump_events(response, &mut tx).await {
        let _ = tx.send(StreamEvent::Failed(error.to_string())).await;
    }
}

async fn pump_events(
    response: reqwest::Response,
    tx: &mut mpsc::Sender<StreamEvent>,
) -> Result<(), Error> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut tools: Vec<ToolCallAccumulator> = Vec::new();
    let mut finish: Option<FinishReason> = None;
    // A28: an SSE stream has two legitimate terminators — a `finish_reason`
    // on a choice, or `data: [DONE]`. Seeing neither means the connection
    // ended mid-generation. Tracking this is the whole fix: without it the
    // `unwrap_or(Stop)` below reported a dropped connection as a clean stop,
    // and the agent committed the partial text as the turn's answer.
    let mut saw_done = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::StreamFailed {
            cause: e.to_string(),
        })?;
        buffer.extend_from_slice(&chunk);
        // SSE permits \r\n line endings; drop \r so events always terminate in
        // a bare \n\n (UTF-8 continuation bytes are never 0x0D, so this is safe).
        buffer.retain(|&byte| byte != b'\r');
        while let Some(raw_event) = take_sse_event(&mut buffer) {
            let event = String::from_utf8_lossy(&raw_event).into_owned();
            let Some(data) = sse_data(&event) else {
                continue; // comment / keep-alive (`: ping`)
            };
            if data == "[DONE]" {
                saw_done = true;
                continue;
            }
            let parsed: StreamChunk =
                serde_json::from_str(&data).map_err(|e| Error::Malformed {
                    cause: format!("{e} in {}", clip_lines(&data, 3)),
                })?;
            let Some(choice) = parsed.choices.into_iter().next() else {
                continue; // empty-choices ping chunks (vLLM)
            };
            if let Some(text) = choice.delta.content {
                if !text.is_empty() && !forward(tx, StreamEvent::Delta(text)).await {
                    return Ok(()); // consumer dropped the channel
                }
            }
            for call in choice.delta.tool_calls {
                let index = call.index as usize;
                // The index is wire-supplied. Resizing to an unbounded `u32`
                // asks for ~309 GB in one contiguous allocation on
                // `"index": 4294967295` — an abort, not a recoverable error,
                // against §6.1's never-panic-on-malformed-input posture.
                if index >= MAX_TOOL_CALLS {
                    return Err(Error::Malformed {
                        cause: format!(
                            "tool_call index {index} exceeds the {MAX_TOOL_CALLS}-call limit"
                        ),
                    });
                }
                if index >= tools.len() {
                    tools.resize(index + 1, ToolCallAccumulator::default());
                }
                let acc = &mut tools[index];
                if acc.id.is_none() {
                    acc.id = call.id;
                }
                if let Some(function) = call.function {
                    if acc.name.is_empty() {
                        acc.name = function.name.unwrap_or_default();
                    }
                    acc.arguments
                        .push_str(&function.arguments.unwrap_or_default());
                }
            }
            if let Some(reason) = choice.finish_reason {
                finish = Some(FinishReason::parse(&reason));
            }
        }
    }
    // Stream ended: flush fully assembled tool calls, then the finish event.
    for acc in tools {
        if acc.name.is_empty() {
            continue; // fragments never assembled into a call
        }
        if !forward(
            tx,
            StreamEvent::ToolCall {
                id: acc.id,
                name: acc.name,
                arguments: acc.arguments,
            },
        )
        .await
        {
            return Ok(());
        }
    }
    // A28: truncation is reported, never rounded up to a clean stop. The
    // check precedes the tool-call flush above only in intent — a stream cut
    // mid-generation may also have left a tool call half-assembled, and the
    // consumer discards the whole turn either way (`abandon_turn`).
    //
    // Deliberately narrow: a server that omits `[DONE]` but does send a
    // `finish_reason` is still treated as complete, because it said so. Only
    // the case where *neither* terminator arrived is a failure, which is the
    // one genuinely indistinguishable from a dropped connection.
    if finish.is_none() && !saw_done {
        let _ = tx
            .send(StreamEvent::Failed(
                "the server closed the stream without a finish reason or \
                 [DONE] — the reply is truncated, not complete; retry the \
                 request, and check the server's logs for an out-of-memory \
                 or context-overflow kill"
                    .to_owned(),
            ))
            .await;
        return Ok(());
    }
    let _ = tx
        .send(StreamEvent::Finish(finish.unwrap_or(FinishReason::Stop)))
        .await;
    Ok(())
}

/// Sends `event`; `false` when the consumer has dropped the channel.
async fn forward(tx: &mpsc::Sender<StreamEvent>, event: StreamEvent) -> bool {
    tx.send(event).await.is_ok()
}

/// Removes and returns the next complete SSE event (blank-line terminated) from
/// `buffer`, consuming the blank line. Byte-level on purpose: multibyte UTF-8
/// split across chunks stays buffered until its event completes.
fn take_sse_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let end = buffer.windows(2).position(|pair| pair == b"\n\n")?;
    Some(buffer.drain(..end + 2).collect())
}

/// Extracts the joined `data:` payload of one SSE event; `None` for comments
/// (`: ping`) and non-data events. Multiple `data:` lines join with `\n` (SSE).
fn sse_data(event: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            parts.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_base_url_with_remedy() {
        let error = HttpBackend::new(HttpConfig {
            base_url: "127.0.0.1:8080/v1".to_owned(),
            ..HttpConfig::default()
        })
        .unwrap_err();
        assert!(error.to_string().contains("base_url"), "{error}");
    }

    #[test]
    fn take_sse_event_consumes_the_terminating_blank_line() {
        let mut buffer = b"first\n\nseco".to_vec();
        assert_eq!(take_sse_event(&mut buffer).unwrap(), b"first\n\n".to_vec());
        assert_eq!(buffer, b"seco".to_vec());
        assert_eq!(take_sse_event(&mut buffer), None);
    }

    #[test]
    fn sse_data_ignores_comments_and_joins_data_lines() {
        assert_eq!(
            sse_data(": ping\n\ndata: one\ndata: two\n\n").as_deref(),
            Some("one\ntwo")
        );
        assert_eq!(sse_data(": keep-alive\n\n"), None);
        assert_eq!(sse_data("event: x\ndata: {}\n\n").as_deref(), Some("{}"));
    }
}
