//! Project / repair a raw context history into a provider-safe message list.

use lm_bridge::message::{ContentPart, Message, Role};

/// Options controlling the projection pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProjectOptions {
    /// Synthesize a placeholder `tool_result` for every assistant `tool_call`
    /// that lacks a matching result, including trailing in-flight calls.
    pub synthesize_missing: bool,
    /// Drop `tool_result` messages whose `tool_call_id` matches no assistant
    /// `tool_call` anywhere in the history.
    pub drop_orphan_results: bool,
    /// Drop leading non-`user` messages until the first `user` message.
    pub drop_leading_non_user: bool,
    /// Merge consecutive `assistant` messages into one.
    pub merge_consecutive_assistants: bool,
    /// Keep the first occurrence of each `tool_call` id and the first result
    /// for each id; drop later duplicates.
    pub dedupe_duplicate_tool_calls: bool,
}

impl ProjectOptions {
    /// Standard projection used for normal model requests.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            synthesize_missing: true,
            drop_orphan_results: true,
            ..Self::default()
        }
    }

    /// Strict projection used for the one-shot resend fallback after provider
    /// tool-exchange / message-structure errors.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            synthesize_missing: true,
            drop_orphan_results: true,
            drop_leading_non_user: true,
            merge_consecutive_assistants: true,
            dedupe_duplicate_tool_calls: true,
        }
    }
}

/// Project a raw context history into a provider-safe message list.
#[must_use]
pub fn project(history: &[Message], options: &ProjectOptions) -> Vec<Message> {
    let mut result = merge_adjacent_user_messages(history);
    if options.dedupe_duplicate_tool_calls {
        result = dedupe_duplicate_tool_calls(result);
    }
    result = repair_tool_exchange_adjacency(&result, options);
    if options.merge_consecutive_assistants {
        result = merge_consecutive_assistant_messages(result);
    }
    if options.drop_orphan_results {
        result = drop_orphan_tool_results(result);
    }
    if options.drop_leading_non_user {
        result = drop_leading_non_user_messages(result);
    }
    result
}

/// Whether a content part serializes to nothing the provider can read.
///
/// Text and thinking that are empty or whitespace carry no payload; media parts
/// always do. This is the distinction that keeps a "vacuous" assistant message
/// off the wire: an assistant message with no tool calls whose parts are all
/// vacuous serializes to an empty `assistant` message, which Moonshot and Z.AI
/// reject ("the message at position N with role 'assistant' must not be empty")
/// on every resend — permanently wedging the session.
fn part_is_vacuous(part: &ContentPart) -> bool {
    match part {
        ContentPart::Text { text } => text.trim().is_empty(),
        ContentPart::Think { think } => think.trim().is_empty(),
        ContentPart::ReasoningDetails { details } => details.is_empty(),
        ContentPart::ImageUrl { .. }
        | ContentPart::AudioUrl { .. }
        | ContentPart::VideoUrl { .. } => false,
    }
}

/// Whether a part contributes to the message's `content` field on the wire.
///
/// Reasoning is the exception, and the reason this is separate from
/// [`part_is_vacuous`]: a `Think` part serializes to `reasoning_content`, and a
/// `ReasoningDetails` part to `reasoning_details` — never to `content`. However
/// much reasoning a message holds, it fills `content` with nothing.
fn part_fills_content(part: &ContentPart) -> bool {
    !matches!(
        part,
        ContentPart::Think { .. } | ContentPart::ReasoningDetails { .. }
    ) && !part_is_vacuous(part)
}

/// Whether a message carries anything the provider will accept: at least one
/// part that fills `content`, or a tool call.
///
/// Interrupting a turn while the model is still streaming its reasoning seals an
/// assistant message holding a single *non-empty* `Think` part and nothing else.
/// It is not vacuous — there is real text in it — but it serializes to a message
/// with neither `content` nor `tool_calls`, which Moonshot and Z.AI reject on
/// every resend, wedging the session exactly as the empty-thinking case in
/// [`part_is_vacuous`] would. Judging by what reaches `content` covers both.
fn message_has_sendable_content(message: &Message) -> bool {
    // A system message may carry only tool definitions (`messages[].tools`).
    !message.tool_calls.is_empty()
        || !message.tools.is_empty()
        || message.content.iter().any(part_fills_content)
}

