//! M1 acceptance tests: the HTTP backend end-to-end against the hand-rolled
//! mock SSE server — token stream e2e, retry/backoff with a flaky mock, the
//! 404 `base_url` hint, `tool_calls` passthrough, and non-streaming completion
//! (ADR §8, §9).

mod mock_server;

use std::time::Duration;

use serde_json::json;

use rusta_llm::Error;
use rusta_llm::http::{HttpBackend, HttpConfig};
use rusta_llm::types::{ChatRequest, FinishReason, Message, StreamEvent};

use mock_server::{MockServer, Step};

fn backend(port: u16) -> HttpBackend {
    HttpBackend::new(HttpConfig {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        ..HttpConfig::default()
    })
    .expect("valid config")
}

fn request() -> ChatRequest {
    ChatRequest::new(vec![Message::user("hello")])
}

async fn collect(mut rx: tokio::sync::mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

fn content_delta(text: &str) -> serde_json::Value {
    json!({ "choices": [{ "delta": { "content": text } }] })
}

fn finish_delta(reason: &str) -> serde_json::Value {
    json!({ "choices": [{ "delta": {}, "finish_reason": reason }] })
}

#[tokio::test]
async fn streams_deltas_and_finish_end_to_end() {
    let server = MockServer::start(|_| {
        Step::Sse(vec![
            content_delta("Hel"),
            content_delta("lo "),
            content_delta("world"),
            json!({ "choices": [{ "delta": { "content": "!" }, "finish_reason": null }] }),
            finish_delta("stop"),
        ])
    });
    let rx = backend(server.port)
        .stream(request())
        .await
        .expect("stream starts");
    let events = collect(rx).await;
    assert_eq!(
        events,
        vec![
            StreamEvent::Delta("Hel".into()),
            StreamEvent::Delta("lo ".into()),
            StreamEvent::Delta("world".into()),
            StreamEvent::Delta("!".into()),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn assembles_native_tool_calls_passthrough() {
    let server = MockServer::start(|_| {
        Step::Sse(vec![
            json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "id": "call_1", "function": { "name": "read", "arguments": "{\"pa" } }
            ] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "function": { "arguments": "th\":\"src/main.rs\"}" } }
            ] } }] }),
            finish_delta("tool_calls"),
        ])
    });
    let rx = backend(server.port)
        .stream(request())
        .await
        .expect("stream starts");
    let events = collect(rx).await;
    assert_eq!(
        events,
        vec![
            StreamEvent::ToolCall {
                id: Some("call_1".into()),
                name: "read".into(),
                arguments: "{\"path\":\"src/main.rs\"}".into(),
            },
            StreamEvent::Finish(FinishReason::ToolCalls),
        ]
    );
}

