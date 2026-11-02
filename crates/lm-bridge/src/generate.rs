use std::collections::HashMap;

use futures::StreamExt;
use serde_json::Number;

use crate::{
    errors::LlmError,
    message::{ContentPart, Message, Role, StreamedMessagePart, ToolCall, ToolCallPart},
    provider::{FinishReason, GenerateCallbacks, StreamedMessage},
    providers::tool_call_id::deduplicate_tool_call_ids,
    usage::TokenUsage,
};

/// Result of draining a provider stream.
#[derive(Debug, Clone)]
pub struct GenerateResult {
    pub id: Option<String>,
    pub message: Message,
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<FinishReason>,
    pub raw_finish_reason: Option<String>,
}

/// What [`generate`] makes of a reply that finished normally with reasoning
/// alone ([`Message::is_thinking_only`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingOnly {
    /// Fail it as an empty response, to be retried: a request that needs
    /// the answer text, such as a compaction summary.
    Reject,
    /// Return it, for an agent's turn to ask for the answer. One cut short by
    /// the output budget, or with no finish reason, still fails: it is a
    /// fragment, not a reply.
    AcceptCompleted,
}

/// Consume a `StreamedMessage` and produce a final `GenerateResult`, invoking
/// callbacks. A reply with neither answer text nor a tool call fails as an
/// empty response, except one of reasoning alone that `thinking_only`
/// accepts.
pub async fn generate(
    streamed: StreamedMessage,
    callbacks: GenerateCallbacks,
    thinking_only: ThinkingOnly,
) -> Result<GenerateResult, LlmError> {
    let StreamedMessage {
        id,
        mut finish_reason,
        mut raw_finish_reason,
        usage,
        stream,
    } = streamed;

    let mut text_parts: Vec<String> = Vec::new();
    let mut think_parts: Vec<String> = Vec::new();
    let mut reasoning_details: Vec<serde_json::Value> = Vec::new();
    let mut tool_calls_by_index: HashMap<Number, ToolCall> = HashMap::new();
    let mut ordered_indices: Vec<Number> = Vec::new();
    let mut saw_any_part = false;
    let mut collected_usage = usage;

    let mut stream = stream;
    while let Some(part) = stream.next().await {
        let part = part?;

        match part {
            StreamedMessagePart::Usage(u) => {
                if let Some(existing) = collected_usage.as_mut() {
                    existing.add(&u);
                } else {
                    collected_usage = Some(u);
                }
            }
            other => {
                saw_any_part = true;

                if let Some(cb) = callbacks.on_message_part.as_ref() {
                    cb(other.clone());
                }

                match other {
                    StreamedMessagePart::Content(ContentPart::Text { text }) => {
                        text_parts.push(text);
                    }
                    StreamedMessagePart::Content(ContentPart::Think { think }) => {
                        think_parts.push(think);
                    }
                    StreamedMessagePart::Content(ContentPart::ReasoningDetails { details }) => {
                        reasoning_details.extend(details);
                    }
                    StreamedMessagePart::Content(_) => {
                        // Image/audio/video URLs are not expected mid-stream for text agents.
                    }
                    StreamedMessagePart::ToolCall(tc) => {
                        if let Some(cb) = callbacks.on_tool_call.as_ref() {
                            cb(tc.clone());
                        }
                        let idx = tc
                            .stream_index
                            .clone()
                            .unwrap_or_else(|| Number::from(ordered_indices.len()));
                        if !ordered_indices.contains(&idx) {
                            ordered_indices.push(idx.clone());
                        }
                        let mut tc = tc;
                        if tc.stream_index.is_none() {
                            tc.stream_index = Some(idx.clone());
                        }
                        tool_calls_by_index.insert(idx, tc);
                    }
                    StreamedMessagePart::ToolCallPart(ToolCallPart {
                        index,
                        arguments_part,
                    }) => {
                        if !ordered_indices.contains(&index) {
                            ordered_indices.push(index.clone());
                        }
                        let tc = tool_calls_by_index
                            .entry(index)
                            .or_insert_with(|| ToolCall::new("", ""));
                        if let Some(delta) = arguments_part {
                            tc.arguments = Some(match tc.arguments.take() {
                                Some(existing) => existing + &delta,
                                None => delta,
                            });
                        }
                    }
                    StreamedMessagePart::Finish(reason, raw) => {
                        finish_reason = reason;
                        raw_finish_reason = raw;
                    }
                    StreamedMessagePart::Usage(_) => unreachable!(),
                }
            }
        }
    }

    // A generation the provider cut short is a fragment however much of it
    // streamed, and its tool calls may be too. Failing it as a dropped
    // connection lets the retry layer re-issue the request; nothing from it,
    // text or tool call, is kept.
    if finish_reason == Some(FinishReason::Interrupted) {
        return Err(LlmError::ApiConnection(format!(
            "provider ended the generation early: finish_reason={}",
            raw_finish_reason.as_deref().unwrap_or("interrupted")
        )));
    }

    if !saw_any_part {
        // Not a single part arrived — not even a terminating `Finish`, which
        // would have set `saw_any_part`. There is no finish reason to read, so
        // this cannot be a filter refusal; it is a stream that produced nothing,
        // which is exactly the case worth retrying.
        return Err(LlmError::empty_response(false));
    }

    let mut content: Vec<ContentPart> = Vec::new();
    // Judge by the *joined* value, not by whether any delta arrived. A provider
    // refusal typically streams one empty thinking delta and then terminates on
    // its filter reason: the vec is non-empty, but it joins to "". Guarding on
    // the vec built a `Think { think: "" }` part, which made the message below
    // look content-bearing — so a refusal returned `Ok` with a message that
    // serializes to nothing, and the emptiness check never ran.
    let text = text_parts.join("");
    if !text.trim().is_empty() {
        content.push(ContentPart::Text { text });
    }
    let think = think_parts.join("");
    let has_text = !content.is_empty();
    let has_think = !think.trim().is_empty();
    if has_think {
        content.push(ContentPart::Think { think });
    }
    if !reasoning_details.is_empty() {
        content.push(ContentPart::ReasoningDetails {
            details: reasoning_details,
        });
    }

    let mut tool_calls: Vec<ToolCall> = ordered_indices
        .into_iter()
        .filter_map(|idx| {
            let mut tc = tool_calls_by_index.remove(&idx)?;
            tc.stream_index = None;
            Some(tc)
        })
        .map(sanitize_tool_call_arguments)
        .collect();

    let ids: Vec<String> = tool_calls.iter().map(|tc| tc.id.clone()).collect();
    let deduped_ids = deduplicate_tool_call_ids(ids);
    for (tc, id) in tool_calls.iter_mut().zip(deduped_ids) {
        tc.id = id;
    }

    let message = Message {
        role: Role::Assistant,
        content,
        tool_calls,
        tool_call_id: None,
        tools: Vec::new(),
        partial: false,
    };

    let accepted = thinking_only == ThinkingOnly::AcceptCompleted
        && finish_reason == Some(FinishReason::Stop)
        && message.is_thinking_only();
    if !has_text && message.tool_calls.is_empty() && !accepted {
        // The stream ran and terminated, but produced nothing sendable: no
        // answer text and no tool calls. Reasoning alone is not an answer — it
        // is what a reply cut short mid-thought looks like, whether by the
        // output budget or a broken stream — so it is rejected exactly like an
        // empty reply, unless the model finished normally and the caller takes
        // it. When the stream ended on a content filter — OpenAI's
        // `content_filter`, Z.AI's `sensitive`, both mapped to `ContentFilter`
        // — the refusal is deterministic and must not be retried.
        return Err(LlmError::ApiEmptyResponse {
            filtered: finish_reason == Some(FinishReason::ContentFilter),
            think_only: has_think,
            finish_reason: raw_finish_reason,
        });
    }

    Ok(GenerateResult {
        id,
        message,
        usage: collected_usage,
        finish_reason,
        raw_finish_reason,
    })
}

