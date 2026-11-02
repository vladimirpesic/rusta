use reqwest::StatusCode;

/// Errors that can occur inside the LLM bridge.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("SSE stream failed: {0}")]
    EventSource(String),

    #[error("JSON (de)serialization failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("API returned {status}: {message}")]
    ApiStatus {
        status: StatusCode,
        message: String,
        /// Provider explicitly asked us to retry (e.g. `x-should-retry: true`).
        retry_hint: bool,
        /// Server-requested backoff parsed from the `Retry-After` response
        /// header. When present, the retry loop honors it in preference to its
        /// own exponential backoff — a `Retry-After` directive is the server
        /// telling us exactly how long to wait.
        retry_after: Option<std::time::Duration>,
    },

    #[error("API connection error: {0}")]
    ApiConnection(String),

    #[error("API timeout")]
    ApiTimeout,

    /// The provider returned nothing usable.
    ///
    /// `filtered` separates the two reasons this happens, because they want
    /// opposite handling. A transient empty stream is worth another attempt. A
    /// safety-filter refusal is deterministic — replaying the identical request
    /// re-triggers the identical filter — so retrying it only delays the notice
    /// by the whole backoff ramp, ten attempts deep.
    ///
    /// `think_only` marks a reply that carried reasoning but neither answer
    /// text nor tool calls — typically the output budget ran out mid-thought,
    /// or the stream was cut short. It is not an answer, so it is retried like
    /// any other empty reply. One that finished normally is no error for an
    /// agent's turn, which asks for the answer instead
    /// ([`crate::generate::ThinkingOnly`]).
    #[error("{}", describe_empty_response(*.filtered, *.think_only, .finish_reason.as_deref()))]
    ApiEmptyResponse {
        filtered: bool,
        think_only: bool,
        /// The finish reason exactly as the provider spelled it, if it sent one.
        finish_reason: Option<String>,
    },

    #[error("Context overflow")]
    ApiContextOverflow,

    /// The request body is larger than the endpoint accepts — not the token
    /// window, the bytes: Kimi's `message size exceeds limit`, or an HTTP 413
    /// that does not blame the context length. Almost always inline media;
    /// the turn loop recovers by omitting the oldest media from history.
    #[error("Request too large: {0}")]
    ApiRequestTooLarge(String),

    #[error("API business error ({code}): {message}")]
    ApiBusinessError { code: String, message: String },

    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Streaming was cancelled")]
    Cancelled,

    #[error("Invalid configuration: {0}")]
    Config(String),

    #[error("Tool schema normalization failed: {0}")]
    SchemaNormalization(String),

    #[error("Tool execution failed: {0}")]
    ToolExecution(String),

    #[error("MCP error: {0}")]
    Mcp(String),
}

impl LlmError {
    /// Whether the error is retriable at the LLM-request level.
    pub fn is_retryable(&self) -> bool {
        match self {
            // Transport failures are worth retrying: a connect error or timeout,
            // a connection dropped while the request was in flight, or one
            // dropped while the body was being read. reqwest reports the last as
            // a *decode* error wrapping the body failure (`bytes()`,
            // `bytes_stream()`); Rusta never asks reqwest to decode JSON, so
            // here a decode error always means the body read failed. Builder,
            // redirect and status errors describe the request itself and fail
            // fast.
            Self::Request(e) => {
                e.is_timeout() || e.is_connect() || e.is_request() || e.is_body() || e.is_decode()
            }
            Self::ApiConnection(_) | Self::ApiTimeout => true,
            Self::ApiBusinessError { .. } => false,
            // See the variant's own doc comment: an unexplained empty stream may
            // clear on a retry, a filtered one never will.
            Self::ApiEmptyResponse { filtered, .. } => !filtered,
            Self::ApiStatus {
                status, retry_hint, ..
            } => {
                *retry_hint
                    || status.is_server_error()
                    || matches!(
                        *status,
                        StatusCode::REQUEST_TIMEOUT
                            | StatusCode::CONFLICT
                            | StatusCode::TOO_MANY_REQUESTS
                    )
            }
            _ => false,
        }
    }

