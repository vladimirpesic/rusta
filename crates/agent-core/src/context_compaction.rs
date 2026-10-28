//! Context compaction helpers: the shape a compacted history takes, and the
//! texts that frame it.
//!
//! A compacted history keeps the user's own messages (the oldest and the most
//! recent, within a budget), then the model's handoff summary, then a
//! continuation anchor telling the model to resume the latest request.

use std::path::{Path, PathBuf};

use lm_bridge::message::{ContentPart, Message, Role};

/// Maximum token budget for the pool of genuine user messages kept after
/// compaction. Once the pool exceeds this, a head/tail split is applied.
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: u64 = 20_000;
/// Tokens reserved for the oldest user messages when the pool no longer fits.
pub const COMPACT_USER_MESSAGE_HEAD_TOKENS: u64 = 2_000;
/// Estimated token cost for an image/audio/video content part.
pub const MEDIA_TOKEN_ESTIMATE: u64 = 2_000;

/// Prefix inserted before a compaction summary so the model recognizes it as
/// notes rather than a new user turn.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation so far has been compacted to free up context. What follows is your own working summary of this task — use it to continue your train of thought rather than starting over. Treat it as notes, not proof: where it says a step was done, tests passed, or a fix worked, verify that yourself before relying on it. Any user messages earlier in this context are preserved verbatim from the compacted conversation; where a system-reminder note among them marks an omitted middle section, the user messages it replaced are covered by this summary. The summary records which earlier requests were already addressed.";

/// The last message of a compacted history. Without it the summary is the
/// final message, and a model can read it as the task's conclusion rather
/// than as notes to work from.
pub const COMPACTION_CONTINUATION_TEXT: &str = "<system-reminder>\nContext compaction is complete — continue the work that was in progress when it began.\n</system-reminder>";

/// The handoff instruction for the model that writes the summary, with
/// `{records_note}` and `{todo_note}` placeholders. See
/// [`render_compaction_instruction`].
const COMPACTION_INSTRUCTION_TEMPLATE: &str = include_str!("compaction_instruction.md");

/// Selection produced by `select_compaction_user_messages`.
#[derive(Debug, Clone)]
pub struct CompactionUserSelection {
    pub head: Vec<Message>,
    pub tail: Vec<Message>,
    pub elided: bool,
    pub omitted_tokens: u64,
}

impl CompactionUserSelection {
    #[must_use]
    pub fn kept_tokens(&self) -> u64 {
        self.head
            .iter()
            .chain(self.tail.iter())
            .map(estimate_tokens_for_message)
            .sum()
    }
}

/// Whether a message counts as genuine user input that should survive
/// compaction: a user-role message that is neither a compaction summary nor
/// one of Rusta's own injections ([`is_injection`]).
pub fn is_real_user_input(message: &Message) -> bool {
    message.role == Role::User && !is_injection(message) && !is_compaction_summary_message(message)
}

/// Whether a user-role message was injected by Rusta rather than written by
/// the user: a `<system-reminder>` (compaction markers, cut-off notices, the
/// continuation anchor) or a background-task `<notification>`.
pub fn is_injection(message: &Message) -> bool {
    if message.role != Role::User {
        return false;
    }
    let text = message.text_content();
    let text = text.trim_start();
    text.starts_with("<system-reminder>") || text.starts_with("<notification ")
}

pub fn collect_compactable_user_messages(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter(|m| is_real_user_input(m) && !is_compaction_summary_message(m))
        .cloned()
        .collect()
}

/// Whether `message` is a compaction summary.
pub fn is_compaction_summary_message(message: &Message) -> bool {
    message.role == Role::User
        && message
            .text_content()
            .starts_with(COMPACTION_SUMMARY_PREFIX)
}

/// Estimate tokens for a text string with a cheap heuristic: ASCII
/// characters cost ~1/4 token each; non-ASCII code points cost 1 token each.
#[must_use]
pub fn estimate_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}