fn sanitize_tool_call_arguments(mut tc: ToolCall) -> ToolCall {
    if let Some(args) = &tc.arguments
        && args.trim().is_empty()
    {
        tc.arguments = Some("{}".to_string());
    }
    tc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{StreamedMessagePart, ToolCallPart};
    use crate::provider::StreamedMessage;
    use futures::stream;

    fn streamed(parts: Vec<Result<StreamedMessagePart, LlmError>>) -> StreamedMessage {
        StreamedMessage::new(stream::iter(parts))
    }

    /// A GLM refusal arrives as an *empty* thinking delta plus `sensitive`.
    /// Guarding on the delta vec rather than the joined value built a
    /// `Think { think: "" }` part, so the message looked content-bearing, the
    /// emptiness check never ran, and the refusal returned `Ok` with a message
    /// that serializes to nothing.
    #[tokio::test]
    async fn a_zai_sensitive_refusal_is_marked_filtered() {
        assert_eq!(
            crate::providers::openai_common::parse_finish_reason(Some("sensitive")),
            Some(FinishReason::ContentFilter),
            "Z.AI's spelling must reach the same finish reason"
        );
        let parts = vec![
            Ok(StreamedMessagePart::Content(ContentPart::Think {
                think: String::new(),
            })),
            Ok(StreamedMessagePart::Finish(
                Some(FinishReason::ContentFilter),
                Some("sensitive".to_string()),
            )),
        ];
        let err = generate(
            streamed(parts),
            GenerateCallbacks::empty(),
            ThinkingOnly::Reject,
        )
        .await
        .expect_err("a refusal yields no message");
        assert!(matches!(
            err,
            LlmError::ApiEmptyResponse { filtered: true, .. }
        ));
        assert!(!err.is_retryable());
    }

    /// Reasoning alone is not an answer: it is what a reply cut short
    /// mid-thought looks like, and it must be retried, not reported as final.
    #[tokio::test]
    async fn a_reasoning_only_reply_is_a_retryable_empty_response() {
        let parts = vec![
            Ok(StreamedMessagePart::Content(ContentPart::think(
                "Let me think about the file layout...",
            ))),
            Ok(StreamedMessagePart::Finish(
                Some(FinishReason::Length),
                Some("length".to_string()),
            )),
        ];
        let err = generate(
            streamed(parts),
            GenerateCallbacks::empty(),
            ThinkingOnly::Reject,
        )
        .await
        .expect_err("a think-only reply is not an answer");
        match &err {
            LlmError::ApiEmptyResponse {
                filtered,
                think_only,
                finish_reason,
            } => {
                assert!(!filtered);
                assert!(think_only);
                assert_eq!(finish_reason.as_deref(), Some("length"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(err.is_retryable());
    }

    /// A reply of reasoning alone that finished normally is returned when the
    /// caller takes one, its reasoning kept; cut short by the budget, with no
    /// finish reason, or when the caller needs text, it is still an empty
    /// response.
    #[tokio::test]
    async fn a_completed_reasoning_only_reply_is_returned_when_accepted() {
        let reply = |finish: Option<FinishReason>| {
            let mut parts = vec![Ok(StreamedMessagePart::Content(ContentPart::think(
                "Nothing to change here.",
            )))];
            if let Some(reason) = finish {
                parts.push(Ok(StreamedMessagePart::Finish(Some(reason), None)));
            }
            streamed(parts)
        };
        let result = generate(
            reply(Some(FinishReason::Stop)),
            GenerateCallbacks::empty(),
            ThinkingOnly::AcceptCompleted,
        )
        .await
        .expect("a completed reasoning-only reply");
        assert!(result.message.is_thinking_only());
        assert_eq!(result.message.think_content(), "Nothing to change here.");
        assert_eq!(result.finish_reason, Some(FinishReason::Stop));

        for (finish, thinking_only) in [
            (Some(FinishReason::Length), ThinkingOnly::AcceptCompleted),
            (None, ThinkingOnly::AcceptCompleted),
            (Some(FinishReason::Stop), ThinkingOnly::Reject),
        ] {
            let err = generate(reply(finish), GenerateCallbacks::empty(), thinking_only)
                .await
                .expect_err("not a reply to take");
            assert!(
                matches!(
                    err,
                    LlmError::ApiEmptyResponse {
                        think_only: true,
                        ..
                    }
                ),
                "{finish:?} {thinking_only:?}: {err:?}"
            );
        }

        // A reply with nothing in it at all is no reasoning to take.
        let empty = streamed(vec![Ok(StreamedMessagePart::Finish(
            Some(FinishReason::Stop),
            None,
        ))]);
        let err = generate(
            empty,
            GenerateCallbacks::empty(),
            ThinkingOnly::AcceptCompleted,
        )
        .await
        .expect_err("an empty reply");
        assert!(matches!(
            err,
            LlmError::ApiEmptyResponse {
                think_only: false,
                ..
            }
        ));
    }

    /// A generation the provider cut short, in each of its spellings, is a
    /// retryable error whatever streamed before the cut: its text and its
    /// tool call are fragments, and neither is returned.
    #[tokio::test]
    async fn a_generation_the_provider_cut_short_is_retryable() {
        for raw in ["insufficient_system_resource", "aborted", "network_error"] {
            let reason = crate::providers::openai_common::parse_finish_reason(Some(raw));
            assert_eq!(reason, Some(FinishReason::Interrupted), "{raw}");
            let parts = vec![
                Ok(StreamedMessagePart::Content(ContentPart::Text {
                    text: "partial ".to_string(),
                })),
                Ok(StreamedMessagePart::ToolCall(
                    ToolCall::new("call_1", "write_file").with_arguments("{\"path\":\"a\"}"),
                )),
                Ok(StreamedMessagePart::Finish(reason, Some(raw.to_string()))),
            ];
            let err = generate(
                streamed(parts),
                GenerateCallbacks::empty(),
                ThinkingOnly::Reject,
            )
            .await
            .expect_err("a cut generation is no answer");
            let expected = format!("provider ended the generation early: finish_reason={raw}");
            assert!(
                matches!(&err, LlmError::ApiConnection(message) if *message == expected),
                "{err:?}"
            );
            assert!(err.is_retryable(), "{raw}");
        }
    }

    /// Reasoning followed by a tool call is a normal step.
    #[tokio::test]
    async fn reasoning_with_a_tool_call_is_a_valid_step() {
        let parts = vec![
            Ok(StreamedMessagePart::Content(ContentPart::think("plan"))),
            Ok(StreamedMessagePart::ToolCall(
                ToolCall::new("tc-1", "read_file")
                    .with_arguments("{}")
                    .with_stream_index(0),
            )),
            Ok(StreamedMessagePart::Finish(
                Some(FinishReason::ToolCalls),
                Some("tool_calls".to_string()),
            )),
        ];
        let result = generate(
            streamed(parts),
            GenerateCallbacks::empty(),
            ThinkingOnly::Reject,
        )
        .await
        .expect("a tool call is an answer");
        assert_eq!(result.message.tool_calls.len(), 1);
        assert_eq!(result.message.think_content(), "plan");
    }

    /// Structured reasoning arrays from every chunk are merged into one part,
    /// in order, ready to be echoed back.
    #[tokio::test]
    async fn reasoning_detail_arrays_are_merged_in_order() {
        let parts = vec![
            Ok(StreamedMessagePart::Content(
                ContentPart::ReasoningDetails {
                    details: vec![serde_json::json!({"type": "reasoning.text", "text": "a"})],
                },
            )),
            Ok(StreamedMessagePart::Content(
                ContentPart::ReasoningDetails {
                    details: vec![serde_json::json!({"type": "reasoning.text", "text": "b"})],
                },
            )),
            Ok(StreamedMessagePart::Content(ContentPart::text("answer"))),
            Ok(StreamedMessagePart::Finish(
                Some(FinishReason::Stop),
                Some("stop".to_string()),
            )),
        ];
        let result = generate(
            streamed(parts),
            GenerateCallbacks::empty(),
            ThinkingOnly::Reject,
        )
        .await
        .unwrap();
        let details: Vec<_> = result
            .message
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::ReasoningDetails { details } => Some(details.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(details.len(), 1, "one merged part");
        assert_eq!(details[0].len(), 2);
        assert_eq!(details[0][1]["text"], serde_json::json!("b"));
    }

    /// A stream that produced nothing at all has no reason to blame, so it stays
    /// retryable.
    #[tokio::test]
    async fn a_wholly_empty_stream_stays_retryable() {
        let err = generate(
            streamed(Vec::new()),
            GenerateCallbacks::empty(),
            ThinkingOnly::Reject,
        )
        .await
        .expect_err("an empty stream yields no message");
        assert!(matches!(
            err,
            LlmError::ApiEmptyResponse {
                filtered: false,
                think_only: false,
                ..
            }
        ));
        assert!(err.is_retryable());
    }

    /// A filtered finish on a stream that *did* produce content is not an error;
    /// the partial answer is returned with its reason intact.
    #[tokio::test]
    async fn a_filtered_finish_with_content_is_not_an_error() {
        let parts = vec![
            Ok(StreamedMessagePart::Content(ContentPart::text("partial"))),
            Ok(StreamedMessagePart::Finish(
                Some(FinishReason::ContentFilter),
                Some("content_filter".to_string()),
            )),
        ];
        let result = generate(
            streamed(parts),
            GenerateCallbacks::empty(),
            ThinkingOnly::Reject,
        )
        .await
        .expect("content was produced, so there is a message");
        assert_eq!(result.message.text_content(), "partial");
        assert_eq!(result.finish_reason, Some(FinishReason::ContentFilter));
    }

    #[tokio::test]
    async fn test_generate_assembles_text_and_tool_call() {
        let parts: Vec<Result<StreamedMessagePart, LlmError>> = vec![
            Ok(StreamedMessagePart::Content(ContentPart::text("Let me "))),
            Ok(StreamedMessagePart::Content(ContentPart::text("help."))),
            Ok(StreamedMessagePart::ToolCall(
                ToolCall::new("tc-1", "read_file").with_stream_index(0),
            )),
            Ok(StreamedMessagePart::ToolCallPart(ToolCallPart {
                index: 0.into(),
                arguments_part: Some("{\"path\": \"".to_string()),
            })),
            Ok(StreamedMessagePart::ToolCallPart(ToolCallPart {
                index: 0.into(),
                arguments_part: Some("foo.txt\"}".to_string()),
            })),
        ];
        let streamed = StreamedMessage {
            id: Some("chat-1".to_string()),
            finish_reason: Some(crate::provider::FinishReason::ToolCalls),
            raw_finish_reason: Some("tool_calls".to_string()),
            usage: Some(crate::usage::TokenUsage::empty()),
            stream: Box::pin(stream::iter(parts)),
        };

        let result = generate(streamed, GenerateCallbacks::empty(), ThinkingOnly::Reject)
            .await
            .unwrap();
        assert_eq!(result.message.text_content(), "Let me help.");
        assert_eq!(result.message.tool_calls.len(), 1);
        assert_eq!(result.message.tool_calls[0].name, "read_file");
        assert_eq!(
            result.message.tool_calls[0].arguments.as_deref(),
            Some("{\"path\": \"foo.txt\"}")
        );
    }

    trait ToolCallIndex {
        fn with_stream_index(self, index: usize) -> Self;
    }

    impl ToolCallIndex for ToolCall {
        fn with_stream_index(mut self, index: usize) -> Self {
            self.stream_index = Some(index.into());
            self
        }
    }

    #[tokio::test]
    async fn test_generate_collects_usage_parts() {
        let parts: Vec<Result<StreamedMessagePart, LlmError>> = vec![
            Ok(StreamedMessagePart::Content(ContentPart::text("hi"))),
            Ok(StreamedMessagePart::Usage(crate::usage::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            })),
        ];
        let streamed = StreamedMessage {
            id: None,
            finish_reason: Some(crate::provider::FinishReason::Stop),
            raw_finish_reason: Some("stop".to_string()),
            usage: None,
            stream: Box::pin(stream::iter(parts)),
        };

        let result = generate(streamed, GenerateCallbacks::empty(), ThinkingOnly::Reject)
            .await
            .unwrap();
        let usage = result.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }
}