    /// Whether the error is transient and may succeed on retry.
    pub fn is_transient(&self) -> bool {
        self.is_retryable()
    }

    /// The server-requested backoff, if the response carried a `Retry-After`.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::ApiStatus { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Attach a `Retry-After` duration to an `ApiStatus` error; a no-op for any
    /// other variant. Lets the provider classify the error first, then annotate
    /// it with the header it parsed from the same response.
    #[must_use]
    pub fn with_retry_after(mut self, duration: Option<std::time::Duration>) -> Self {
        if let Self::ApiStatus { retry_after, .. } = &mut self {
            *retry_after = duration;
        }
        self
    }

    /// Whether the error is fatal and should abort the current operation.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::ApiBusinessError { .. } => true,
            Self::Config(_)
            | Self::SchemaNormalization(_)
            | Self::Provider(_)
            | Self::ToolExecution(_)
            | Self::Mcp(_)
            | Self::Json(_) => true,
            Self::ApiStatus { .. } => !self.is_retryable(),
            Self::Request(_) => !self.is_retryable(),
            _ => false,
        }
    }

    /// Whether the error indicates the context window was exceeded.
    pub fn is_context_overflow(&self) -> bool {
        matches!(self, Self::ApiContextOverflow)
    }

    /// An empty reply with no further detail about why.
    #[must_use]
    pub fn empty_response(filtered: bool) -> Self {
        Self::ApiEmptyResponse {
            filtered,
            think_only: false,
            finish_reason: None,
        }
    }

    /// Whether the error is a provider-side tool-exchange structural error
    /// (orphan tool results / unmatched tool_calls) that can often be repaired
    /// by dropping orphan tool results and retrying once.
    pub fn is_tool_exchange_structure_error(&self) -> bool {
        let message = match self {
            Self::ApiStatus { message, .. } => message,
            Self::ApiBusinessError { message, .. } => message,
            _ => return false,
        };
        let lowered = message.to_lowercase();
        (lowered.contains("role 'tool'") || lowered.contains("tool message"))
            && (lowered.contains("preceding message with 'tool_calls'")
                || lowered.contains("must be a response to")
                || lowered.contains("must be followed by tool messages")
                || lowered.contains("tool_calls' must be followed"))
            || lowered.contains("tool_use ids must be unique")
            || lowered.contains("tool_call_id") && lowered.contains("not found")
    }
}