#[must_use]
pub fn estimate_tokens_for_message(message: &Message) -> u64 {
    let role_tokens = estimate_tokens(match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    });

    let content_tokens: u64 = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => estimate_tokens(text),
            ContentPart::Think { think } => estimate_tokens(think),
            ContentPart::ImageUrl { .. }
            | ContentPart::AudioUrl { .. }
            | ContentPart::VideoUrl { .. } => MEDIA_TOKEN_ESTIMATE,
            // Echoed back verbatim to the gateway that sent it, so it costs
            // what its serialized form costs.
            ContentPart::ReasoningDetails { details } => {
                estimate_tokens(&serde_json::Value::from(details.clone()).to_string())
            }
        })
        .sum();

    let tool_tokens: u64 = message
        .tool_calls
        .iter()
        .map(|tc| {
            let args_len = tc.arguments.as_ref().map_or(0, |s| s.len() as u64);
            estimate_tokens(&tc.name) + args_len + estimate_tokens(&tc.id)
        })
        .sum();

    let message_tool_tokens: u64 = message
        .tools
        .iter()
        .map(|t| estimate_tokens(&t.name) + estimate_tokens(&t.description))
        .sum();

    role_tokens + content_tokens + tool_tokens + message_tool_tokens
}

/// Keep the most recent user messages whose cumulative estimated size fits
/// `max_tokens`. The oldest kept message is truncated from the beginning to
/// fit the remaining budget; older messages are dropped.
#[must_use]
pub fn select_recent_user_messages(messages: &[Message], max_tokens: u64) -> Vec<Message> {
    if max_tokens == 0 {
        return Vec::new();
    }

    let mut kept = Vec::new();
    let mut remaining = max_tokens;

    for message in messages.iter().rev() {
        let cost = estimate_tokens_for_message(message);
        if cost <= remaining {
            kept.push(message.clone());
            remaining -= cost;
        } else {
            let text = message.text_content();
            let truncated = truncate_text_to_tokens_from_end(&text, remaining);
            if !truncated.is_empty() {
                kept.push(replace_message_text(message, truncated));
            }
            break;
        }
    }

    kept.reverse();
    kept
}

/// Preserve both the oldest and the most recent user messages, inserting an
/// elision marker conceptually between them.
#[must_use]
pub fn select_compaction_user_messages(
    messages: &[Message],
    max_tokens: u64,
    head_tokens: u64,
) -> CompactionUserSelection {
    let total_tokens: u64 = messages.iter().map(estimate_tokens_for_message).sum();

    if total_tokens <= max_tokens {
        return CompactionUserSelection {
            head: Vec::new(),
            tail: messages.to_vec(),
            elided: false,
            omitted_tokens: 0,
        };
    }

    let head_budget = head_tokens.min(max_tokens);
    let tail_budget = max_tokens.saturating_sub(head_budget);

    // Build tail from the end, truncating the boundary message from the end.
    let mut tail = Vec::new();
    let mut remaining_tail = tail_budget;
    let mut tail_boundary_dropped_prefix: Option<Message> = None;
    let mut tail_boundary_index: Option<usize> = None;

    for (idx, message) in messages.iter().enumerate().rev() {
        let cost = estimate_tokens_for_message(message);
        if cost <= remaining_tail {
            tail.push(message.clone());
            remaining_tail -= cost;
        } else {
            let text = message.text_content();
            let truncated = truncate_text_to_tokens_from_end(&text, remaining_tail);
            if !truncated.is_empty() {
                tail.push(replace_message_text(message, truncated.clone()));
            }
            // The prefix of this message that was discarded is still eligible
            // for the head budget.
            let kept_len = truncated.len();
            let dropped_prefix = text[..text.len().saturating_sub(kept_len)].to_string();
            if !dropped_prefix.is_empty() {
                tail_boundary_dropped_prefix = Some(replace_message_text(message, dropped_prefix));
            }
            tail_boundary_index = Some(idx);
            break;
        }
    }

    tail.reverse();

    // Build head from the start, up to the boundary message, including its
    // dropped prefix.
    let mut head = Vec::new();
    let mut remaining_head = head_budget;
    let head_end = tail_boundary_index.unwrap_or(messages.len());

    for message in messages.iter().take(head_end) {
        let cost = estimate_tokens_for_message(message);
        if cost <= remaining_head {
            head.push(message.clone());
            remaining_head -= cost;
        } else {
            let text = message.text_content();
            let truncated = truncate_text_to_tokens(&text, remaining_head);
            if !truncated.is_empty() {
                head.push(replace_message_text(message, truncated));
            }
            remaining_head = 0;
            break;
        }
    }

    if let Some(prefix) = tail_boundary_dropped_prefix {
        let text = prefix.text_content();
        let budget_left = remaining_head;
        let truncated = truncate_text_to_tokens(&text, budget_left);
        if !truncated.is_empty() {
            head.push(replace_message_text(&prefix, truncated));
        }
    }

    let kept_tokens = head
        .iter()
        .chain(tail.iter())
        .map(estimate_tokens_for_message)
        .sum::<u64>();
    let omitted_tokens = total_tokens.saturating_sub(kept_tokens);

    CompactionUserSelection {
        head,
        tail,
        elided: true,
        omitted_tokens,
    }
}

