//! What the next request to the model holds, part by part: the view behind
//! `/context`.
//!
//! The request is measured as a step sends it: the system prompt, the tool
//! table, and the conversation, projected, with spilled tool results read
//! back in. Each part is estimated as the rest of Rusta estimates tokens
//! ([`crate::context_compaction::estimate_tokens`]), so the figures are
//! close, not exact; a provider's tokenizer has the last word.

use std::collections::HashMap;

use lm_bridge::message::{ContentPart, Message, Role};
use lm_bridge::tool::Tool;

use crate::context_compaction::{
    MEDIA_TOKEN_ESTIMATE, estimate_tokens, estimate_tool_tokens, is_compaction_summary_message,
};

/// Where a tool in the table comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    /// One of Rusta's own tools.
    BuiltIn,
    /// An MCP server's, by the server part of its name (`mcp__<server>__<tool>`).
    Mcp(String),
}

impl ToolSource {
    /// Where the tool named `name` comes from.
    #[must_use]
    pub fn of(name: &str) -> Self {
        name.strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .map_or(Self::BuiltIn, |(server, _)| Self::Mcp(server.to_string()))
    }
}

/// What a part of the request is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartKind {
    /// The system prompt.
    SystemPrompt,
    /// The tools of one source in the request's tool table.
    Tools(ToolSource),
    /// Tools `select_tools` loaded that the model receives on system messages
    /// rather than in the tool table.
    LoadedTools,
    /// The names and descriptions of the tools the model may load.
    ToolAnnouncements,
    /// What the user wrote, scheduled prompts and skills included.
    UserMessages,
    /// What Rusta adds to the conversation: `<system-reminder>` notes, the git
    /// context, goal reminders.
    Notes,
    /// The model's visible replies.
    Replies,
    /// The model's reasoning, as the request carries it back.
    Thinking,
    /// The model's tool calls with their arguments.
    ToolCalls,
    /// The results of one tool's calls.
    ToolResults(String),
    /// The summary that replaced compacted history.
    CompactionSummary,
    /// Images, audio and video.
    Media,
}

/// One part of the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPart {
    pub kind: PartKind,
    /// How many tools, messages, notes, calls, results or media items.
    pub items: usize,
    pub tokens: u64,
}

/// What the next request holds.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextUsage {
    /// The parts in the order the request carries them, each tool source and
    /// each tool's results largest first. Empty parts are left out.
    pub parts: Vec<ContextPart>,
    /// The tools the model's cap on tools per request leaves out of the table.
    pub tools_left_out: Vec<String>,
    /// The context window.
    pub window: u64,
    /// Where compaction starts, on [`Self::compaction_measure`]'s scale.
    pub compaction_at: u64,
    /// The size compaction is decided on: the system prompt and the
    /// conversation, without the tool table
    /// ([`crate::context::ContextMemory::estimate_tokens`]).
    pub compaction_measure: u64,
    /// How the model counted the requests it was sent against Rusta's
    /// estimate of them ([`crate::context::ContextMemory::calibrate`]). Every
    /// figure here is scaled by it; `None` until a reply has taught it.
    pub token_scale: Option<f64>,
}

impl ContextUsage {
    /// The whole request, in estimated tokens.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.parts.iter().map(|part| part.tokens).sum()
    }
}