/// Prepare a message for projection by dropping empty/whitespace-only text
/// parts and messages that would serialize to nothing.
fn prepare_message_for_projection(message: &Message) -> Option<Message> {
    if message.partial {
        return None;
    }

    let mut cleaned = Vec::with_capacity(message.content.len());
    for part in &message.content {
        match part {
            ContentPart::Text { text } => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    if !text.is_empty() {
                        tracing::debug!("dropping whitespace-only text part during projection");
                    }
                    continue;
                }
                cleaned.push(part.clone());
            }
            // Non-text parts are kept verbatim — a content-bearing message keeps
            // its empty thinking blocks, which preserved-thinking providers
            // expect replayed alongside the real content.
            _ => cleaned.push(part.clone()),
        }
    }

    // The guard is by *content*, not part count: an assistant message left with
    // only an empty thinking part (e.g. a provider-filtered response) has a
    // non-empty `content` vec yet serializes to nothing, so counting parts let
    // it survive and wedge the session. Judge sendability instead.
    // Stripping the vacuous text parts above never changes this verdict, so the
    // original message is a fine input.
    if !message_has_sendable_content(message) {
        return None;
    }

    let mut out = message.clone();
    out.content = cleaned;
    Some(out)
}

fn can_merge_user_message(message: &Message) -> bool {
    message.role == Role::User && !message.partial
}

fn merge_two_user_messages(a: &Message, b: &Message) -> Message {
    let mut content = Vec::with_capacity(a.content.len() + b.content.len());

    // Combine all text parts with a blank line separator.
    let a_text = a.text_content();
    let b_text = b.text_content();
    let merged_text = format!("{a_text}\n\n{b_text}");
    if !merged_text.trim().is_empty() {
        content.push(ContentPart::text(merged_text));
    }

    // Append all non-text parts from both messages.
    for part in a.content.iter().chain(b.content.iter()) {
        if !part.is_text() {
            content.push(part.clone());
        }
    }

    Message {
        role: Role::User,
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        tools: Vec::new(),
        partial: false,
    }
}

fn merge_adjacent_user_messages(history: &[Message]) -> Vec<Message> {
    let mut result = Vec::with_capacity(history.len());
    for message in history {
        let Some(message) = prepare_message_for_projection(message) else {
            continue;
        };
        if can_merge_user_message(&message)
            && let Some(last) = result.last_mut()
            && can_merge_user_message(last)
        {
            let merged = merge_two_user_messages(last, &message);
            *last = merged;
            continue;
        }
        result.push(message);
    }
    result
}

fn dedupe_duplicate_tool_calls(history: Vec<Message>) -> Vec<Message> {
    let mut seen_ids = std::collections::HashSet::new();
    let mut result = Vec::with_capacity(history.len());

    for message in history {
        if message.role == Role::Assistant {
            let (kept, dropped): (Vec<_>, Vec<_>) = message
                .tool_calls
                .into_iter()
                .partition(|tc| seen_ids.insert(tc.id.clone()));

            if !dropped.is_empty() {
                tracing::debug!(
                    dropped = dropped.len(),
                    "dropped duplicate assistant tool_calls during strict projection"
                );
            }

            // Stripping every tool call as a duplicate can leave a remainder
            // whose only content is vacuous (an empty thinking part). Its
            // `content` vec is non-empty, but it serializes to an empty
            // assistant message the provider rejects — the very failure the
            // strict resend was recovering from. Drop it wholesale.
            if kept.is_empty() && !message.content.iter().any(part_fills_content) {
                if !message.content.is_empty() {
                    tracing::debug!(
                        "dropping duplicate-stripped assistant left with only vacuous content"
                    );
                }
                continue;
            }

            result.push(Message {
                tool_calls: kept,
                ..message
            });
        } else if message.role == Role::Tool {
            if let Some(id) = &message.tool_call_id {
                if seen_ids.insert(id.clone()) {
                    result.push(message);
                } else {
                    tracing::debug!(
                        tool_call_id = id.as_str(),
                        "dropped duplicate tool result during strict projection"
                    );
                }
            } else {
                result.push(message);
            }
        } else {
            result.push(message);
        }
    }

    result
}