fn replace_message_text(message: &Message, text: impl Into<String>) -> Message {
    let mut out = message.clone();
    out.content = vec![ContentPart::text(text)];
    out.tool_calls = Vec::new();
    out
}

fn truncate_text_to_tokens(text: &str, max_tokens: u64) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    let mut end = 0;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
        if ascii.div_ceil(4) + non_ascii > max_tokens {
            break;
        }
        end += ch.len_utf8();
    }
    text[..end].to_string()
}

fn truncate_text_to_tokens_from_end(text: &str, max_tokens: u64) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    let mut start = text.len();

    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for &(pos, ch) in chars.iter().rev() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
        if ascii.div_ceil(4) + non_ascii > max_tokens {
            break;
        }
        start = pos;
    }

    text[start..].to_string()
}

/// Build the elision marker that sits between the head and tail segments.
#[must_use]
pub fn build_compaction_elision_text(omitted_tokens: u64) -> String {
    format!(
        "<system-reminder>\nSome of this conversation's user messages were omitted here during \
         compaction: the messages above this note are the oldest user input, the messages below \
         are the most recent, and roughly {omitted_tokens} tokens in between were dropped. The \
         omitted content is covered by the compaction summary at the end of the conversation.\n\
         </system-reminder>"
    )
}

/// Whether `message` is one of the markers [`apply_compaction`] generates (the
/// elision note or the continuation anchor). Replaying a transcript
/// regenerates them from the summary, so recorded copies are skipped.
#[must_use]
pub fn is_compaction_marker(message: &Message) -> bool {
    if message.role != Role::User {
        return false;
    }
    let text = message.text_content();
    text == COMPACTION_CONTINUATION_TEXT
        || text.starts_with(
            "<system-reminder>\nSome of this conversation's user messages were omitted here during",
        )
}

/// Where a compaction's source messages can still be read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordsWindow {
    /// The session's records file (JSONL, one record per line).
    pub path: PathBuf,
    /// First and last line (1-based, inclusive) this compaction replaced.
    pub first_line: usize,
    pub last_line: usize,
    /// Where tool results that were moved out of the records file live.
    pub blob_dir: Option<PathBuf>,
}

/// The pointer appended to a summary: where the replaced messages still are,
/// and how to read them.
#[must_use]
pub fn build_records_pointer(window: &RecordsWindow) -> String {
    let blobs = window.blob_dir.as_deref().map_or_else(String::new, |dir| {
        format!(
            " Tool results longer than 2,000 characters appear there as `<blob id=\"ID\"/>`; \
             the full text is in {}/ID.",
            dir.display()
        )
    });
    format!(
        "## Context recovery\n\
         Everything before this summary is still on disk in this session's records file \
         (append-only, one JSON record per line): {}\n\
         This compaction replaced lines {}–{}. If you need exact command output, file contents, \
         error text or the wording of an earlier request, look it up there instead of guessing: \
         grep the file for a keyword to find the line, then read just that line with read_file \
         (offset=<line>, limit=1), following the continuation note's column_offset when a line \
         is long. Each line holds one message under `event.data` (`role`, `content`, \
         `tool_calls`).{blobs}",
        window.path.display(),
        window.first_line,
        window.last_line,
    )
}

/// The notice left where the fallback eviction removed messages.
#[must_use]
pub fn build_removal_notice(removed: usize, records: Option<&Path>) -> String {
    let pointer = records.map_or_else(String::new, |path| {
        format!(
            " They are still in the session's records file, one JSON record per line: {}.",
            path.display()
        )
    });
    format!(
        "<system-reminder>\n{removed} earlier messages were removed to free context; details of \
         that work are no longer available.{pointer}\n</system-reminder>"
    )
}