/// The message for [`LlmError::ApiEmptyResponse`].
fn describe_empty_response(
    filtered: bool,
    think_only: bool,
    finish_reason: Option<&str>,
) -> String {
    let mut message = if think_only {
        "API returned only reasoning, with no answer text or tool calls; the output budget may \
         have run out while the model was thinking, or the stream was cut short"
            .to_string()
    } else {
        "API returned an empty response".to_string()
    };
    if filtered {
        message.push_str(" (blocked by the provider's content filter)");
    }
    if let Some(reason) = finish_reason {
        message.push_str(&format!(" [finish_reason: {reason}]"));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_is_transient_and_not_fatal() {
        let err = LlmError::ApiTimeout;
        assert!(err.is_transient());
        assert!(!err.is_fatal());
    }

    /// A safety-filter refusal is deterministic: the same request re-triggers
    /// the same filter, so retrying burns the whole backoff ramp before
    /// surfacing a notice it could have surfaced immediately.
    #[test]
    fn a_filtered_empty_response_is_not_retryable() {
        let err = LlmError::empty_response(true);
        assert!(!err.is_retryable());
        assert!(err.to_string().contains("content filter"), "{err}");
    }

    /// An unexplained empty stream is the opposite: nothing says it will happen
    /// again, and on thinking-heavy Kimi and GLM turns it routinely does not.
    #[test]
    fn an_unexplained_empty_response_is_retryable() {
        let err = LlmError::empty_response(false);
        assert!(err.is_retryable());
        assert!(!err.to_string().contains("content filter"), "{err}");
    }

    /// A reasoning-only reply says so, and names the finish reason, so the
    /// surfaced error explains itself instead of reading as "empty".
    #[test]
    fn a_think_only_response_explains_itself_and_is_retryable() {
        let err = LlmError::ApiEmptyResponse {
            filtered: false,
            think_only: true,
            finish_reason: Some("length".to_string()),
        };
        assert!(err.is_retryable());
        let text = err.to_string();
        assert!(text.contains("only reasoning"), "{text}");
        assert!(text.contains("finish_reason: length"), "{text}");
    }

    /// A connection that drops while the body is still streaming surfaces as a
    /// body error; that is as transient as a failed connect.
    #[tokio::test]
    async fn a_body_read_failure_is_retryable() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            // Promise a 100-byte body, send 5 bytes, and hang up.
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nhello")
                .await;
            let _ = socket.shutdown().await;
        });
        let response = crate::http::client()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        let body_err = response.text().await.unwrap_err();
        assert!(body_err.is_body() || body_err.is_decode() || body_err.is_request());
        let err = LlmError::Request(body_err);
        assert!(err.is_retryable(), "{err}");
    }

    #[test]
    fn test_config_is_fatal() {
        let err = LlmError::Config("bad key".to_string());
        assert!(!err.is_transient());
        assert!(err.is_fatal());
    }

    #[test]
    fn test_client_error_400_is_fatal() {
        let err = LlmError::ApiStatus {
            status: StatusCode::BAD_REQUEST,
            message: "bad request".to_string(),
            retry_hint: false,
            retry_after: None,
        };
        assert!(!err.is_transient());
        assert!(err.is_fatal());
    }

    #[test]
    fn test_rate_limit_is_transient_not_fatal() {
        let err = LlmError::ApiStatus {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "rate limited".to_string(),
            retry_hint: false,
            retry_after: None,
        };
        assert!(err.is_transient());
        assert!(!err.is_fatal());
    }

    #[test]
    fn test_408_and_409_are_transient_not_fatal() {
        for status in [StatusCode::REQUEST_TIMEOUT, StatusCode::CONFLICT] {
            let err = LlmError::ApiStatus {
                status,
                message: "transient".to_string(),
                retry_hint: false,
                retry_after: None,
            };
            assert!(err.is_retryable(), "{status} should be retryable");
            assert!(!err.is_fatal(), "{status} should not be fatal");
        }
    }

    #[test]
    fn test_retry_hint_overrides_fatal_status() {
        let err = LlmError::ApiStatus {
            status: StatusCode::BAD_REQUEST,
            message: "bad request".to_string(),
            retry_hint: true,
            retry_after: None,
        };
        assert!(err.is_retryable());
        assert!(!err.is_fatal());
    }

    #[tokio::test]
    async fn test_connect_error_is_retryable_but_builder_error_is_fatal() {
        // A connection-refused error is transient.
        let connect_err = crate::http::client()
            .get("http://127.0.0.1:1")
            .send()
            .await
            .unwrap_err();
        assert!(connect_err.is_connect());
        let err = LlmError::Request(connect_err);
        assert!(err.is_retryable());
        assert!(!err.is_fatal());

        // A builder error (invalid URL) should not be retried.
        let builder_err = crate::http::client()
            .get("not a valid url")
            .send()
            .await
            .unwrap_err();
        assert!(builder_err.is_builder());
        let err = LlmError::Request(builder_err);
        assert!(!err.is_retryable());
        assert!(err.is_fatal());
    }

    #[test]
    fn test_tool_exchange_structure_error_detection() {
        let examples = [
            "role 'tool' must be a response to a preceding message with 'tool_calls'",
            "tool message must be followed by tool messages",
            "tool_use ids must be unique",
            "tool_call_id 'tc-1' not found",
        ];
        for msg in examples {
            let err = LlmError::ApiStatus {
                status: StatusCode::BAD_REQUEST,
                message: msg.to_string(),
                retry_hint: false,
                retry_after: None,
            };
            assert!(
                err.is_tool_exchange_structure_error(),
                "'{msg}' should be classified as tool-exchange structure error"
            );
        }

        let not = LlmError::ApiStatus {
            status: StatusCode::BAD_REQUEST,
            message: "invalid api key".to_string(),
            retry_hint: false,
            retry_after: None,
        };
        assert!(!not.is_tool_exchange_structure_error());
    }
}