/// Stands in for a tool call that was still running when a forked sub-agent
/// inherited the conversation — including the `agent` call that created the
/// fork, which by construction cannot have finished.
pub const INHERITED_IN_FLIGHT_TOOL_TEXT: &str = "This tool call was still executing when this conversation was inherited from the calling \
     agent, so its result is not part of this context. The outcome is unknown — do not assume it \
     succeeded or failed, and do not wait for it.";

/// Answer the trailing assistant message's unanswered tool calls with `text`.
///
/// A forked sub-agent inherits a conversation whose last assistant turn is
/// mid-exchange: it called `agent`, and that call is what produced the fork, so
/// it cannot have a result. Leaving it open would hand the child a history the
/// provider rejects; dropping the assistant message would erase the reasoning
/// that led to the fork. Closing it keeps both, and says plainly that the
/// outcome is unknown rather than letting the child read silence as failure.
///
/// Only the trailing exchange is closed. Anything unanswered earlier is a
/// different problem, and [`project`]'s `synthesize_missing` handles it with its
/// own wording.
#[must_use]
pub fn close_trailing_open_tool_exchange(history: &[Message], text: &str) -> Vec<Message> {
    let mut idx = history.len();
    while idx > 0 && history[idx - 1].role == Role::Tool {
        idx -= 1;
    }
    let Some(assistant) = idx.checked_sub(1).and_then(|i| history.get(i)) else {
        return history.to_vec();
    };
    if assistant.role != Role::Assistant || assistant.tool_calls.is_empty() {
        return history.to_vec();
    }

    let answered: std::collections::HashSet<&str> = history[idx..]
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    let open: Vec<&lm_bridge::message::ToolCall> = assistant
        .tool_calls
        .iter()
        .filter(|tc| !answered.contains(tc.id.as_str()))
        .collect();
    if open.is_empty() {
        return history.to_vec();
    }

    let mut out = history.to_vec();
    // A snapshot taken mid-stream can hold a partial assistant message. It is
    // settled by the act of being inherited: nothing more will be appended to it.
    if let Some(msg) = out.get_mut(idx - 1) {
        msg.partial = false;
    }
    for tc in open {
        out.push(Message::tool_result(&tc.id, text));
    }
    out
}

const MISSING_TOOL_RESULT_TEXT: &str = "Tool result is not available in the current context. Do not assume the tool completed successfully.";

fn repair_tool_exchange_adjacency(history: &[Message], options: &ProjectOptions) -> Vec<Message> {
    let n = history.len();
    let mut consumed = vec![false; n];
    let mut result = Vec::with_capacity(n);

    // Index of the last non-tool message in the original history; used to
    // decide whether an open assistant tool_call is "mid-history" or trailing.
    let last_non_tool_index = history
        .iter()
        .rposition(|m| m.role != Role::Tool)
        .unwrap_or(0);

    let mut i = 0;
    while i < n {
        if consumed[i] {
            i += 1;
            continue;
        }
        let message = &history[i];

        if message.role == Role::Assistant && !message.tool_calls.is_empty() {
            let assistant_index = i;
            result.push(message.clone());

            let mut pending: std::collections::HashSet<String> =
                message.tool_calls.iter().map(|tc| tc.id.clone()).collect();

            // Pull matching tool_results forward, in the order of the calls.
            for target_id in message.tool_calls.iter().map(|tc| tc.id.clone()) {
                if !pending.contains(&target_id) {
                    continue;
                }
                if let Some(j) =
                    find_next_tool_result(history, &consumed, assistant_index + 1, &target_id)
                {
                    consumed[j] = true;
                    pending.remove(&target_id);
                    result.push(history[j].clone());
                }
            }

            // Synthesize results for any calls that are still pending.
            let is_mid_history = assistant_index < last_non_tool_index;
            if options.synthesize_missing || is_mid_history {
                for tc in &message.tool_calls {
                    if pending.remove(&tc.id) {
                        tracing::debug!(
                            tool_call_id = tc.id.as_str(),
                            trailing = !is_mid_history,
                            "synthesizing missing tool result during projection"
                        );
                        result.push(Message::tool_result(&tc.id, MISSING_TOOL_RESULT_TEXT));
                    }
                }
            }

            i += 1;
        } else {
            result.push(message.clone());
            i += 1;
        }
    }

    result
}