/// The handoff instruction for the summarizing model.
///
/// `has_records` says whether a records pointer will follow the summary, and
/// `has_todos` whether the live todo list will be re-attached; the
/// instruction only promises what will actually be there. `custom_instruction`
/// is `/compact <instruction>`'s focus text, appended verbatim as an
/// "Optional user instruction" block at the end.
#[must_use]
pub fn render_compaction_instruction(
    has_records: bool,
    has_todos: bool,
    custom_instruction: Option<&str>,
) -> String {
    let records_note = if has_records {
        "This session's records file stays on disk, and a pointer to it is appended below \
         this summary automatically, so you need not reproduce long outputs verbatim — keep \
         exact identifiers, key values and error lines, and name anything the next turn should \
         look up.\n\n"
    } else {
        ""
    };
    let todo_note = if has_todos {
        "Your todo list is re-attached automatically below this summary from its live source, \
         so do not transcribe it — copying it wastes space and can contradict the live version. \
         What that list cannot hold is the reasoning between tasks — why one was reordered or \
         dropped, or a decision on one that constrains another — so record that instead.\n\n"
    } else {
        ""
    };
    COMPACTION_INSTRUCTION_TEMPLATE
        .replace("{records_note}", records_note)
        .replace("{todo_note}", todo_note)
        .trim_end()
        .to_string()
        + &custom_instruction
            .filter(|custom| !custom.is_empty())
            .map_or_else(String::new, |custom| {
                format!("\n\nOptional user instruction:\n{custom}")
            })
}

/// Wrap a summary with the compaction-summary prefix.
#[must_use]
pub fn build_compaction_summary_text(summary: &str) -> String {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        format!("{COMPACTION_SUMMARY_PREFIX}\n(no summary available)")
    } else {
        format!("{COMPACTION_SUMMARY_PREFIX}\n{trimmed}")
    }
}