/// The parts of the request made of `history` (as
/// [`crate::context::ContextMemory::build_history`] builds it, the system
/// prompt first) and the tool table `tools`.
#[must_use]
pub fn measure(history: &[Message], tools: &[Tool]) -> Vec<ContextPart> {
    let mut tally = Tally::default();
    let mut sources: Vec<(ToolSource, usize, u64)> = Vec::new();
    for tool in tools {
        let source = ToolSource::of(&tool.name);
        let tokens = estimate_tool_tokens(tool);
        match sources.iter_mut().find(|(known, ..)| *known == source) {
            Some((_, items, total)) => {
                *items += 1;
                *total += tokens;
            }
            None => sources.push((source, 1, tokens)),
        }
    }

    let mut tool_names: HashMap<&str, &str> = HashMap::new();
    let mut results: Vec<(String, usize, u64)> = Vec::new();
    for (index, message) in history.iter().enumerate() {
        match message.role {
            Role::System if index == 0 => {
                tally.add(Slot::SystemPrompt, estimate_tokens(&message.text_content()));
            }
            Role::System => {
                if !message.tools.is_empty() {
                    tally.add_many(
                        Slot::LoadedTools,
                        message.tools.len(),
                        message.tools.iter().map(estimate_tool_tokens).sum(),
                    );
                }
                let text = message.text_content();
                if text.contains("<tools_added>") || text.contains("<tools_removed>") {
                    tally.add(Slot::ToolAnnouncements, estimate_tokens(&text));
                } else if !text.trim().is_empty() {
                    tally.add(Slot::Notes, estimate_tokens(&text));
                }
            }
            Role::User if is_compaction_summary_message(message) => {
                tally.add(
                    Slot::CompactionSummary,
                    estimate_tokens(&message.text_content()),
                );
            }
            Role::User => {
                let text = message.text_content();
                let (notes, own) = split_notes(&text);
                for note in notes {
                    tally.add(Slot::Notes, estimate_tokens(note));
                }
                if !own.trim().is_empty() {
                    tally.add(Slot::UserMessages, estimate_tokens(&own));
                }
            }
            Role::Assistant => {
                let mut reply = 0;
                let mut thinking = 0;
                for part in &message.content {
                    match part {
                        ContentPart::Text { text } => reply += estimate_tokens(text),
                        ContentPart::Think { think } => thinking += estimate_tokens(think),
                        ContentPart::ReasoningDetails { details } => {
                            thinking += estimate_tokens(
                                &serde_json::Value::from(details.clone()).to_string(),
                            );
                        }
                        _ => {}
                    }
                }
                if reply > 0 {
                    tally.add(Slot::Replies, reply);
                }
                if thinking > 0 {
                    tally.add(Slot::Thinking, thinking);
                }
                for call in &message.tool_calls {
                    tool_names.insert(&call.id, &call.name);
                    tally.add(
                        Slot::ToolCalls,
                        estimate_tokens(&call.name)
                            + call.arguments.as_deref().map_or(0, estimate_tokens)
                            + estimate_tokens(&call.id),
                    );
                }
            }
            Role::Tool => {
                let tool = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| tool_names.get(id).copied())
                    .unwrap_or("unknown");
                let tokens = estimate_tokens(&message.text_content());
                match results.iter_mut().find(|(name, ..)| name == tool) {
                    Some((_, items, total)) => {
                        *items += 1;
                        *total += tokens;
                    }
                    None => results.push((tool.to_string(), 1, tokens)),
                }
            }
        }
        let media = message.content.iter().filter(|part| is_media(part)).count();
        if media > 0 {
            tally.add_many(Slot::Media, media, media as u64 * MEDIA_TOKEN_ESTIMATE);
        }
    }

    sources.sort_by_key(|&(_, _, tokens)| std::cmp::Reverse(tokens));
    results.sort_by_key(|&(_, _, tokens)| std::cmp::Reverse(tokens));
    let mut parts = Vec::new();
    parts.extend(tally.part(Slot::SystemPrompt));
    parts.extend(
        sources
            .into_iter()
            .map(|(source, items, tokens)| ContextPart {
                kind: PartKind::Tools(source),
                items,
                tokens,
            }),
    );
    for slot in [
        Slot::LoadedTools,
        Slot::ToolAnnouncements,
        Slot::CompactionSummary,
        Slot::UserMessages,
        Slot::Notes,
        Slot::Replies,
        Slot::Thinking,
        Slot::ToolCalls,
    ] {
        parts.extend(tally.part(slot));
    }
    parts.extend(
        results
            .into_iter()
            .map(|(tool, items, tokens)| ContextPart {
                kind: PartKind::ToolResults(tool),
                items,
                tokens,
            }),
    );
    parts.extend(tally.part(Slot::Media));
    parts
}

/// Whether `part` is an image, audio or a video.
fn is_media(part: &ContentPart) -> bool {
    matches!(
        part,
        ContentPart::ImageUrl { .. } | ContentPart::AudioUrl { .. } | ContentPart::VideoUrl { .. }
    )
}

/// The parts that are counted by kind alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    SystemPrompt,
    LoadedTools,
    ToolAnnouncements,
    CompactionSummary,
    UserMessages,
    Notes,
    Replies,
    Thinking,
    ToolCalls,
    Media,
}