fn find_next_tool_result(
    history: &[Message],
    consumed: &[bool],
    start: usize,
    target_id: &str,
) -> Option<usize> {
    history
        .iter()
        .enumerate()
        .skip(start)
        .find(|(j, m)| {
            !consumed[*j] && m.role == Role::Tool && m.tool_call_id.as_deref() == Some(target_id)
        })
        .map(|(j, _)| j)
}

fn merge_consecutive_assistant_messages(history: Vec<Message>) -> Vec<Message> {
    let mut result: Vec<Message> = Vec::with_capacity(history.len());
    for message in history {
        if message.role == Role::Assistant
            && let Some(last) = result.last_mut()
            && last.role == Role::Assistant
        {
            let mut content = std::mem::take(&mut last.content);
            content.extend(message.content);
            let mut tool_calls = std::mem::take(&mut last.tool_calls);
            tool_calls.extend(message.tool_calls);
            last.content = content;
            last.tool_calls = tool_calls;
            continue;
        }
        result.push(message);
    }
    result
}

fn drop_orphan_tool_results(history: Vec<Message>) -> Vec<Message> {
    let mut known_ids = std::collections::HashSet::new();
    for message in &history {
        if message.role == Role::Assistant {
            for tc in &message.tool_calls {
                known_ids.insert(tc.id.clone());
            }
        }
    }

    history
        .into_iter()
        .filter(|m| {
            if m.role == Role::Tool
                && let Some(id) = &m.tool_call_id
            {
                if known_ids.contains(id) {
                    return true;
                }
                tracing::debug!(
                    tool_call_id = id.as_str(),
                    "dropped orphan tool result during projection"
                );
                return false;
            }
            true
        })
        .collect()
}

