use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use futures::Stream;
use serde::Deserialize;
use serde_json::{Number, Value};

use crate::{
    errors::LlmError,
    message::{ContentPart, StreamedMessagePart, ToolCall, ToolCallKind, ToolCallPart},
    provider::StreamedMessage,
};

use super::{
    openai_common::{
        OpenAiUsage, classify_api_error, de_reasoning_string, extract_usage, parse_finish_reason,
        reasoning_details_text,
    },
    reasoning_key::{self, ReasoningDialect},
    tool_call_id::sanitize_tool_call_id,
};
use eventsource_stream::{EventStreamError, Eventsource};
use futures::StreamExt;

#[derive(Debug, Clone, Deserialize)]
struct StreamChunk {
    #[serde(skip_serializing_if = "Option::is_none", rename = "id")]
    _id: Option<String>,
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<OpenAiUsage>,
    /// Gateways (OpenRouter and similar) report an upstream failure mid-stream
    /// as an `error` object instead of a status code, since the status line has
    /// already been sent.
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    _index: u32,
    #[serde(default)]
    delta: StreamDelta,
    #[serde(skip_serializing_if = "Option::is_none", rename = "finish_reason")]
    finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
struct StreamDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    // All three reasoning dialects, kept apart so the one the endpoint actually
    // used can be echoed back. Reading only `reasoning_content` here dropped
    // thinking outright on a `reasoning`-dialect endpoint (current vLLM,
    // GPT-OSS) — and the streaming path is the default, so that was silent on
    // every normal run. See `super::reasoning_key`.
    //
    // Parsed leniently: a field that is not a string (a `null`, or a structured
    // value) is skipped rather than failing the whole chunk.
    #[serde(default, deserialize_with = "de_reasoning_string")]
    reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "de_reasoning_string")]
    reasoning: Option<String>,
    /// A string on some gateways; on OpenRouter-style gateways an array of
    /// detail objects, which is kept verbatim for the echo.
    #[serde(default)]
    reasoning_details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamToolCallDelta {
    pub index: Number,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamFunctionDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

struct BufferedToolCall {
    id: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// The message used when a stream closes before the provider said it was done.
const INCOMPLETE_STREAM_MESSAGE: &str =
    "the response stream ended before the provider reported a finish reason";

/// Convert a raw SSE byte stream into a `StreamedMessage` of normalized parts.
///
/// Two failure shapes are surfaced as retryable errors rather than as a reply:
///
/// - the connection dies mid-stream (reset, proxy drop, or the idle timeout,
///   which reqwest reports as a body-read timeout), and
/// - the body closes cleanly without a finish reason or the `[DONE]` sentinel —
///   a proxy that times the response out can end it this way.
///
/// Either way what arrived is a fragment of a reply, and running the tool calls
/// assembled from a fragment is how a half-streamed `write_file` truncates a
/// file. A retry re-requests the whole step.
pub fn stream_chat_completions<S>(raw: S, reasoning: Arc<ReasoningDialect>) -> StreamedMessage
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let parsed = raw.eventsource();
    let mut buffered: HashMap<Number, BufferedToolCall> = HashMap::new();
    let mut seen_indices: Vec<Number> = Vec::new();
    let completed = Arc::new(AtomicBool::new(false));
    let completed_by_events = Arc::clone(&completed);

    let stream = parsed.flat_map(move |event| {
        let items: Vec<Result<StreamedMessagePart, LlmError>> = match event {
            Ok(eventsource_stream::Event { data, .. }) if data.trim() == "[DONE]" => {
                completed_by_events.store(true, Ordering::SeqCst);
                Vec::new()
            }
            Ok(eventsource_stream::Event { data, .. }) => {
                match handle_stream_event(&data, &mut buffered, &mut seen_indices, &reasoning) {
                    Ok(parts) => {
                        if parts
                            .iter()
                            .any(|part| matches!(part, StreamedMessagePart::Finish(..)))
                        {
                            completed_by_events.store(true, Ordering::SeqCst);
                        }
                        parts.into_iter().map(Ok).collect()
                    }
                    Err(e) => vec![Err(e)],
                }
            }
            Err(e) => vec![Err(stream_error(e))],
        };
        futures::stream::iter(items)
    });

    // Runs once the byte stream is exhausted. A stream that errored mid-way
    // never gets here: `generate` stops at the first error.
    let completion_check = futures::stream::once(async move {
        if completed.load(Ordering::SeqCst) {
            None
        } else {
            Some(Err(LlmError::ApiConnection(
                INCOMPLETE_STREAM_MESSAGE.to_string(),
            )))
        }
    })
    .filter_map(futures::future::ready);

    StreamedMessage::new(stream.chain(completion_check))
}

/// Classify a failure of the SSE stream itself.
///
/// A transport failure — the connection reset, a proxy hanging up, or the idle
/// timeout firing between chunks — is transient: the same request is likely to
/// succeed, so it maps onto the retryable connection and timeout errors. Bytes
/// that are not UTF-8, or not an event stream at all, will not improve on a
/// retry and stay fatal.
fn stream_error(error: EventStreamError<reqwest::Error>) -> LlmError {
    match error {
        EventStreamError::Transport(e) if e.is_timeout() => LlmError::ApiTimeout,
        EventStreamError::Transport(e) => {
            LlmError::ApiConnection(format!("the response stream was interrupted: {e}"))
        }
        other => LlmError::EventSource(other.to_string()),
    }
}

fn handle_stream_event(
    data: &str,
    buffered: &mut HashMap<Number, BufferedToolCall>,
    seen_indices: &mut Vec<Number>,
    reasoning: &ReasoningDialect,
) -> Result<Vec<StreamedMessagePart>, LlmError> {
    let chunk: StreamChunk = serde_json::from_str(data)?;

    if let Some(error) = &chunk.error {
        // Classified exactly like an error response body, as a server error:
        // a gateway that fails mid-stream usually fails upstream and
        // transiently, while a spent quota or an overrun context keeps its own
        // (fatal, or recoverable) classification.
        let body = serde_json::json!({ "error": error }).to_string();
        return Err(classify_api_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            &body,
            false,
        ));
    }

    // Some providers emit a final chunk with `choices: []` and a `usage` field.
    if chunk.choices.is_empty() {
        if let Some(usage) = chunk.usage {
            return Ok(vec![StreamedMessagePart::Usage(extract_usage(Some(
                &usage,
            )))]);
        }
        return Ok(Vec::new());
    }

    let choice = chunk
        .choices
        .into_iter()
        .next()
        .unwrap_or_else(|| StreamChoice {
            _index: 0,
            delta: StreamDelta::default(),
            finish_reason: None,
            usage: None,
        });

    let mut parts: Vec<StreamedMessagePart> = Vec::new();

    // Content / reasoning deltas.
    if let Some(text) = choice.delta.content
        && !text.is_empty()
    {
        parts.push(StreamedMessagePart::Content(ContentPart::Text { text }));
    }
    let detail_items = match &choice.delta.reasoning_details {
        Some(Value::Array(items)) if !items.is_empty() => Some(items.clone()),
        _ => None,
    };
    if let Some((key, think)) = reasoning_key::pick(
        choice.delta.reasoning_content.as_deref(),
        choice
            .delta
            .reasoning_details
            .as_ref()
            .and_then(Value::as_str),
        choice.delta.reasoning.as_deref(),
    ) {
        // Learn the dialect from the first chunk that carries reasoning, so the
        // *next* request echoes thinking under a key this endpoint reads.
        reasoning.observe(key);
        if !think.is_empty() {
            parts.push(StreamedMessagePart::Content(ContentPart::Think {
                think: think.to_string(),
            }));
        }
    } else if let Some(items) = &detail_items {
        // No string reasoning alongside the structured array: surface the
        // readable part of the array so the thinking still displays. A string
        // field, when present, is the better copy of the same text, which is
        // why this is the fallback.
        let text = reasoning_details_text(items);
        if !text.is_empty() {
            parts.push(StreamedMessagePart::Content(ContentPart::Think {
                think: text,
            }));
        }
    }
    if let Some(details) = detail_items {
        parts.push(StreamedMessagePart::Content(
            ContentPart::ReasoningDetails { details },
        ));
    }

    // Tool-call deltas.
    if let Some(tool_deltas) = choice.delta.tool_calls {
        for delta in tool_deltas {
            let idx = delta.index.clone();
            if !seen_indices.contains(&idx) {
                seen_indices.push(idx.clone());
            }
            let buf = buffered
                .entry(idx.clone())
                .or_insert_with(|| BufferedToolCall {
                    id: None,
                    kind: None,
                    name: None,
                    arguments: String::new(),
                });

            if let Some(id) = delta.id {
                buf.id = Some(id);
            }
            if let Some(kind) = delta.kind {
                buf.kind = Some(kind);
            }
            if let Some(func) = delta.function {
                if let Some(name) = func.name {
                    buf.name = Some(name.clone());
                    // Some providers (e.g. Z.AI GLM) send the function name and
                    // the full arguments in the same delta. Buffer any argument
                    // text that arrived with the name so it is included in the
                    // emitted ToolCall header.
                    if let Some(args) = func.arguments {
                        buf.arguments.push_str(&args);
                    }
                    // Emit a header part now that we know the name.
                    let kind = match buf.kind.as_deref() {
                        Some("builtin_function") => ToolCallKind::BuiltinFunction,
                        _ => ToolCallKind::Function,
                    };
                    let mut tc = ToolCall {
                        id: sanitize_tool_call_id(
                            &buf.id.clone().unwrap_or_else(|| idx.to_string()),
                        ),
                        kind,
                        name,
                        arguments: None,
                        extras: None,
                        stream_index: Some(idx.clone()),
                    };
                    if !buf.arguments.is_empty() {
                        tc.arguments = Some(buf.arguments.clone());
                    }
                    parts.push(StreamedMessagePart::ToolCall(tc));
                    continue;
                }
                if let Some(args) = func.arguments {
                    buf.arguments.push_str(&args);
                    parts.push(StreamedMessagePart::ToolCallPart(ToolCallPart {
                        index: idx,
                        arguments_part: Some(args),
                    }));
                    continue;
                }
            }
        }
    }

    // Finish reason and usage may arrive alongside content/tool deltas, so emit
    // them as separate parts after the other deltas.
    if let Some(finish_reason) = choice.finish_reason {
        let reason = parse_finish_reason(Some(&finish_reason));
        parts.push(StreamedMessagePart::Finish(reason, Some(finish_reason)));
    }

    // Prefer top-level usage; fall back to per-choice usage when the provider
    // only reports it there. Avoid emitting both to prevent double counting.
    if let Some(usage) = chunk.usage {
        parts.push(StreamedMessagePart::Usage(extract_usage(Some(&usage))));
    } else if let Some(usage) = choice.usage {
        parts.push(StreamedMessagePart::Usage(extract_usage(Some(&usage))));
    }

    if parts.is_empty() {
        // Otherwise return a sentinel text part that the assembler will ignore.
        parts.push(StreamedMessagePart::Content(ContentPart::text("")));
    }

    Ok(parts)
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    /// Feed SSE lines (each already prefixed with `data: `) through the parser
    /// and collect every item, errors included.
    async fn run(events: &[&str]) -> Vec<Result<StreamedMessagePart, LlmError>> {
        let bytes: Vec<Result<bytes::Bytes, reqwest::Error>> = events
            .iter()
            .map(|s| Ok(bytes::Bytes::from(format!("{s}\n\n"))))
            .collect();
        stream_chat_completions(
            futures::stream::iter(bytes),
            Arc::new(ReasoningDialect::detecting()),
        )
        .stream
        .collect()
        .await
    }

    /// A body that closes cleanly but never reported a finish reason or
    /// `[DONE]` is a fragment of a reply, not a reply: retryable, not assembled.
    #[tokio::test]
    async fn a_stream_that_ends_without_finishing_is_a_retryable_error() {
        let items = run(&[
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"a"}}]}}]}"#,
        ])
        .await;
        let err = items
            .into_iter()
            .find_map(Result::err)
            .expect("an incomplete stream must end in an error");
        assert!(matches!(err, LlmError::ApiConnection(_)), "{err:?}");
        assert!(err.is_retryable());
    }

    /// `[DONE]` alone is enough: some endpoints omit the finish reason.
    #[tokio::test]
    async fn done_without_a_finish_reason_is_complete() {
        let items = run(&[
            r#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            "data: [DONE]",
        ])
        .await;
        assert!(items.iter().all(Result::is_ok), "{items:?}");
    }

    /// A finish reason alone is enough too: not every endpoint sends `[DONE]`.
    #[tokio::test]
    async fn a_finish_reason_without_done_is_complete() {
        let items = run(&[
            r#"data: {"choices":[{"index":0,"finish_reason":"stop","delta":{"content":"hi"}}]}"#,
        ])
        .await;
        assert!(items.iter().all(Result::is_ok), "{items:?}");
    }

    /// A gateway reporting an upstream failure mid-stream is classified like
    /// an error response: a generic upstream error is retryable, a spent quota
    /// is not.
    #[tokio::test]
    async fn an_in_stream_error_object_is_classified() {
        let items = run(&[
            r#"data: {"choices":[{"index":0,"delta":{"content":"par"}}]}"#,
            r#"data: {"error":{"message":"upstream provider disconnected","code":502}}"#,
        ])
        .await;
        let err = items.into_iter().find_map(Result::err).unwrap();
        assert!(err.is_retryable(), "{err:?}");

        let items =
            run(&[r#"data: {"error":{"code":"1113","message":"Insufficient balance"}}"#]).await;
        let err = items.into_iter().find_map(Result::err).unwrap();
        assert!(matches!(err, LlmError::ApiBusinessError { .. }), "{err:?}");
        assert!(!err.is_retryable());
    }

    /// A transport failure between chunks — the connection reset, or the idle
    /// timeout firing — maps onto the retryable connection error.
    #[tokio::test]
    async fn a_mid_stream_transport_failure_is_retryable() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            // A chunked SSE response that dies after the first event.
            let event = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"}}]}\n\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{event}\r\n",
                event.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        let response = crate::http::client()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        let items: Vec<_> = stream_chat_completions(
            response.bytes_stream(),
            Arc::new(ReasoningDialect::detecting()),
        )
        .stream
        .collect()
        .await;
        let err = items
            .into_iter()
            .find_map(Result::err)
            .expect("a dropped stream must surface an error");
        assert!(
            matches!(err, LlmError::ApiConnection(_) | LlmError::ApiTimeout),
            "{err:?}"
        );
        assert!(err.is_retryable());
    }

    /// A structured `reasoning_details` array neither fails the chunk nor loses
    /// its text: the text is shown, and the array is kept for the echo.
    #[tokio::test]
    async fn a_reasoning_details_array_is_displayed_and_kept() {
        let items = run(&[
            r#"data: {"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","text":"weighing options"}]}}]}"#,
            r#"data: {"choices":[{"index":0,"finish_reason":"stop","delta":{"content":"done"}}]}"#,
            "data: [DONE]",
        ])
        .await;
        let parts: Vec<_> = items.into_iter().map(Result::unwrap).collect();
        assert!(
            parts.contains(&StreamedMessagePart::Content(ContentPart::Think {
                think: "weighing options".to_string()
            }))
        );
        assert!(parts.iter().any(|p| matches!(
            p,
            StreamedMessagePart::Content(ContentPart::ReasoningDetails { details })
                if details[0]["text"] == serde_json::json!("weighing options")
        )));
    }

    /// When a string reasoning field accompanies the array, the string is the
    /// displayed copy — the array text is not shown twice.
    #[tokio::test]
    async fn a_string_reasoning_field_wins_over_the_array_for_display() {
        let items = run(&[
            r#"data: {"choices":[{"index":0,"delta":{"reasoning":"string copy","reasoning_details":[{"type":"reasoning.text","text":"array copy"}]}}]}"#,
            r#"data: {"choices":[{"index":0,"finish_reason":"stop","delta":{"content":"done"}}]}"#,
        ])
        .await;
        let thinks: Vec<String> = items
            .into_iter()
            .filter_map(|item| match item {
                Ok(StreamedMessagePart::Content(ContentPart::Think { think })) => Some(think),
                _ => None,
            })
            .collect();
        assert_eq!(thinks, vec!["string copy".to_string()]);
    }

    /// A `null` or otherwise non-string reasoning field is skipped, not fatal.
    #[tokio::test]
    async fn a_null_reasoning_field_does_not_fail_the_chunk() {
        let items = run(&[
            r#"data: {"choices":[{"index":0,"finish_reason":"stop","delta":{"content":"ok","reasoning_content":null,"reasoning":{"unexpected":true}}}]}"#,
        ])
        .await;
        assert!(items.iter().all(Result::is_ok), "{items:?}");
    }

    #[tokio::test]
    async fn test_glm_style_stream_emits_tool_args_and_finish_reason() {
        // Z.AI GLM sends the function name and the full arguments in the same
        // streaming delta, reports finish_reason on the final content chunk,
        // and places usage at the top level of the final chunk.
        let events = vec![
            r#"data: {"id":"chat-1","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"ok"}}]}"#,
            r#"data: {"id":"chat-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"greeting.txt\"}"}}]}}]}"#,
            r#"data: {"id":"chat-1","choices":[{"index":0,"finish_reason":"stop","delta":{"role":"assistant","content":"done"}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            "data: [DONE]",
        ];
        let bytes: Vec<Result<bytes::Bytes, reqwest::Error>> = events
            .into_iter()
            .map(|s| Ok(bytes::Bytes::from(format!("{s}\n\n"))))
            .collect();
        let stream = stream_chat_completions(
            futures::stream::iter(bytes),
            Arc::new(ReasoningDialect::detecting()),
        );

        let parts: Vec<_> = stream.stream.collect().await;
        let parts: Vec<StreamedMessagePart> = parts
            .into_iter()
            .filter_map(|r| r.ok())
            .filter(|p| !matches!(p, StreamedMessagePart::Content(ContentPart::Text { text }) if text.is_empty()))
            .collect();

        assert_eq!(parts.len(), 5);
        assert!(matches!(
            &parts[0],
            StreamedMessagePart::Content(ContentPart::Think { .. })
        ));
        assert!(matches!(
            &parts[1],
            StreamedMessagePart::ToolCall(ToolCall { name, arguments, .. })
            if name == "write_file" && arguments.as_deref() == Some(r#"{"path":"greeting.txt"}"#)
        ));
        assert_eq!(
            parts[2],
            StreamedMessagePart::Content(ContentPart::text("done"))
        );
        assert_eq!(
            parts[3],
            StreamedMessagePart::Finish(
                Some(crate::provider::FinishReason::Stop),
                Some("stop".to_string())
            )
        );
        assert!(matches!(
            &parts[4],
            StreamedMessagePart::Usage(crate::usage::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                ..
            })
        ));
    }
}