/// The compacted form of `messages`: the user's own messages (head and tail
/// within `max_tokens`, with an elision note between them when some were
/// dropped), then the summary message `summary_text`, then the continuation
/// anchor.
///
/// `summary_text` is the whole summary message, prefix included (see
/// [`build_compaction_summary_text`]).
#[must_use]
pub fn apply_compaction(
    messages: &[Message],
    summary_text: &str,
    max_tokens: u64,
    head_tokens: u64,
) -> Vec<Message> {
    let user_messages = collect_compactable_user_messages(messages);
    let selection = select_compaction_user_messages(&user_messages, max_tokens, head_tokens);

    let mut compacted = Vec::with_capacity(selection.head.len() + selection.tail.len() + 3);
    compacted.extend(selection.head);
    if selection.elided {
        compacted.push(Message::user(build_compaction_elision_text(
            selection.omitted_tokens,
        )));
    }
    compacted.extend(selection.tail);
    compacted.push(Message::user(summary_text));
    compacted.push(Message::user(COMPACTION_CONTINUATION_TEXT));
    compacted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(text: &str) -> Message {
        Message::user(text)
    }

    #[test]
    fn test_estimate_tokens_ascii_vs_non_ascii() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("中文"), 2);
    }

    #[test]
    fn test_head_tail_selection() {
        let messages: Vec<Message> = (0..5).map(|_| user_msg(&"a".repeat(100))).collect();
        // Each 100-ASCII message costs 25 tokens.
        let selection = select_compaction_user_messages(&messages, 75, 25);
        assert!(selection.elided);
        assert_eq!(selection.head.len(), 1);
        assert_eq!(selection.tail.len(), 2);
        assert!(selection.omitted_tokens > 0);
    }

    #[test]
    fn test_small_pool_not_elided() {
        let messages = vec![user_msg("hello"), user_msg("world")];
        let selection = select_compaction_user_messages(&messages, 100, 10);
        assert!(!selection.elided);
        assert_eq!(selection.tail.len(), 2);
        assert!(selection.head.is_empty());
    }

    #[test]
    fn test_tail_only_selection() {
        let messages: Vec<Message> = (0..3).map(|i| user_msg(&format!("msg{i}"))).collect();
        let tail = select_recent_user_messages(&messages, 2);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].text_content(), "msg2");
    }

    #[test]
    fn test_compaction_summary_text_wraps_prefix() {
        let text = build_compaction_summary_text("did the thing");
        assert!(text.starts_with(COMPACTION_SUMMARY_PREFIX));
        assert!(text.ends_with("did the thing"));
    }

    /// The compacted shape: the user's messages, then the summary, then the
    /// anchor — last, so the model resumes the work rather than reading the
    /// summary as a finished answer.
    #[test]
    fn test_apply_compaction_keeps_user_messages_then_summary_then_anchor() {
        let messages = vec![
            user_msg("first"),
            Message::assistant_text("answer"),
            Message::tool_result("tc", "result"),
            user_msg("<system-reminder>\ninjected\n</system-reminder>"),
            user_msg("second"),
        ];
        let summary = build_compaction_summary_text("summary");
        let compacted = apply_compaction(&messages, &summary, 10_000, 2_000);
        let texts: Vec<String> = compacted.iter().map(Message::text_content).collect();
        assert_eq!(
            texts,
            [
                "first",
                "second",
                summary.as_str(),
                COMPACTION_CONTINUATION_TEXT
            ]
        );
        assert!(compacted.iter().all(|m| m.role == Role::User));
    }

    #[test]
    fn test_injections_are_not_real_user_input() {
        assert!(is_real_user_input(&user_msg("fix the bug")));
        assert!(!is_real_user_input(&user_msg(COMPACTION_CONTINUATION_TEXT)));
        assert!(!is_real_user_input(&user_msg(
            "<notification category=\"task\">done</notification>"
        )));
        assert!(!is_real_user_input(&user_msg(
            &build_compaction_summary_text("notes")
        )));
        assert!(is_compaction_marker(&user_msg(
            COMPACTION_CONTINUATION_TEXT
        )));
        assert!(is_compaction_marker(&user_msg(
            &build_compaction_elision_text(5)
        )));
        assert!(!is_compaction_marker(&user_msg("hello")));
    }

    /// The instruction promises a records pointer and a todo list only when
    /// they will be attached.
    #[test]
    fn test_instruction_mentions_only_what_is_attached() {
        let bare = render_compaction_instruction(false, false, None);
        assert!(bare.starts_with("You are about to run out of context."));
        assert!(!bare.contains("records file"));
        assert!(!bare.contains("todo list is re-attached"));
        assert!(!bare.contains("Optional user instruction"));
        assert!(!bare.contains('{'), "no placeholder is left: {bare}");
        let full = render_compaction_instruction(true, true, None);
        assert!(full.contains("records file stays on disk"));
        assert!(full.contains("todo list is re-attached"));
    }

    /// `/compact <instruction>`'s focus text reaches the summarizer as an
    /// "Optional user instruction" block at the end of the handoff
    /// instruction.
    #[test]
    fn test_instruction_carries_the_compact_focus_text() {
        let text = render_compaction_instruction(false, false, Some("keep the auth notes"));
        assert!(
            text.ends_with("Optional user instruction:\nkeep the auth notes"),
            "{text}"
        );
        // The template itself is untouched apart from the appended block.
        let bare = render_compaction_instruction(false, false, None);
        assert_eq!(
            text.len(),
            bare.len() + "\n\nOptional user instruction:\nkeep the auth notes".len()
        );
    }

    #[test]
    fn test_records_pointer_names_the_file_and_range() {
        let pointer = build_records_pointer(&RecordsWindow {
            path: PathBuf::from("/data/records.jsonl"),
            first_line: 3,
            last_line: 40,
            blob_dir: Some(PathBuf::from("/data/blobs")),
        });
        assert!(pointer.contains("/data/records.jsonl"));
        assert!(pointer.contains("lines 3–40"));
        assert!(pointer.contains("/data/blobs/ID"));
    }

    #[test]
    fn test_removal_notice_counts_and_points() {
        let notice = build_removal_notice(7, Some(Path::new("/data/records.jsonl")));
        assert!(notice.starts_with("<system-reminder>\n7 earlier messages were removed"));
        assert!(notice.contains("/data/records.jsonl"));
        assert!(!build_removal_notice(1, None).contains("records file"));
    }
}
