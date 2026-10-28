//! A tolerant [`StreamableHttpClient`] wrapper for servers that acknowledge
//! notifications with a bodyless `200 OK`.
//!
//! # Why this exists
//!
//! The MCP streamable-HTTP spec says a server that accepts a client
//! *notification* (a one-way message, no response expected) should reply
//! `202 Accepted` with an empty body. `rmcp`'s reqwest client implements exactly
//! that: it short-circuits on `202 Accepted` / `204 No Content`, and otherwise
//! demands a `Content-Type` of either `text/event-stream` or `application/json`.
//!
//! Z.AI's MCP endpoints (`web_search_prime`, `web_reader`) instead answer
//! `notifications/initialized` with **`200 OK`, no `Content-Type`, empty body**.
//! `rmcp` classifies that as `UnexpectedContentType(None)` — a fatal transport
//! error — and tears the connection down before a single tool is listed:
//!
//! ```text
//! ERROR rmcp::transport::worker: worker quit with fatal:
//!       Unexpected content type: None, when send initialized notification
//! ```
//!
//! # What this does
//!
//! [`LenientHttpClient`] delegates every call to `reqwest::Client`'s own
//! implementation and rewrites exactly one outcome: a notification that failed
//! with `UnexpectedContentType(None)` is reported as
//! [`StreamableHttpPostResponse::Accepted`].
//!
//! The narrowness is deliberate:
//!
//! * **Only notifications.** A *request* answered without a `Content-Type` is
//!   still a hard error. Swallowing that would return `Accepted` for a message
//!   whose response the caller is waiting on, and the client would hang forever.
//! * **Only a missing `Content-Type`.** A present-but-wrong content type
//!   (`text/html`, say) still fails, because that signals a real protocol or
//!   proxy problem rather than a terse acknowledgement.
//! * **Only after `error_for_status`.** `rmcp` raises this error only for 2xx
//!   responses, so a `500` with no content type never reaches the rewrite.

use std::collections::HashMap;
use std::sync::Arc;

use futures::stream::BoxStream;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    model::{ClientJsonRpcMessage, JsonRpcMessage},
    transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use sse_stream::Sse;

/// Wraps a `reqwest::Client` and tolerates bodyless `200 OK` notification acks.
///
/// See the [module docs](self) for the exact rewrite and why it is safe.
#[derive(Clone, Debug)]
pub struct LenientHttpClient {
    inner: reqwest::Client,
}

impl LenientHttpClient {
    /// Wrap an existing `reqwest::Client`.
    #[must_use]
    pub fn new(inner: reqwest::Client) -> Self {
        Self { inner }
    }
}

/// True if `message` is a JSON-RPC notification, i.e. no response is expected.
fn is_notification(message: &ClientJsonRpcMessage) -> bool {
    matches!(message, JsonRpcMessage::Notification(_))
}

/// The one rewrite: a notification answered with a 2xx that has neither a
/// `Content-Type` nor a body was accepted, as a `202` would have said.
///
/// rmcp itself accepts an empty 2xx when the server sends `Content-Length: 0`;
/// a chunked empty body has no length, so that case still reaches here.
fn lenient(
    notification: bool,
    response: Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>>,
) -> Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>> {
    match response {
        Err(StreamableHttpError::UnexpectedContentType(None)) if notification => {
            tracing::debug!(
                "server acknowledged a notification with a bodyless 2xx and no \
                 Content-Type; treating it as 202 Accepted"
            );
            Ok(StreamableHttpPostResponse::Accepted)
        }
        other => other,
    }
}

impl StreamableHttpClient for LenientHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        // Capture this before `message` is moved into the delegate.
        let notification = is_notification(&message);
        let response = self
            .inner
            .post_message(uri, message, session_id, auth_header, custom_headers)
            .await;
        lenient(notification, response)
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let notification = is_notification(&message);
        let response = self
            .inner
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await;
        lenient(notification, response)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.inner
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.inner
            .get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Parse a wire-format JSON-RPC message, exactly as the transport would.
    fn parse(value: serde_json::Value) -> ClientJsonRpcMessage {
        serde_json::from_value(value).expect("valid client JSON-RPC message")
    }

    #[test]
    fn initialized_notification_is_a_notification() {
        // This is the precise message Z.AI answers with a bodyless 200.
        let msg = parse(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));
        assert!(is_notification(&msg));
    }

    #[test]
    fn cancelled_notification_is_a_notification() {
        let msg = parse(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": 1 },
        }));
        assert!(is_notification(&msg));
    }

    /// A request answered without a `Content-Type` must stay a hard error: the
    /// caller awaits its response, so reporting `Accepted` would hang forever.
    #[test]
    fn requests_are_never_treated_as_notifications() {
        let ping = parse(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }));
        assert!(!is_notification(&ping));

        let call = parse(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "search", "arguments": {} },
        }));
        assert!(!is_notification(&call));
    }
}