impl Slot {
    fn kind(self) -> PartKind {
        match self {
            Self::SystemPrompt => PartKind::SystemPrompt,
            Self::LoadedTools => PartKind::LoadedTools,
            Self::ToolAnnouncements => PartKind::ToolAnnouncements,
            Self::CompactionSummary => PartKind::CompactionSummary,
            Self::UserMessages => PartKind::UserMessages,
            Self::Notes => PartKind::Notes,
            Self::Replies => PartKind::Replies,
            Self::Thinking => PartKind::Thinking,
            Self::ToolCalls => PartKind::ToolCalls,
            Self::Media => PartKind::Media,
        }
    }
}

/// Items and tokens per [`Slot`].
#[derive(Default)]
struct Tally(Vec<(Slot, usize, u64)>);

impl Tally {
    fn add(&mut self, slot: Slot, tokens: u64) {
        self.add_many(slot, 1, tokens);
    }

    fn add_many(&mut self, slot: Slot, items: usize, tokens: u64) {
        match self.0.iter_mut().find(|(known, ..)| *known == slot) {
            Some((_, count, total)) => {
                *count += items;
                *total += tokens;
            }
            None => self.0.push((slot, items, tokens)),
        }
    }

    fn part(&self, slot: Slot) -> Option<ContextPart> {
        self.0
            .iter()
            .find(|(known, ..)| *known == slot)
            .map(|&(_, items, tokens)| ContextPart {
                kind: slot.kind(),
                items,
                tokens,
            })
    }
}

/// The notes Rusta put in a user message's `text` — `<system-reminder>` and
/// `<git-context>` blocks — and the text around them, which is the user's.
fn split_notes(text: &str) -> (Vec<&str>, String) {
    const BLOCKS: [(&str, &str); 2] = [
        ("<system-reminder>", "</system-reminder>"),
        ("<git-context>", "</git-context>"),
    ];
    let mut notes = Vec::new();
    let mut own = String::new();
    let mut rest = text;
    loop {
        let next = BLOCKS
            .iter()
            .filter_map(|&(open, close)| rest.find(open).map(|at| (at, close)))
            .min_by_key(|&(at, _)| at);
        let Some((start, close)) = next else {
            own.push_str(rest);
            break;
        };
        own.push_str(&rest[..start]);
        let block = &rest[start..];
        let end = block.find(close).map_or(block.len(), |at| at + close.len());
        notes.push(&block[..end]);
        rest = &block[end..];
    }
    (notes, own)
}

#[cfg(test)]
mod tests {
    use lm_bridge::message::ToolCall;

    use super::*;

    fn tool(name: &str) -> Tool {
        Tool::new(
            name,
            "Does one thing.",
            serde_json::json!({"type": "object", "properties": {}}),
        )
    }

    fn part(parts: &[ContextPart], kind: &PartKind) -> Option<(usize, u64)> {
        parts
            .iter()
            .find(|part| &part.kind == kind)
            .map(|part| (part.items, part.tokens))
    }

    /// A tool is Rusta's own unless its name is an MCP server's.
    #[test]
    fn a_tool_comes_from_rusta_or_from_the_server_its_name_says() {
        assert_eq!(ToolSource::of("read_file"), ToolSource::BuiltIn);
        assert_eq!(
            ToolSource::of("mcp__devscriptor__search_code"),
            ToolSource::Mcp("devscriptor".to_string())
        );
        assert_eq!(ToolSource::of("mcp__broken"), ToolSource::BuiltIn);
    }

    /// The tool table is counted per source, the largest first, each tool at
    /// its whole definition.
    #[test]
    fn the_tool_table_is_counted_per_source() {
        let tools = [
            tool("read_file"),
            tool("mcp__devscriptor__a"),
            tool("bash"),
            tool("grep"),
        ];
        let parts = measure(&[Message::system("prompt")], &tools);
        assert_eq!(parts[0].kind, PartKind::SystemPrompt);
        assert_eq!(parts[1].kind, PartKind::Tools(ToolSource::BuiltIn));
        assert_eq!(parts[1].items, 3);
        assert_eq!(
            parts[1].tokens,
            [&tools[0], &tools[2], &tools[3]]
                .into_iter()
                .map(estimate_tool_tokens)
                .sum::<u64>()
        );
        assert_eq!(
            parts[2].kind,
            PartKind::Tools(ToolSource::Mcp("devscriptor".to_string()))
        );
        assert_eq!(parts[2].items, 1);
    }