fn drop_leading_non_user_messages(history: Vec<Message>) -> Vec<Message> {
    let first_user = history.iter().position(|m| m.role == Role::User);
    match first_user {
        Some(0) => history,
        Some(idx) => {
            tracing::debug!(dropped = idx, "dropped leading non-user messages");
            history.into_iter().skip(idx).collect()
        }
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lm_bridge::message::ToolCall;

    #[test]
    fn test_well_formed_history_unchanged() {
        let history = vec![Message::user("hello"), Message::assistant_text("hi")];
        let out = project(&history, &ProjectOptions::standard());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text_content(), "hello");
        assert_eq!(out[1].text_content(), "hi");
    }

    #[test]
    fn test_merge_adjacent_user_messages() {
        let history = vec![Message::user("a"), Message::user("b")];
        let out = project(&history, &ProjectOptions::standard());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text_content(), "a\n\nb");
    }

    #[test]
    fn test_reorder_tool_result_past_foreign_message() {
        let history = vec![
            Message::assistant_tool_calls(vec![ToolCall::new("tc1", "read_file")]),
            Message::user("wait"),
            Message::tool_result("tc1", "contents"),
        ];
        let out = project(&history, &ProjectOptions::standard());
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, Role::Assistant);
        assert_eq!(out[1].role, Role::Tool);
        assert_eq!(out[2].role, Role::User);
    }

    #[test]
    fn test_synthesize_missing_mid_history_result() {
        let history = vec![
            Message::assistant_tool_calls(vec![ToolCall::new("tc1", "read_file")]),
            Message::user("next"),
        ];
        let out = project(&history, &ProjectOptions::standard());
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, Role::Assistant);
        assert_eq!(out[1].role, Role::Tool);
        assert!(
            out[1]
                .text_content()
                .contains("Tool result is not available")
        );
        assert_eq!(out[2].role, Role::User);
    }

    #[test]
    fn test_trailing_open_tool_exchange_left_open_by_default() {
        let history = vec![Message::assistant_tool_calls(vec![ToolCall::new(
            "tc1",
            "read_file",
        )])];
        let out = project(&history, &ProjectOptions::standard());
        // standard() sets synthesize_missing=true, so it closes even trailing calls.
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].role, Role::Tool);
    }

    #[test]
    fn test_orphan_tool_result_dropped() {
        let history = vec![Message::tool_result("orphan", "result")];
        let out = project(&history, &ProjectOptions::standard());
        assert!(out.is_empty());
    }

    fn assistant(content: Vec<ContentPart>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        }
    }

    /// An assistant message whose only part is an empty/whitespace thinking block
    /// serializes to an empty assistant message the provider rejects on every
    /// resend. It must be dropped, not carried into the projected history.
    #[test]
    fn empty_thinking_only_assistant_is_dropped() {
        let history = vec![
            Message::user("hi"),
            assistant(vec![ContentPart::Think { think: "  ".into() }]),
            Message::user("next"),
        ];
        let out = project(&history, &ProjectOptions::standard());
        assert!(
            out.iter().all(message_has_sendable_content),
            "no vacuous assistant may reach the wire: {out:?}"
        );
        // With the empty assistant gone, the surrounding users merge.
        assert!(out.iter().all(|m| m.role == Role::User));
    }

    /// Stripping every tool call as a duplicate can leave an assistant whose only
    /// remaining content is an empty thinking part. That remainder must be dropped
    /// too, or the strict resend re-creates the exact empty message it was
    /// recovering from.
    #[test]
    fn duplicate_stripped_assistant_left_vacuous_is_dropped() {
        let dup = Message {
            tool_calls: vec![ToolCall::new("tc1", "read_file")],
            ..assistant(vec![ContentPart::Think {
                think: String::new(),
            }])
        };
        let history = vec![
            Message::assistant_tool_calls(vec![ToolCall::new("tc1", "read_file")]),
            Message::tool_result("tc1", "result"),
            dup,
            Message::user("continue"),
        ];
        let out = project(&history, &ProjectOptions::strict());
        assert!(
            out.iter().all(message_has_sendable_content),
            "the duplicate-stripped vacuous remainder must be dropped: {out:?}"
        );
    }

    /// A content-bearing message keeps its empty thinking block verbatim —
    /// preserved-thinking providers expect it replayed alongside the real content.
    #[test]
    fn content_bearing_message_keeps_empty_thinking_verbatim() {
        let msg = assistant(vec![
            ContentPart::Think {
                think: String::new(),
            },
            ContentPart::text("real answer"),
        ]);
        let prepared =
            prepare_message_for_projection(&msg).expect("a content-bearing message is kept");
        assert_eq!(
            prepared.content.len(),
            2,
            "the empty thinking block must survive alongside the text"
        );
        assert!(matches!(prepared.content[0], ContentPart::Think { .. }));
    }

    #[test]
    fn test_strict_projection_drops_leading_non_user() {
        let history = vec![
            Message::assistant_text("unprompted"),
            Message::user("hello"),
        ];
        let out = project(&history, &ProjectOptions::strict());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::User);
    }

    #[test]
    fn test_strict_projection_merges_consecutive_assistants() {
        let history = vec![
            Message::user("hello"),
            Message::assistant_text("part one"),
            Message::assistant_text("part two"),
        ];
        let out = project(&history, &ProjectOptions::strict());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, Role::User);
        assert_eq!(out[1].text_content(), "part onepart two");
    }

    /// Interrupting a turn mid-reasoning seals an assistant message holding a
    /// single *non-empty* thinking part and nothing else. It is not vacuous, so
    /// the empty-part guard alone would let it through — but it serializes to
    /// `reasoning_content` with no `content` and no `tool_calls`, which
    /// Moonshot and Z.AI reject on every resend.
    #[test]
    fn interrupted_thinking_only_assistant_is_dropped() {
        let history = vec![
            Message::user("hi"),
            assistant(vec![ContentPart::Think {
                think: "I should start by reading the".into(),
            }]),
            Message::user("next"),
        ];
        let out = project(&history, &ProjectOptions::standard());
        assert!(
            out.iter().all(message_has_sendable_content),
            "an interrupted thinking-only assistant must not reach the wire: {out:?}"
        );
        assert!(out.iter().all(|m| m.role == Role::User));
    }

    /// **Preserved Thinking must not be collateral damage.** Reasoning attached
    /// to real content or to tool calls is what both vendors require replayed,
    /// and every such message survives — only reasoning standing completely
    /// alone is dropped.
    #[test]
    fn thinking_alongside_content_or_tool_calls_always_survives() {
        let with_text = assistant(vec![
            ContentPart::Think {
                think: "let me explain".into(),
            },
            ContentPart::text("here is why"),
        ]);
        let with_tool_calls = Message {
            tool_calls: vec![ToolCall::new("tc9", "grep")],
            ..assistant(vec![ContentPart::Think {
                think: "I will search first".into(),
            }])
        };
        for msg in [&with_text, &with_tool_calls] {
            let prepared = prepare_message_for_projection(msg)
                .expect("reasoning attached to real payload must survive");
            assert!(
                matches!(prepared.content[0], ContentPart::Think { .. }),
                "the reasoning block must be replayed verbatim"
            );
        }

        let history = vec![
            Message::user("go"),
            with_tool_calls,
            Message::tool_result("tc9", "found"),
        ];
        assert_eq!(
            project(&history, &ProjectOptions::standard()).len(),
            3,
            "nothing may be dropped"
        );
    }

    // ---- fork: closing the trailing open exchange -------------------------

    fn closed(history: &[Message]) -> Vec<Message> {
        close_trailing_open_tool_exchange(history, INHERITED_IN_FLIGHT_TOOL_TEXT)
    }

    /// **The call that creates the fork can never have a result.** The child
    /// inherits a history whose last assistant turn called `agent`; leaving that
    /// open hands it a message list the provider rejects, and dropping the
    /// assistant message would erase the reasoning that led to the fork.
    #[test]
    fn the_in_flight_call_is_answered_rather_than_dropped() {
        let history = vec![
            Message::user("find the bug"),
            Message {
                tool_calls: vec![ToolCall::new("tc-fork", "agent")],
                ..assistant(vec![ContentPart::text("delegating")])
            },
        ];
        let out = closed(&history);

        assert_eq!(
            out.len(),
            3,
            "the exchange is closed, not truncated: {out:?}"
        );
        assert_eq!(
            out[1].tool_calls.len(),
            1,
            "the assistant's call must survive"
        );
        assert_eq!(out[2].tool_call_id.as_deref(), Some("tc-fork"));
        assert!(
            out[2].text_content().contains("outcome is unknown"),
            "the child must not read silence as failure: {}",
            out[2].text_content()
        );
    }

    /// Calls that *did* answer keep their real results.
    #[test]
    fn only_the_unanswered_calls_are_filled_in() {
        let history = vec![
            Message::user("go"),
            Message {
                tool_calls: vec![
                    ToolCall::new("tc-done", "read_file"),
                    ToolCall::new("tc-open", "agent"),
                ],
                ..assistant(vec![ContentPart::text("two calls")])
            },
            Message::tool_result("tc-done", "file contents"),
        ];
        let out = closed(&history);
        assert_eq!(out.len(), 4);
        assert_eq!(
            out[2].text_content(),
            "file contents",
            "the real result stands"
        );
        assert_eq!(out[3].tool_call_id.as_deref(), Some("tc-open"));
    }

    /// A history with nothing open is returned untouched.
    #[test]
    fn a_settled_history_is_unchanged() {
        for history in [
            vec![Message::user("hi"), Message::assistant_text("hello")],
            Vec::new(),
        ] {
            assert_eq!(closed(&history), history, "{history:?}");
        }
    }

    /// A snapshot taken mid-stream can hold a partial assistant message. Being
    /// inherited settles it, and leaving it partial would have projection drop
    /// it — taking the tool calls with it.
    #[test]
    fn a_partial_assistant_is_settled_by_being_inherited() {
        let history = vec![
            Message::user("go"),
            Message {
                partial: true,
                tool_calls: vec![ToolCall::new("tc-open", "agent")],
                ..assistant(vec![ContentPart::text("mid-stream")])
            },
        ];
        let out = closed(&history);
        assert!(!out[1].partial, "the inherited assistant must be settled");
        assert!(
            project(&out, &ProjectOptions::standard())
                .iter()
                .any(|m| !m.tool_calls.is_empty()),
            "the tool call must reach the child"
        );
    }
}