#[tokio::test]
async fn tolerates_pings_chunk_splits_and_utf8_boundaries() {
    // `: ping` comment, a JSON payload split mid-string across TCP reads, and a
    // multi-byte character split across chunk boundaries.
    let server = MockServer::start(|_| {
        Step::Fragments(vec![
            b": ping\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\xc3".to_vec(), // UTF-8 split mid-ä
            b"\xa4\"}}]}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_vec(),
            b"data: [DONE]\n\n".to_vec(),
        ])
    });
    let rx = backend(server.port)
        .stream(request())
        .await
        .expect("stream starts");
    let events = collect(rx).await;
    assert_eq!(
        events,
        vec![
            StreamEvent::Delta("a\u{e4}".into()),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[tokio::test]
async fn retries_5xx_then_succeeds_on_third_attempt() {
    let server = MockServer::start(|n| {
        if n <= 2 {
            Step::Status(500, "upstream exploded".into())
        } else {
            Step::Sse(vec![content_delta("ok"), finish_delta("stop")])
        }
    });
    let backend = backend(server.port).with_backoff(Duration::from_millis(1));
    let rx = backend.stream(request()).await.expect("stream starts");
    let events = collect(rx).await;
    assert_eq!(events.len(), 2);
    assert_eq!(server.hits(), 3);
}

#[tokio::test]
async fn exhausts_retries_on_persistent_5xx() {
    let server = MockServer::start(|_| Step::Status(503, "down for maintenance".into()));
    let backend = backend(server.port).with_backoff(Duration::from_millis(1));
    let error = backend.stream(request()).await.expect_err("must fail");
    assert!(
        matches!(error, Error::Unreachable { attempts: 3, .. }),
        "{error}"
    );
    assert_eq!(server.hits(), 3);
}

#[tokio::test]
async fn connection_refused_is_unreachable_after_three_attempts() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let port = probe.local_addr().expect("probe addr").port();
    drop(probe);
    let backend = HttpBackend::new(HttpConfig {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        ..HttpConfig::default()
    })
    .expect("valid config")
    .with_backoff(Duration::from_millis(1));
    let error = backend.stream(request()).await.expect_err("must fail");
    assert!(
        matches!(error, Error::Unreachable { attempts: 3, .. }),
        "{error}"
    );
}

#[tokio::test]
async fn a_404_yields_base_url_remedy_without_retry() {
    let server = MockServer::start(|_| Step::Status(404, "no such route".into()));
    let error = backend(server.port)
        .stream(request())
        .await
        .expect_err("must fail");
    let message = error.to_string();
    assert!(message.contains("8080/v1"), "{message}");
    assert!(message.contains("11434/v1"), "{message}");
    assert!(message.contains("1234/v1"), "{message}");
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn a_4xx_surfaces_clipped_server_detail() {
    let body = (0..12)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let server = MockServer::start(move |_| Step::Status(429, body.clone()));
    let backend = backend(server.port).with_backoff(Duration::from_millis(1));
    let error = backend.stream(request()).await.expect_err("must fail");
    assert!(matches!(error, Error::Http { status: 429, .. }), "{error}");
    let message = error.to_string();
    assert!(message.contains("line 0"), "{message}");
    assert!(
        !message.contains("line 11"),
        "detail must be clipped: {message}"
    );
}

#[tokio::test]
async fn completes_non_streaming() {
    let server = MockServer::start(|_| {
        Step::Json(json!({
            "choices": [{ "message": { "role": "assistant", "content": "sum: 42" }, "finish_reason": "stop" }]
        }))
    });
    let output = backend(server.port)
        .complete(request())
        .await
        .expect("completion");
    assert_eq!(output, "sum: 42");
}

// ------------------------------------------- 2026-09-17 round-8 findings

/// A28/A30: a server that closes mid-stream without a `finish_reason` and
/// without `[DONE]` used to be reported as a clean `Finish(Stop)`, and the
/// agent committed the partial text as the turn's answer — truncated output
/// indistinguishable from a complete reply, which is what a SEARCH block
/// then gets anchored on.
///
/// A30 is the other half: before this, *no* test in the crate produced a
/// `StreamEvent::Failed` at all, so the whole failure path shipped unrun.
#[tokio::test]
async fn a_stream_cut_without_a_terminator_fails_instead_of_finishing() {
    let server = MockServer::start(|_| {
        // Raw fragments so the response ends exactly here: content arrived,
        // then the connection closed. No finish_reason, no [DONE].
        Step::Fragments(vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"partial ans\"}}]}\n\n".to_vec(),
        ])
    });
    let events = collect(
        backend(server.port)
            .stream(request())
            .await
            .expect("stream opens"),
    )
    .await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Failed(cause) if cause.contains("truncated"))),
        "a cut stream must report failure with a remedy: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, StreamEvent::Finish(_))),
        "a cut stream must not also report a clean finish: {events:?}"
    );
}

/// The A28 check is deliberately narrow: a server that omits `[DONE]` but
/// *does* send a `finish_reason` has said the reply is complete, and is
/// believed. Without this the fix would break every such server — the
/// over-fencing risk of A28.
#[tokio::test]
async fn a_finish_reason_without_done_is_still_a_clean_finish() {
    let server = MockServer::start(|_| {
        Step::Fragments(vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_vec(),
        ])
    });
    let events = collect(
        backend(server.port)
            .stream(request())
            .await
            .expect("stream opens"),
    )
    .await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Finish(FinishReason::Stop))),
        "finish_reason alone must still finish cleanly: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, StreamEvent::Failed(_))),
        "a server that reported a finish reason must not be failed: {events:?}"
    );
}

/// A31: the A8 remediation bounded a wire-supplied `tool_calls` index at 64
/// — `"index": 4294967295` had asked for a ~309 GB contiguous allocation,
/// an abort rather than a recoverable error. The bound shipped correct and
/// *untested*, while ADR §16.4 claimed every prior finding was pinned by a
/// regression test. This is that test.
#[tokio::test]
async fn an_out_of_range_tool_call_index_is_rejected_not_allocated() {
    let server = MockServer::start(|_| {
        Step::Fragments(vec![
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":4294967295,\
              \"id\":\"x\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n"
                .to_vec(),
        ])
    });
    let events = collect(
        backend(server.port)
            .stream(request())
            .await
            .expect("stream opens"),
    )
    .await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Failed(cause) if cause.contains("limit"))),
        "an out-of-range index must be a recoverable error: {events:?}"
    );
}