    /// Each message lands in its part: the user's text apart from Rusta's
    /// notes in it, replies apart from thinking, calls, and each tool's
    /// results under the tool's name.
    #[test]
    fn the_conversation_is_counted_by_kind() {
        let reminder = "<system-reminder>\nCheck the todo list.\n</system-reminder>";
        let mut reply = Message::assistant_tool_calls(vec![
            ToolCall::new("c1", "read_file").with_arguments(r#"{"path": "a.rs"}"#),
            ToolCall::new("c2", "bash").with_arguments(r#"{"command": "ls"}"#),
        ]);
        reply.content = vec![
            ContentPart::Think {
                think: "I should look.".to_string(),
            },
            ContentPart::text("Looking."),
        ];
        let history = [
            Message::system("prompt"),
            Message::user(format!(
                "<git-context>\nbranch main\n</git-context>\n\nFix the bug.\n\n{reminder}"
            )),
            reply,
            Message::tool_result("c1", "x".repeat(4_000)),
            Message::tool_result("c2", "file list"),
            Message::system("<tools_added>\nmcp__srv__a — does a\n</tools_added>"),
        ];
        let parts = measure(&history, &[]);

        assert_eq!(
            part(&parts, &PartKind::UserMessages),
            Some((1, estimate_tokens("\n\nFix the bug.\n\n")))
        );
        assert_eq!(
            part(&parts, &PartKind::Notes).map(|(items, _)| items),
            Some(2)
        );
        assert_eq!(
            part(&parts, &PartKind::Replies),
            Some((1, estimate_tokens("Looking.")))
        );
        assert_eq!(
            part(&parts, &PartKind::Thinking),
            Some((1, estimate_tokens("I should look.")))
        );
        assert_eq!(
            part(&parts, &PartKind::ToolCalls).map(|(items, _)| items),
            Some(2)
        );
        assert_eq!(
            part(&parts, &PartKind::ToolResults("read_file".to_string())),
            Some((1, 1_000))
        );
        assert_eq!(
            part(&parts, &PartKind::ToolResults("bash".to_string())).map(|(items, _)| items),
            Some(1)
        );
        assert_eq!(
            part(&parts, &PartKind::ToolAnnouncements).map(|(items, _)| items),
            Some(1)
        );
        let read_file = parts
            .iter()
            .position(|part| part.kind == PartKind::ToolResults("read_file".to_string()));
        let bash = parts
            .iter()
            .position(|part| part.kind == PartKind::ToolResults("bash".to_string()));
        assert!(read_file < bash, "the larger results come first");
    }

    /// A compaction summary, tools delivered on a system message, and media
    /// each have a part of their own.
    #[test]
    fn summaries_loaded_tools_and_media_have_their_own_parts() {
        let summary = Message::user(format!(
            "{}\n\nWe fixed the parser.",
            crate::context_compaction::COMPACTION_SUMMARY_PREFIX
        ));
        let mut carrier = Message::system("");
        carrier.tools = vec![tool("mcp__srv__a"), tool("mcp__srv__b")];
        let mut with_image = Message::user("What is this?");
        with_image.content.push(ContentPart::ImageUrl {
            image_url: lm_bridge::message::ImageUrl {
                url: "data:image/png;base64,AAAA".to_string(),
                id: None,
            },
        });
        let parts = measure(
            &[Message::system("prompt"), summary, carrier, with_image],
            &[],
        );

        assert_eq!(
            part(&parts, &PartKind::CompactionSummary).map(|(items, _)| items),
            Some(1)
        );
        assert_eq!(
            part(&parts, &PartKind::LoadedTools),
            Some((2, 2 * estimate_tool_tokens(&tool("mcp__srv__a"))))
        );
        assert_eq!(
            part(&parts, &PartKind::Media),
            Some((1, MEDIA_TOKEN_ESTIMATE))
        );
        assert_eq!(
            part(&parts, &PartKind::UserMessages).map(|(items, _)| items),
            Some(1)
        );
    }

    /// Text outside Rusta's blocks is the user's, and a block left open runs
    /// to the end of the text.
    #[test]
    fn notes_are_told_from_the_users_text() {
        let (notes, own) =
            split_notes("Do it.\n<system-reminder>a</system-reminder>\nThanks.<git-context>b");
        assert_eq!(
            notes,
            ["<system-reminder>a</system-reminder>", "<git-context>b"]
        );
        assert_eq!(own, "Do it.\n\nThanks.");
    }
}
