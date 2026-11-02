use std::path::Path;
use std::time::Duration;

use lm_bridge::errors::LlmError;
use lm_bridge::message::{ContentPart, Message, Role};
use lm_bridge::provider::FinishReason;
use lm_bridge::tool::Tool;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::{
    agent_loop::llm::{CompactionNotice, Llm},
    blob::BlobStore,
    context_compaction::{self, RecordsWindow},
    context_projector,
    errors::AgentError,
    records::Records,
};

/// Minimum number of recent messages to preserve during any compaction.
const MIN_PRESERVED_MESSAGES: usize = 2;
/// Number of recent messages to keep during emergency compaction.
const EMERGENCY_PRESERVE_MESSAGES: usize = 4;
/// Token budget kept for the oldest user messages during proactive compaction.
const HEAD_USER_TOKEN_BUDGET: u64 = 2_000;
/// If the user-message pool fits within this budget it is kept whole; otherwise
/// only the oldest `HEAD_USER_TOKEN_BUDGET` tokens are preserved as a head.
const USER_POOL_TOKEN_BUDGET: u64 = 20_000;
/// Tool-result texts longer than this are moved to the blob sidecar.
const BLOB_THRESHOLD_CHARS: usize = 2_000;
/// Output cap for a summary request. Generous on purpose: reasoning models
/// spend part of it thinking before they write, and a summary cut off by the
/// cap is thrown away.
const COMPACTION_MAX_OUTPUT_TOKENS: u32 = 32_768;
/// Share of the window, after the output reserve, the summarizer's input may
/// fill: the estimate is a heuristic, so it keeps headroom.
const COMPACTION_INPUT_SAFETY_RATIO: f64 = 0.85;
/// After the summarizer's own request overflows, its input is cut to these
/// shares of its size in turn before giving up.
const COMPACTION_OVERFLOW_SHRINK_RATIOS: [f64; 3] = [0.7, 0.5, 0.35];
/// Default for [`ContextMemory::set_compaction_max_attempts`]: model requests
/// one compaction may make, retries and shrinks included.
pub const DEFAULT_COMPACTION_MAX_ATTEMPTS: u32 = 5;

/// Token-budget configuration for a conversation.
#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    max_tokens: u64,
    /// Compaction triggers when the context grows beyond this ratio of the budget.
    proactive_threshold: u64,
    /// Compaction target that leaves comfortable headroom before the hard limit.
    block_threshold: u64,
}

impl ContextBudget {
    /// Create a budget from a maximum token limit.
    ///
    /// The proactive compaction trigger is set to 90 % of the limit and the
    /// block (target) threshold to 50 %, matching a safe-split heuristic.
    #[must_use]
    pub fn new(max_tokens: u64) -> Self {
        Self::reserving_output(max_tokens, None)
    }

    /// [`Self::new`], keeping `reservation` tokens of the window free for the
    /// model's output: compaction triggers at the lower of 90 % and
    /// `max_tokens − reservation`, never below the block threshold it
    /// compacts to. DeepSeek reserves 256K of its 1M window, so it compacts
    /// at 786,432 tokens, as DeepSeek's own guides configure.
    #[must_use]
    pub fn reserving_output(max_tokens: u64, reservation: Option<u64>) -> Self {
        let block_threshold = max_tokens.saturating_mul(5).saturating_div(10);
        let ninety_percent = max_tokens.saturating_mul(9).saturating_div(10);
        let proactive_threshold = reservation.map_or(ninety_percent, |reserved| {
            ninety_percent
                .min(max_tokens.saturating_sub(reserved))
                .max(block_threshold)
        });
        Self {
            max_tokens,
            proactive_threshold,
            block_threshold,
        }
    }

    /// Maximum tokens allowed in the context window.
    #[must_use]
    pub fn max_tokens(&self) -> u64 {
        self.max_tokens
    }

    /// Threshold at which proactive compaction should run.
    #[must_use]
    pub fn proactive_threshold(&self) -> u64 {
        self.proactive_threshold
    }

    /// Target token count after proactive compaction.
    #[must_use]
    pub fn block_threshold(&self) -> u64 {
        self.block_threshold
    }
}

/// What kind of messages may be evicted during compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactMode {
    /// Evict the oldest non-system messages.
    AllButSystem,
    /// Evict only tool-result messages.
    ToolResultsOnly,
}

/// What a compaction needs besides the history itself.
pub struct CompactionInputs<'a> {
    /// The model that writes the summary. `None` goes straight to eviction.
    pub llm: Option<&'a dyn Llm>,
    /// The conversation's tool table, declared on the summary request so it
    /// shares the conversation's cached prefix.
    pub tools: &'a [Tool],
    /// The live todo list, rendered; re-attached below the summary.
    pub todo: Option<String>,
    /// `/compact <instruction>`'s focus text for the summarizer; auto
    /// compactions pass `None`.
    pub instruction: Option<String>,
    pub cancel: &'a CancellationToken,
}

/// Why a multi-turn undo did not run: an undo count that is not fully
/// satisfiable undoes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoError {
    /// Fewer than the requested turns can be undone; nothing was removed.
    Insufficient { requested: usize, undoable: usize },
}

impl std::fmt::Display for UndoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Insufficient {
                requested,
                undoable,
            } => write!(
                f,
                "Nothing to undo: only {undoable} of {requested} requested turn(s) available"
            ),
        }
    }
}

impl std::error::Error for UndoError {}

/// Why no summary could be made.
enum SummaryFailure {
    Cancelled,
    Failed(String),
}

/// Holds the conversation history and provides compaction.
pub struct ContextMemory {
    messages: Vec<Message>,
    system_prompt: String,
    budget: ContextBudget,
    records: Option<Records>,
    blob_store: Option<BlobStore>,
    blob_threshold_chars: usize,
    /// Messages added since the last [`persist`](Self::persist), in order.
    ///
    /// Kept apart from `messages` because compaction rewrites the live
    /// history: an index into it cannot say what the records file still has to
    /// receive once messages have been removed or replaced.
    unwritten: Vec<Message>,
    /// The last records-file line an earlier compaction's pointer covered.
    compacted_through_line: usize,
    /// Model requests one compaction may make.
    compaction_max_attempts: u32,
    /// The estimate right after the last compaction. Proactive compaction does
    /// not run again until the context has grown past it.
    last_compacted_tokens: Option<u64>,
}

impl ContextMemory {
    /// Create a new context with the given system prompt and token budget.
    #[must_use]
    pub fn new(system_prompt: impl Into<String>, max_tokens: u64) -> Self {
        Self {
            messages: Vec::new(),
            system_prompt: system_prompt.into(),
            budget: ContextBudget::new(max_tokens),
            records: None,
            blob_store: None,
            blob_threshold_chars: BLOB_THRESHOLD_CHARS,
            unwritten: Vec::new(),
            compacted_through_line: 0,
            compaction_max_attempts: DEFAULT_COMPACTION_MAX_ATTEMPTS,
            last_compacted_tokens: None,
        }
    }

    /// Attach a records store for JSONL persistence.
    ///
    /// Also opens a sibling blob directory for offloading large tool outputs.
    #[must_use]
    pub fn with_records(mut self, records: Records) -> Self {
        if let Some(parent) = records.path().parent() {
            self.blob_store = BlobStore::new(parent.join("blobs")).ok();
        }
        self.records = Some(records);
        self
    }

    /// Attach an explicit blob store (overrides the sibling of the records path).
    #[must_use]
    pub fn with_blob_store(mut self, blob_store: BlobStore) -> Self {
        self.blob_store = Some(blob_store);
        self
    }

    /// Model requests one compaction may make, retries and shrinks included.
    /// `0` restores the default ([`DEFAULT_COMPACTION_MAX_ATTEMPTS`]).
    pub fn set_compaction_max_attempts(&mut self, attempts: u32) {
        self.compaction_max_attempts = if attempts == 0 {
            DEFAULT_COMPACTION_MAX_ATTEMPTS
        } else {
            attempts
        };
    }

    /// The session's records file, when one is attached.
    #[must_use]
    pub fn records_path(&self) -> Option<&Path> {
        self.records.as_ref().map(Records::path)
    }

    /// The records store, when the conversation is recorded.
    #[must_use]
    pub fn records(&self) -> Option<&Records> {
        self.records.as_ref()
    }

    /// Where large tool results are moved out of the records file.
    #[must_use]
    pub fn blob_dir(&self) -> Option<&Path> {
        self.blob_store.as_ref().map(BlobStore::dir)
    }

    /// Append `message` to the history and queue it for the records file.
    fn push(&mut self, message: Message) {
        if self.records.is_some() {
            self.unwritten.push(message.clone());
        }
        self.messages.push(message);
    }

    /// The messages as stored, unprojected. Test-only: production reads go
    /// through [`ContextMemory::build_history`], which merges adjacent user
    /// messages (reminders fold into their neighbour) and drops vacuous ones.
    #[cfg(test)]
    pub(crate) fn raw_messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn add_user_message(&mut self, text: impl Into<String>) {
        self.push(Message::user(text));
    }

    /// The assistant steps after the newest message `marks` matches: all of
    /// them when none does.
    pub fn steps_since(&self, marks: impl Fn(&Message) -> bool) -> usize {
        self.messages
            .iter()
            .rev()
            .take_while(|message| !marks(message))
            .filter(|message| message.role == Role::Assistant)
            .count()
    }

    /// The conversation's first exchange: the first prompt the user wrote,
    /// and the model's last text before the next one, its answer. `None`
    /// until that turn has an answer.
    pub fn first_exchange(&self) -> Option<(String, String)> {
        let start = self
            .messages
            .iter()
            .position(context_compaction::is_real_user_input)?;
        let turn = &self.messages[start + 1..];
        let end = turn
            .iter()
            .position(context_compaction::is_real_user_input)
            .unwrap_or(turn.len());
        let answer = turn[..end]
            .iter()
            .rev()
            .filter(|message| message.role == Role::Assistant)
            .map(Message::text_content)
            .find(|text| !text.trim().is_empty())?;
        Some((self.messages[start].text_content(), answer))
    }

    /// Whether any real user input is in the history — not an injected
    /// reminder or notification. A fresh sub-agent's task message is the
    /// first; a resumed agent replays its own.
    pub fn has_real_user_input(&self) -> bool {
        self.messages
            .iter()
            .any(context_compaction::is_real_user_input)
    }

    /// Append the interruption reminder after a turn the user
    /// cancelled: the partial output above is incomplete and the next message
    /// continues the conversation. Added once — skipped when the newest
    /// message already is one — and taken away with its turn by
    /// [`ContextMemory::undo_last_turn`], since a reminder is an injection,
    /// not the start of one.
    pub fn add_interruption_reminder(&mut self) {
        let newest = self.messages.iter().rev().find(|message| {
            // What a cancelled turn leaves behind: a partial assistant
            // message with no tool calls and no text worth keeping.
            !(message.role == Role::Assistant
                && message.partial
                && message.tool_calls.is_empty()
                && message.text_content().trim().is_empty())
        });
        if let Some(message) = newest
            && crate::prompt::is_interruption_reminder(&message.text_content())
        {
            return;
        }
        self.push(Message::user(crate::prompt::interruption_reminder()));
    }

    /// Tell the model today's date with a reminder, unless the newest date
    /// reminder in the context already names it. The date stays out of the
    /// system prompt so the prompt prefix stays cacheable; compaction drops
    /// reminders, so a compacted context is told again at its next prompt.
    /// Returns whether a reminder was added.
    pub fn announce_date(&mut self, today: chrono::NaiveDate) -> bool {
        let announced = self.messages.iter().rev().find_map(|m| {
            if m.role != Role::User {
                return None;
            }
            match m.content.first() {
                Some(ContentPart::Text { text }) => crate::prompt::parse_date_reminder(text),
                _ => None,
            }
        });
        if announced == Some(today) {
            return false;
        }
        self.push(Message::user(crate::prompt::date_reminder(today)));
        true
    }

    pub fn add_assistant_text(&mut self, text: impl Into<String>) {
        self.push(Message::assistant_text(text));
    }

    pub fn add_message(&mut self, message: Message) {
        self.push(message);
    }

    /// Add a message, offloading large tool results to the blob sidecar.
    pub async fn add_message_with_blob(&mut self, mut message: Message) -> Result<(), AgentError> {
        if let Some(store) = &self.blob_store
            && message.role == Role::Tool
        {
            let text = message.text_content();
            if text.len() > self.blob_threshold_chars {
                let id = store.write(text.as_bytes()).await?;
                message.content = vec![ContentPart::Text {
                    text: crate::blob::marker(&id),
                }];
            }
        }
        self.push(message);
        Ok(())
    }

    /// Replay messages read back from the records file (or inherited by a
    /// fork). They are already on disk, so nothing is queued for writing.
    ///
    /// A recorded compaction summary folds the history before it into the
    /// compacted shape, exactly as it did when the summary was made; the
    /// elision note and continuation anchor are regenerated by that fold, so
    /// recorded copies of them are skipped.
    pub fn replay_messages(&mut self, messages: Vec<Message>) {
        for message in messages {
            if context_compaction::is_compaction_marker(&message) {
                continue;
            }
            if context_compaction::is_compaction_summary_message(&message) {
                self.messages = context_compaction::apply_compaction(
                    &self.messages,
                    &message.text_content(),
                    context_compaction::COMPACT_USER_MESSAGE_MAX_TOKENS,
                    context_compaction::COMPACT_USER_MESSAGE_HEAD_TOKENS,
                );
            } else {
                self.messages.push(message);
            }
        }
    }

    /// Append any messages not yet written to the record file.
    ///
    /// A write failure is reported and dropped rather than propagated. Records
    /// are a side effect of the conversation, not the conversation itself: a
    /// full disk, a read-only mount, or a revoked permission should cost the
    /// transcript, not the user's in-flight work. The turn continues, and the
    /// queue is still cleared so a transient failure does not make the next
    /// call rewrite everything that already landed.
    pub async fn persist(&mut self) -> Result<(), crate::errors::AgentError> {
        let unwritten = std::mem::take(&mut self.unwritten);
        if let Some(records) = &self.records {
            for msg in &unwritten {
                if let Err(e) = records.append(msg).await {
                    tracing::warn!(
                        "could not write to the record file {}: {e}. \
                         The session continues; this turn will not be in the transcript.",
                        records.path().display()
                    );
                    break;
                }
            }
        }
        Ok(())
    }

    /// Return a provider-safe copy of the message history, with the system
    /// prompt first.
    ///
    /// Blob references are expanded back to their original content, adjacent
    /// user messages are merged, and the tool-exchange adjacency is repaired
    /// (orphan tool results dropped, missing results synthesized).
    #[must_use]
    pub fn build_history(&self) -> Vec<Message> {
        self.project_with_options(&context_projector::ProjectOptions::standard())
    }

    /// A snapshot of this conversation for a forked sub-agent to inherit.
    ///
    /// Deliberately *unprojected*: blob references are expanded, because the
    /// blob store belongs to this agent and the child cannot resolve them, but
    /// the repair passes are left to the child. Projecting here would answer the
    /// still-open `agent` call — the one creating the fork — with the generic
    /// missing-result placeholder, before
    /// [`Agent::with_forked_context`](crate::Agent::with_forked_context) gets
    /// the chance to say what actually happened to it.
    ///
    /// The system prompt is not included: the child has its own.
    #[must_use]
    pub fn fork_snapshot(&self) -> Vec<Message> {
        self.messages
            .iter()
            .map(|m| self.expand_message_blobs(m))
            .collect()
    }

    /// This context's system prompt.
    #[must_use]
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Build a strict, repaired history for the one-shot resend fallback after
    /// provider tool-exchange / message-structure errors.
    #[must_use]
    pub fn build_repaired_history(&self) -> Vec<Message> {
        self.project_with_options(&context_projector::ProjectOptions::strict())
    }

    fn project_with_options(&self, options: &context_projector::ProjectOptions) -> Vec<Message> {
        let expanded: Vec<Message> = self
            .messages
            .iter()
            .map(|m| self.expand_message_blobs(m))
            .collect();
        let mut projected = context_projector::project(&expanded, options);
        let mut history = Vec::with_capacity(projected.len() + 1);
        history.push(Message::system(&self.system_prompt));
        history.append(&mut projected);
        history
    }

    /// `text` with each blob reference replaced by the content it points to;
    /// unchanged without a blob store.
    #[must_use]
    pub fn expand_blob_text(&self, text: &str) -> String {
        match &self.blob_store {
            Some(store) => expand_blob_refs(text, store),
            None => text.to_string(),
        }
    }

    fn expand_message_blobs(&self, message: &Message) -> Message {
        let Some(store) = &self.blob_store else {
            return message.clone();
        };
        let mut expanded = message.clone();
        for part in &mut expanded.content {
            if let ContentPart::Text { text } = part {
                *text = expand_blob_refs(text, store);
            }
        }
        expanded
    }

    /// Return the most recent non-empty assistant text, if any.
    #[must_use]
    pub fn last_assistant_text(&self) -> Option<String> {
        self.messages.iter().rev().find_map(|m| {
            if m.role == Role::Assistant && !m.text_content().is_empty() {
                Some(m.text_content())
            } else {
                None
            }
        })
    }

    /// Maximum tokens configured for this context.
    #[must_use]
    pub fn max_tokens(&self) -> u64 {
        self.budget.max_tokens()
    }

    /// The token budget, with its compaction thresholds.
    #[must_use]
    pub fn budget(&self) -> ContextBudget {
        self.budget
    }

    /// Reconfigure the context token budget.
    pub fn set_budget(&mut self, budget: ContextBudget) {
        self.budget = budget;
    }

    /// Replace the system prompt.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = prompt.into();
    }

    /// Whether the estimated token count exceeds the hard budget.
    #[must_use]
    pub fn is_over_budget(&self) -> bool {
        self.estimate_tokens() > self.budget.max_tokens()
    }

    /// Whether the estimated token count exceeds the proactive compaction threshold.
    #[must_use]
    pub fn should_compact_proactively(&self) -> bool {
        self.estimate_tokens() > self.budget.proactive_threshold
    }

    /// Whether a proactive compaction should run now: the context is past the
    /// proactive threshold, and it has grown since the last compaction.
    ///
    /// A compaction that could not bring the context under the threshold (the
    /// messages it must keep are that large) is not retried on every step:
    /// the next attempt waits until the context has grown by another 5 % of
    /// the window.
    #[must_use]
    pub fn needs_proactive_compaction(&self) -> bool {
        let tokens = self.estimate_tokens();
        if tokens <= self.budget.proactive_threshold {
            return false;
        }
        match self.last_compacted_tokens {
            Some(after) if after >= self.budget.proactive_threshold => {
                tokens >= after.saturating_add(self.budget.max_tokens / 20)
            }
            _ => true,
        }
    }

    /// Compact to the given token target using the chosen mode.
    ///
    /// Returns the messages that were removed so the caller can summarize them.
    /// Assistant tool-call messages are evicted together with their following
    /// tool results to keep the tool exchange intact.
    pub fn compact_to(&mut self, target_tokens: u64, mode: CompactMode) -> Vec<Message> {
        let mut evicted = Vec::new();
        let head_user_budget_chars = HEAD_USER_TOKEN_BUDGET.saturating_mul(4) as usize;
        let user_pool_budget_chars = USER_POOL_TOKEN_BUDGET.saturating_mul(4) as usize;
        let total_user_chars: usize = self
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.text_content().len())
            .sum();
        let split_user_messages = total_user_chars > user_pool_budget_chars;
        let mut preserved_user_head_chars = 0usize;

        while self.messages.len() > MIN_PRESERVED_MESSAGES && self.estimate_tokens() > target_tokens
        {
            let candidate = match mode {
                CompactMode::AllButSystem => self.messages.iter().position(|m| {
                    if split_user_messages
                        && m.role == Role::User
                        && preserved_user_head_chars < head_user_budget_chars
                    {
                        preserved_user_head_chars += m.text_content().len();
                        false
                    } else {
                        m.role != Role::System
                    }
                }),
                CompactMode::ToolResultsOnly => {
                    self.messages.iter().position(|m| m.role == Role::Tool)
                }
            };

            let idx = match candidate {
                Some(idx) => idx,
                None => break,
            };

            evicted.extend(self.evict_block_at(idx));
        }

        evicted
    }

    /// Remove the message at `idx`. If it is an assistant tool-call message,
    /// also remove the contiguous tool-result messages that follow it so the
    /// tool exchange is not split.
    fn evict_block_at(&mut self, idx: usize) -> Vec<Message> {
        let end = self.block_end(idx);
        self.messages.drain(idx..end).collect()
    }

    /// One past the last message of the block starting at `idx`: an assistant
    /// tool call together with the results that follow it, or one message.
    fn block_end(&self, idx: usize) -> usize {
        if self.messages[idx].role == Role::Assistant && !self.messages[idx].tool_calls.is_empty() {
            let mut end = idx + 1;
            while end < self.messages.len() && self.messages[end].role == Role::Tool {
                end += 1;
            }
            end
        } else {
            idx + 1
        }
    }

    /// Emergency compaction used after the provider reports a context-overflow error.
    ///
    /// Keeps only the most recent messages to maximize the chance the next request fits.
    pub fn compact_emergency(&mut self) {
        let keep = self.messages.len().min(EMERGENCY_PRESERVE_MESSAGES);
        let drain = self.messages.len().saturating_sub(keep);
        self.messages.drain(0..drain);
    }

    /// Compact the context: replace it with a model-written handoff summary,
    /// or — when no summary can be had — evict the oldest messages down to
    /// `evict_target` tokens.
    ///
    /// The summary path keeps the user's own messages (oldest and newest, within
    /// a budget), then the summary — with the live todo list and a pointer into
    /// the records file appended — then an anchor telling the model to resume
    /// the latest request. The summarizer sees the real conversation, trimmed
    /// from the oldest end to fit the window, and gets up to
    /// [`set_compaction_max_attempts`](Self::set_compaction_max_attempts)
    /// requests in all.
    ///
    /// The eviction path never removes the first or the latest user message,
    /// and leaves a note saying how many messages went and where they can
    /// still be read.
    ///
    /// Returns what happened; only a cancellation is an error.
    pub async fn compact(
        &mut self,
        inputs: CompactionInputs<'_>,
        evict_target: u64,
    ) -> Result<CompactionNotice, AgentError> {
        let tokens_before = self.estimate_tokens();
        if self.messages.is_empty() {
            return Ok(CompactionNotice::Unchanged {
                reason: "there is nothing to compact".to_string(),
            });
        }
        // Everything the summary replaces must be on disk before the pointer
        // to it is written.
        self.persist().await?;
        let window = self.records_window().await;

        let summary = match inputs.llm {
            Some(llm) => self.request_summary(llm, &inputs).await,
            None => Err(SummaryFailure::Failed("no model is attached".to_string())),
        };
        match summary {
            Ok(summary) => {
                let mut text = context_compaction::build_compaction_summary_text(&summary);
                if let Some(todo) = inputs.todo.as_deref().filter(|t| !t.trim().is_empty()) {
                    text.push_str("\n\n## TODO list\n");
                    text.push_str(todo.trim_end());
                }
                if let Some(window) = &window {
                    text.push_str("\n\n");
                    text.push_str(&context_compaction::build_records_pointer(window));
                    self.compacted_through_line = window.last_line;
                }
                self.messages = context_compaction::apply_compaction(
                    &self.messages,
                    &text,
                    context_compaction::COMPACT_USER_MESSAGE_MAX_TOKENS,
                    context_compaction::COMPACT_USER_MESSAGE_HEAD_TOKENS,
                );
                if self.records.is_some() {
                    // Only the summary is recorded: replaying it regenerates
                    // the rest of the compacted shape.
                    self.unwritten.push(Message::user(text));
                }
                // Kept user messages can be large enough, in a small window,
                // to leave the context past the threshold; trimming them now
                // spares the next step another summary.
                if self.estimate_tokens() > self.budget.proactive_threshold {
                    self.evict_protected(evict_target);
                }
                let tokens_after = self.estimate_tokens();
                self.last_compacted_tokens = Some(tokens_after);
                Ok(CompactionNotice::Summarized {
                    tokens_before,
                    tokens_after,
                })
            }
            Err(SummaryFailure::Cancelled) => Err(AgentError::Interrupted),
            Err(SummaryFailure::Failed(reason)) => {
                tracing::warn!("compaction could not summarize ({reason}); evicting instead");
                Ok(self.evict_with_notice(evict_target, reason))
            }
        }
    }

    /// Evicts down to `target_tokens` ([`evict_protected`](Self::evict_protected))
    /// and leaves a note after the first user message saying how many
    /// messages went and where they can still be read. `reason` says why a
    /// summary was not made instead.
    pub fn evict_with_notice(&mut self, target_tokens: u64, reason: String) -> CompactionNotice {
        let tokens_before = self.estimate_tokens();
        let removed = self.evict_protected(target_tokens);
        if removed == 0 {
            self.last_compacted_tokens = Some(tokens_before);
            return CompactionNotice::Unchanged { reason };
        }
        let notice = context_compaction::build_removal_notice(removed, self.records_path());
        let at = self
            .messages
            .iter()
            .position(context_compaction::is_real_user_input)
            .map_or(0, |first| first + 1);
        self.messages.insert(at, Message::user(notice));
        let tokens_after = self.estimate_tokens();
        self.last_compacted_tokens = Some(tokens_after);
        CompactionNotice::Evicted {
            removed,
            tokens_before,
            tokens_after,
            reason,
        }
    }

    /// Where compaction aims: half the window, well clear of the proactive
    /// threshold.
    #[must_use]
    pub fn compaction_target(&self) -> u64 {
        self.budget.block_threshold()
    }

    /// Asks `llm` for a handoff summary of the history.
    async fn request_summary(
        &self,
        llm: &dyn Llm,
        inputs: &CompactionInputs<'_>,
    ) -> Result<String, SummaryFailure> {
        let mut history = self.build_history();
        let system = history.remove(0);
        if history.is_empty() {
            return Err(SummaryFailure::Failed(
                "there is nothing to summarize".to_string(),
            ));
        }
        let has_todos = inputs.todo.as_deref().is_some_and(|t| !t.trim().is_empty());
        let instruction = Message::user(context_compaction::render_compaction_instruction(
            self.records.is_some(),
            has_todos,
            inputs.instruction.as_deref(),
        ));

        // Fit the summarizer's input to the window up front:
        // an overrun costs a request and says nothing about how much to cut.
        let window = self.budget.max_tokens();
        let reserve = u64::from(COMPACTION_MAX_OUTPUT_TOKENS).min(window / 8);
        let overhead = context_compaction::estimate_tokens_for_message(&system)
            + context_compaction::estimate_tokens_for_message(&instruction)
            + estimate_tool_tokens(inputs.tools);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let budget =
            ((window.saturating_sub(reserve)) as f64 * COMPACTION_INPUT_SAFETY_RATIO) as u64;
        let budget = budget.saturating_sub(overhead);
        let mut messages = if budget > 0 && estimate_messages(&history) > budget {
            let kept = keep_recent_within(&history, budget);
            if kept.is_empty() { history } else { kept }
        } else {
            history
        };

        let max_attempts = self.compaction_max_attempts;
        let mut attempts = 0u32;
        let mut overflow_shrinks = 0usize;
        let mut transient_retries = 0u32;
        loop {
            attempts += 1;
            if inputs.cancel.is_cancelled() {
                return Err(SummaryFailure::Cancelled);
            }
            let mut request = Vec::with_capacity(messages.len() + 2);
            request.push(system.clone());
            request.extend(messages.iter().cloned());
            request.push(instruction.clone());
            let exhausted = attempts >= max_attempts;
            let result = llm
                .summarize(
                    request,
                    inputs.tools.to_vec(),
                    Some(COMPACTION_MAX_OUTPUT_TOKENS),
                    inputs.cancel,
                )
                .await;
            match result {
                Ok(step) => {
                    let text = step.message.text_content().trim().to_string();
                    let cut_off = step.finish_reason == Some(FinishReason::Length);
                    if !text.is_empty() && !cut_off {
                        return Ok(text);
                    }
                    let why = if cut_off {
                        "the summary was cut off by the output limit"
                    } else {
                        "the summary came back empty"
                    };
                    if exhausted || messages.len() <= 1 {
                        return Err(SummaryFailure::Failed(format!(
                            "{why} ({attempts} attempts)"
                        )));
                    }
                    // Less to summarize makes a shorter summary.
                    messages = drop_oldest_message_and_leading_tool_results(messages);
                }
                Err(LlmError::Cancelled) => return Err(SummaryFailure::Cancelled),
                Err(
                    e @ LlmError::ApiEmptyResponse {
                        filtered: false, ..
                    },
                ) => {
                    if exhausted || messages.len() <= 1 {
                        return Err(SummaryFailure::Failed(e.to_string()));
                    }
                    messages = drop_oldest_message_and_leading_tool_results(messages);
                }
                Err(e) if e.is_context_overflow() => {
                    overflow_shrinks += 1;
                    if exhausted
                        || overflow_shrinks > COMPACTION_OVERFLOW_SHRINK_RATIOS.len()
                        || messages.len() <= 1
                    {
                        return Err(SummaryFailure::Failed(e.to_string()));
                    }
                    let ratio = COMPACTION_OVERFLOW_SHRINK_RATIOS[overflow_shrinks - 1];
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let target = (estimate_messages(&messages) as f64 * ratio) as u64;
                    messages = keep_recent_within(&messages, target);
                    if messages.is_empty() {
                        return Err(SummaryFailure::Failed(e.to_string()));
                    }
                }
                Err(e) if !exhausted && !e.is_fatal() && llm.is_retryable_error(&e) => {
                    transient_retries += 1;
                    let delay = Duration::from_secs_f64(crate::agent_loop::retry::chosen_delay(
                        transient_retries,
                        e.retry_after(),
                    ));
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        () = inputs.cancel.cancelled() => return Err(SummaryFailure::Cancelled),
                    }
                }
                Err(e) => return Err(SummaryFailure::Failed(e.to_string())),
            }
        }
    }

    /// The records-file lines a compaction starting now replaces, when this
    /// session keeps records.
    async fn records_window(&self) -> Option<RecordsWindow> {
        let path = self.records_path()?;
        let last_line = count_lines(path).await?;
        let first_line = self.compacted_through_line + 1;
        (last_line >= first_line).then(|| RecordsWindow {
            path: path.to_path_buf(),
            first_line,
            last_line,
            blob_dir: self.blob_dir().map(Path::to_path_buf),
        })
    }

    /// Removes the oldest messages until the estimate is at most
    /// `target_tokens`, and returns how many went.
    ///
    /// Never removes the first or the latest message the user wrote, a
    /// compaction summary, or the last message; an assistant tool call goes
    /// together with its results, and a block holding a protected message
    /// stays.
    pub fn evict_protected(&mut self, target_tokens: u64) -> usize {
        let mut chars = self.system_prompt.len() + self.messages_chars();
        let mut removed = 0;
        while chars / 4 > target_tokens as usize && self.messages.len() > MIN_PRESERVED_MESSAGES {
            let first_user = self
                .messages
                .iter()
                .position(context_compaction::is_real_user_input);
            let last_user = self
                .messages
                .iter()
                .rposition(context_compaction::is_real_user_input);
            let last = self.messages.len() - 1;
            let protected = |i: usize, m: &Message| {
                Some(i) == first_user
                    || Some(i) == last_user
                    || i == last
                    || context_compaction::is_compaction_summary_message(m)
            };
            let mut idx = 0;
            let block = loop {
                if idx >= self.messages.len() {
                    break None;
                }
                let end = self.block_end(idx);
                if (idx..end).any(|i| protected(i, &self.messages[i])) {
                    idx = end;
                    continue;
                }
                break Some((idx, end));
            };
            let Some((start, end)) = block else {
                break;
            };
            let evicted: Vec<Message> = self.messages.drain(start..end).collect();
            for message in &evicted {
                chars = chars.saturating_sub(self.message_chars(message));
            }
            removed += evicted.len();
        }
        removed
    }

    /// Replace the oldest inline media in the history with notes — half of it
    /// by size, at least one item — after the provider rejected a request as
    /// too large. A note names the file when the media's caption did. Returns
    /// how many items were replaced; `0` means there was no media to drop.
    pub fn omit_oldest_media(&mut self) -> usize {
        let mut inline = Vec::new();
        for (m, message) in self.messages.iter().enumerate() {
            for (p, part) in message.content.iter().enumerate() {
                if let Some(url) = lm_bridge::media::media_url(part)
                    .filter(|url| lm_bridge::media::is_data_url(url))
                {
                    inline.push((m, p, url.len()));
                }
            }
        }
        let total: usize = inline.iter().map(|(_, _, len)| len).sum();
        let mut remaining = total;
        let mut omitted = 0;
        for (m, p, len) in inline {
            if omitted > 0 && remaining <= total / 2 {
                break;
            }
            let content = &self.messages[m].content;
            let kind = lm_bridge::media::media_kind(&content[p]);
            let path = content[..p].iter().rev().find_map(|part| match part {
                ContentPart::Text { text } => {
                    crate::tools::builtin::read_media::caption_path(text).map(str::to_string)
                }
                _ => None,
            });
            let note = match path {
                Some(path) => format!("[{kind} omitted to fit the request size limit — {path}]"),
                None => format!("[{kind} omitted to fit the request size limit]"),
            };
            self.messages[m].content[p] = ContentPart::text(note);
            remaining -= len;
            omitted += 1;
        }
        omitted
    }

    /// Synthesize error tool results for tool calls that were abandoned before
    /// a result could be recorded (e.g. the turn was interrupted or tool
    /// execution failed en masse). This prevents orphan assistant tool_calls
    /// from bricking subsequent provider requests.
    pub fn close_abandoned_tool_calls(&mut self, tool_calls: &[lm_bridge::message::ToolCall]) {
        for tc in tool_calls {
            self.push(Message::tool_result(
                &tc.id,
                format!(
                    "Error: tool exchange abandoned before a result was received for {}({}).",
                    tc.name,
                    tc.arguments.as_deref().unwrap_or("{}")
                ),
            ));
        }
    }

    /// Remove all non-system messages, along with anything not yet written.
    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.unwritten.clear();
        self.last_compacted_tokens = None;
    }

    /// Undo the last user/assistant turn.
    ///
    /// Removes the most recent message the user wrote and every message added
    /// after it — injected reminders and notifications do not count as the
    /// start of a turn. Returns the number of messages removed.
    pub fn undo_last_turn(&mut self) -> usize {
        self.undo_turns(1).unwrap_or(0)
    }

    /// How many turns [`undo_turns`](Self::undo_turns) can remove right now:
    /// the count of messages in the conversation that start a turn.
    #[must_use]
    pub fn undoable_turns(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| context_compaction::is_real_user_input(m))
            .count()
    }

    /// Undo the last `count` user/assistant turns.
    ///
    /// Like [`undo_last_turn`](Self::undo_last_turn), the cut lands on the
    /// `count`-th most recent message the user wrote, and every message added
    /// after it goes — injected reminders and notifications do not count as
    /// the start of a turn. A count that exceeds what can be undone is an
    /// error and removes nothing.
    ///
    /// Returns the number of messages removed.
    pub fn undo_turns(&mut self, count: usize) -> Result<usize, UndoError> {
        if count == 0 {
            return Ok(0);
        }
        let original_len = self.messages.len();
        let mut seen = 0usize;
        let mut cut = None;
        for (i, message) in self.messages.iter().enumerate().rev() {
            if context_compaction::is_real_user_input(message) {
                seen += 1;
                if seen == count {
                    cut = Some(i);
                    break;
                }
            }
        }
        let Some(idx) = cut else {
            return Err(UndoError::Insufficient {
                requested: count,
                undoable: seen,
            });
        };
        self.messages.truncate(idx);
        let removed = original_len - idx;
        // Messages written before the undo stay in the append-only records
        // file; ones still queued are dropped with the turn.
        let queued = removed.min(self.unwritten.len());
        self.unwritten.truncate(self.unwritten.len() - queued);
        self.last_compacted_tokens = None;
        Ok(removed)
    }

    /// Estimate token count using a simple 4-character heuristic.
    ///
    /// This matches the heuristic used by `glm-acp-agent` and is intentionally
    /// cheap so it can run on every turn. The system prompt is included.
    #[must_use]
    pub fn estimate_tokens(&self) -> u64 {
        let total_chars: usize = self
            .system_prompt
            .len()
            .saturating_add(self.messages_chars());
        (total_chars / 4) as u64
    }

    fn messages_chars(&self) -> usize {
        self.messages.iter().map(|m| self.message_chars(m)).sum()
    }

    fn message_chars(&self, message: &Message) -> usize {
        let content: usize = message
            .content
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => {
                    if let Some(store) = &self.blob_store {
                        expand_blob_refs(text, store).len()
                    } else {
                        text.len()
                    }
                }
                ContentPart::Think { think } => think.len(),
                ContentPart::ReasoningDetails { details } => {
                    serde_json::Value::from(details.clone()).to_string().len()
                }
                _ => 0,
            })
            .sum();
        let tools: usize = message
            .tool_calls
            .iter()
            .map(|tc| {
                tc.name.len()
                    + tc.arguments.as_ref().map_or(0, std::string::String::len)
                    + tc.id.len()
            })
            .sum();
        content + tools
    }
}

/// Expand all `<blob id="..."/>` markers in `text` using `store`.
fn expand_blob_refs(text: &str, store: &BlobStore) -> String {
    crate::blob::MARKER_RE
        .replace_all(text, |caps: &regex::Captures| {
            let id = &caps[1];
            match store.read(id) {
                Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                None => format!("[blob {id} missing]"),
            }
        })
        .into_owned()
}

fn estimate_messages(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(context_compaction::estimate_tokens_for_message)
        .sum()
}

/// Tokens a tool table costs on the wire: names, descriptions and schemas.
fn estimate_tool_tokens(tools: &[Tool]) -> u64 {
    tools
        .iter()
        .map(|tool| {
            context_compaction::estimate_tokens(&tool.name)
                + context_compaction::estimate_tokens(&tool.description)
                + context_compaction::estimate_tokens(&tool.parameters.to_string())
        })
        .sum()
}

/// The most recent messages whose estimate fits `budget`, without a leading
/// tool result whose call was cut away. Empty when not even the last fits.
fn keep_recent_within(messages: &[Message], budget: u64) -> Vec<Message> {
    let mut start = messages.len();
    let mut used = 0u64;
    for (i, message) in messages.iter().enumerate().rev() {
        let tokens = context_compaction::estimate_tokens_for_message(message);
        if used + tokens > budget {
            break;
        }
        used += tokens;
        start = i;
    }
    let mut kept = messages[start..].to_vec();
    let leading_results = kept.iter().take_while(|m| m.role == Role::Tool).count();
    kept.drain(..leading_results);
    kept
}

/// Lines in `path` (a final line without a newline counts), or `None` when it
/// cannot be read.
async fn count_lines(path: &Path) -> Option<usize> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut lines = 0usize;
    let mut last = b'\n';
    loop {
        let n = file.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        lines += buf[..n].iter().filter(|&&b| b == b'\n').count();
        last = buf[n - 1];
    }
    if last != b'\n' {
        lines += 1;
    }
    Some(lines)
}

/// Drop the oldest message from `messages`, then drop any leading tool-result
/// messages that would become orphans. This keeps the remaining history valid
/// for the provider even when the assistant tool-call that produced them was
/// removed.
fn drop_oldest_message_and_leading_tool_results(mut messages: Vec<Message>) -> Vec<Message> {
    if messages.len() <= 1 {
        return messages;
    }
    messages.remove(0);
    while !messages.is_empty() && messages[0].role == Role::Tool {
        messages.remove(0);
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use lm_bridge::message::ToolCall;

    /// The first exchange is the user's first prompt, past the reminders
    /// before it, and the last text the model gave before the next prompt.
    #[test]
    fn the_first_exchange_is_the_first_prompt_and_its_answer() {
        let mut context = ContextMemory::new("sys", 100_000);
        context
            .add_user_message("<system-reminder>\nToday's date is 2026-10-19.\n</system-reminder>");
        context.add_user_message("Fix the login test.");
        assert_eq!(context.first_exchange(), None, "no answer yet");
        context.add_message(Message::assistant_text("Looking at it."));
        context.add_message(Message::assistant_tool_calls(vec![ToolCall::new(
            "tc", "bash",
        )]));
        context.add_message(Message::tool_result("tc", "ok"));
        context.add_message(Message::assistant_text("Fixed: the cookie was renamed."));
        context.add_user_message("Now the docs.");
        context.add_message(Message::assistant_text("Docs updated."));
        assert_eq!(
            context.first_exchange(),
            Some((
                "Fix the login test.".to_string(),
                "Fixed: the cookie was renamed.".to_string()
            ))
        );
    }

    /// The steps after the newest message a marker matches, or all of them.
    #[test]
    fn steps_are_counted_since_the_newest_match() {
        let mut ctx = ContextMemory::new("system", 10_000);
        ctx.add_user_message("start");
        ctx.add_assistant_text("one");
        ctx.add_user_message("mark");
        ctx.add_assistant_text("two");
        ctx.add_assistant_text("three");
        let marked = |m: &Message| m.text_content() == "mark";
        assert_eq!(ctx.steps_since(marked), 2);
        assert_eq!(ctx.steps_since(|_| false), 3);
        assert_eq!(ctx.steps_since(|m| m.text_content() == "three"), 0);
    }

    /// The date is announced once per day; a compaction or a clear that drops
    /// the reminder leads to a new one, and a replayed history keeps its own.
    #[test]
    fn the_date_is_announced_once_per_day() {
        let day = |d| chrono::NaiveDate::from_ymd_opt(2026, 9, d).unwrap();
        let mut ctx = ContextMemory::new("system", 100_000);

        assert!(ctx.announce_date(day(24)));
        assert!(!ctx.announce_date(day(24)), "already announced");
        ctx.add_user_message("hello");
        assert!(
            !ctx.announce_date(day(24)),
            "a later message does not hide it"
        );
        assert!(ctx.announce_date(day(25)), "a new day is announced");
        let reminders = ctx
            .fork_snapshot()
            .iter()
            .filter(|m| crate::prompt::parse_date_reminder(&m.text_content()).is_some())
            .count();
        assert_eq!(reminders, 2);

        // The projection merges the reminder into the next user message, at
        // its start.
        let first = ctx
            .build_history()
            .into_iter()
            .find(|m| m.role == Role::User)
            .expect("a user message")
            .text_content();
        assert!(
            first.starts_with(&crate::prompt::date_reminder(day(24))),
            "{first}"
        );

        ctx.clear_messages();
        assert!(
            ctx.announce_date(day(25)),
            "a cleared context is told again"
        );

        // A resumed session replays its records, where the reminder is a
        // message of its own.
        let mut resumed = ContextMemory::new("system", 100_000);
        resumed.replay_messages(ctx.fork_snapshot());
        assert!(
            !resumed.announce_date(day(25)),
            "the replayed reminder counts"
        );
    }
    use async_trait::async_trait;

    use crate::agent_loop::types::StepResult;
    use lm_bridge::usage::TokenUsage;

    fn text_step(text: &str, finish_reason: FinishReason) -> StepResult {
        StepResult {
            message: Message::assistant_text(text),
            usage: TokenUsage::empty(),
            finish_reason: Some(finish_reason),
            response_id: None,
            stop_reason: None,
        }
    }

    /// A summarizer that plays back a script of results and records what it
    /// was asked.
    struct ScriptedSummarizer {
        script: std::sync::Mutex<Vec<Result<StepResult, LlmError>>>,
        requests: std::sync::Mutex<Vec<(Vec<Message>, Vec<Tool>)>>,
    }

    impl ScriptedSummarizer {
        fn new(script: Vec<Result<StepResult, LlmError>>) -> Self {
            Self {
                script: std::sync::Mutex::new(script),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn answering(summary: &str) -> Self {
            Self::new(vec![Ok(text_step(summary, FinishReason::Stop))])
        }

        fn requests(&self) -> Vec<(Vec<Message>, Vec<Tool>)> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Llm for ScriptedSummarizer {
        fn model_name(&self) -> &str {
            "scripted-summarizer"
        }

        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: crate::agent_loop::llm::LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, LlmError> {
            panic!("compaction must use summarize, not chat");
        }

        async fn summarize(
            &self,
            messages: Vec<Message>,
            tools: Vec<Tool>,
            _max_output_tokens: Option<u32>,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, LlmError> {
            self.requests.lock().unwrap().push((messages, tools));
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                Err(LlmError::ApiConnection("script exhausted".into()))
            } else {
                script.remove(0)
            }
        }
    }

    fn inputs<'a>(
        llm: Option<&'a dyn Llm>,
        tools: &'a [Tool],
        todo: Option<&str>,
        cancel: &'a CancellationToken,
    ) -> CompactionInputs<'a> {
        CompactionInputs {
            llm,
            tools,
            todo: todo.map(str::to_string),
            instruction: None,
            cancel,
        }
    }

    /// A small window holding a task statement, many large tool results, and a
    /// latest request — the shape proactive compaction meets in practice.
    fn crowded_context() -> ContextMemory {
        let mut ctx = ContextMemory::new("system", 4_000);
        ctx.add_user_message("the task: port the parser to Rust");
        for i in 0..12 {
            let id = format!("tc{i}");
            ctx.add_message(Message::assistant_tool_calls(vec![ToolCall::new(
                &id, "bash",
            )]));
            ctx.add_message(Message::tool_result(&id, "log line\n".repeat(150)));
        }
        ctx.add_user_message("now also add tests");
        ctx.add_message(Message::assistant_tool_calls(vec![ToolCall::new(
            "last", "bash",
        )]));
        ctx.add_message(Message::tool_result("last", "ok"));
        ctx
    }

    fn texts(ctx: &ContextMemory) -> Vec<String> {
        ctx.messages.iter().map(Message::text_content).collect()
    }

    #[test]
    fn test_context_budget_threshold_is_ninety_percent() {
        let budget = ContextBudget::new(1000);
        assert_eq!(budget.max_tokens(), 1000);
        assert_eq!(budget.proactive_threshold(), 900);
    }

    #[test]
    fn test_context_budget_block_threshold_is_fifty_percent() {
        let budget = ContextBudget::new(1000);
        assert_eq!(budget.block_threshold(), 500);
    }

    /// A model that reserves output compacts before the context grows into
    /// it: DeepSeek's 256K of a 1M window moves the trigger to 786,432.
    #[test]
    fn an_output_reservation_lowers_the_proactive_threshold() {
        let deepseek = ContextBudget::reserving_output(1_048_576, Some(262_144));
        assert_eq!(deepseek.proactive_threshold(), 786_432);
        assert_eq!(deepseek.block_threshold(), 524_288);
        // A reservation inside the 10 % margin changes nothing.
        assert_eq!(
            ContextBudget::reserving_output(1000, Some(50)).proactive_threshold(),
            900
        );
        // One that would leave less than the compaction target holds at it.
        assert_eq!(
            ContextBudget::reserving_output(1000, Some(900)).proactive_threshold(),
            500
        );
        assert_eq!(
            ContextBudget::reserving_output(1000, None).proactive_threshold(),
            900
        );
    }

    /// Proactive compaction keeps the first and the latest user message,
    /// writes the summary, and ends with the continuation anchor.
    #[tokio::test]
    async fn proactive_compaction_summarizes_and_anchors() {
        let mut ctx = crowded_context();
        assert!(ctx.needs_proactive_compaction());
        let llm = ScriptedSummarizer::answering("Ported the lexer; tests remain.");
        let tools = [Tool::new("bash", "Run a command", serde_json::json!({}))];
        let cancel = CancellationToken::new();

        let notice = ctx
            .compact(
                inputs(Some(&llm), &tools, Some("[ ] 1: write tests"), &cancel),
                ctx.budget.block_threshold(),
            )
            .await
            .unwrap();

        assert!(
            matches!(notice, CompactionNotice::Summarized { tokens_before, tokens_after } if tokens_after < tokens_before),
            "{notice:?}"
        );
        let texts = texts(&ctx);
        assert_eq!(texts[0], "the task: port the parser to Rust");
        assert_eq!(texts[1], "now also add tests");
        assert!(texts[2].starts_with(context_compaction::COMPACTION_SUMMARY_PREFIX));
        assert!(texts[2].contains("Ported the lexer; tests remain."));
        assert!(texts[2].contains("## TODO list\n[ ] 1: write tests"));
        assert_eq!(texts[3], context_compaction::COMPACTION_CONTINUATION_TEXT);
        assert_eq!(texts.len(), 4);
        assert!(!ctx.needs_proactive_compaction());

        // The summarizer saw the real conversation, the conversation's tools,
        // and the handoff instruction last.
        let (request, request_tools) = &llm.requests()[0];
        assert_eq!(request[0].role, Role::System);
        assert!(request.iter().any(|m| m.role == Role::Tool));
        assert!(
            request
                .last()
                .unwrap()
                .text_content()
                .starts_with("You are about to run out of context.")
        );
        assert_eq!(request_tools, &tools);
    }

    /// When no summary can be had, the oldest messages go — but never the
    /// first or the latest user message — and a notice says what happened.
    #[tokio::test]
    async fn failed_summaries_fall_back_to_protected_eviction() {
        let mut ctx = crowded_context();
        let llm = ScriptedSummarizer::new(vec![Err(LlmError::Config("bad model".into()))]);
        let cancel = CancellationToken::new();

        let notice = ctx
            .compact(
                inputs(Some(&llm), &[], None, &cancel),
                ctx.budget.block_threshold(),
            )
            .await
            .unwrap();

        let CompactionNotice::Evicted {
            removed, reason, ..
        } = notice
        else {
            panic!("expected an eviction, got {notice:?}");
        };
        assert!(removed > 0);
        assert!(reason.contains("bad model"));
        let texts = texts(&ctx);
        assert_eq!(texts[0], "the task: port the parser to Rust");
        assert!(texts[1].starts_with(&format!(
            "<system-reminder>\n{removed} earlier messages were removed to free context"
        )));
        assert!(texts.contains(&"now also add tests".to_string()));
        assert_eq!(texts.last().unwrap(), "ok");
        assert!(ctx.estimate_tokens() <= ctx.budget.block_threshold());
        // Every tool result that remains still follows its call.
        for (i, message) in ctx.messages.iter().enumerate() {
            if let Some(id) = &message.tool_call_id {
                assert!(
                    ctx.messages[..i]
                        .iter()
                        .any(|m| m.tool_calls.iter().any(|tc| &tc.id == id)),
                    "tool result {id} lost its call"
                );
            }
        }
    }

    /// Without a model, compaction still frees space by eviction.
    #[tokio::test]
    async fn without_a_model_compaction_evicts() {
        let mut ctx = crowded_context();
        let cancel = CancellationToken::new();
        let notice = ctx
            .compact(
                inputs(None, &[], None, &cancel),
                ctx.budget.block_threshold(),
            )
            .await
            .unwrap();
        assert!(
            matches!(notice, CompactionNotice::Evicted { .. }),
            "{notice:?}"
        );
    }

    /// An overflowing summarizer request is retried on less history.
    #[tokio::test]
    async fn an_overflowing_summary_request_is_retried_smaller() {
        let mut ctx = crowded_context();
        let llm = ScriptedSummarizer::new(vec![
            Err(LlmError::ApiContextOverflow),
            Ok(text_step("summary", FinishReason::Stop)),
        ]);
        let cancel = CancellationToken::new();
        let notice = ctx
            .compact(
                inputs(Some(&llm), &[], None, &cancel),
                ctx.budget.block_threshold(),
            )
            .await
            .unwrap();
        assert!(
            matches!(notice, CompactionNotice::Summarized { .. }),
            "{notice:?}"
        );
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].0.len() < requests[0].0.len());
    }

    /// Empty and cut-off summaries are retried on less history; the attempt
    /// budget bounds the whole compaction.
    #[tokio::test]
    async fn empty_and_cut_off_summaries_are_retried_within_the_budget() {
        let mut ctx = crowded_context();
        let llm = ScriptedSummarizer::new(vec![
            Ok(text_step("", FinishReason::Stop)),
            Ok(text_step("half a summ", FinishReason::Length)),
            Ok(text_step("a real summary", FinishReason::Stop)),
        ]);
        let cancel = CancellationToken::new();
        let notice = ctx
            .compact(
                inputs(Some(&llm), &[], None, &cancel),
                ctx.budget.block_threshold(),
            )
            .await
            .unwrap();
        assert!(
            matches!(notice, CompactionNotice::Summarized { .. }),
            "{notice:?}"
        );
        assert!(texts(&ctx).iter().any(|t| t.contains("a real summary")));

        let mut ctx = crowded_context();
        ctx.set_compaction_max_attempts(2);
        let llm = ScriptedSummarizer::new(vec![
            Ok(text_step("", FinishReason::Stop)),
            Ok(text_step("", FinishReason::Stop)),
            Ok(text_step("too late", FinishReason::Stop)),
        ]);
        let notice = ctx
            .compact(
                inputs(Some(&llm), &[], None, &cancel),
                ctx.budget.block_threshold(),
            )
            .await
            .unwrap();
        assert!(
            matches!(notice, CompactionNotice::Evicted { .. }),
            "{notice:?}"
        );
        assert_eq!(llm.requests().len(), 2);
    }

    /// A transient failure is retried after a backoff.
    #[tokio::test(start_paused = true)]
    async fn transient_summary_failures_are_retried() {
        let mut ctx = crowded_context();
        let llm = ScriptedSummarizer::new(vec![
            Err(LlmError::ApiConnection("reset".into())),
            Ok(text_step("summary", FinishReason::Stop)),
        ]);
        let cancel = CancellationToken::new();
        let notice = ctx
            .compact(
                inputs(Some(&llm), &[], None, &cancel),
                ctx.budget.block_threshold(),
            )
            .await
            .unwrap();
        assert!(
            matches!(notice, CompactionNotice::Summarized { .. }),
            "{notice:?}"
        );
    }

    /// The summarizer's input is trimmed to the window before the first
    /// request, oldest messages first.
    #[tokio::test]
    async fn the_summary_request_is_fitted_to_the_window() {
        let mut ctx = ContextMemory::new("system", 8_000);
        ctx.add_user_message("the task");
        for i in 0..20 {
            let id = format!("tc{i}");
            ctx.add_message(Message::assistant_tool_calls(vec![ToolCall::new(
                &id, "bash",
            )]));
            ctx.add_message(Message::tool_result(&id, "x".repeat(2_000)));
        }
        let history_tokens = estimate_messages(&ctx.messages);
        assert!(history_tokens > 8_000, "{history_tokens}");
        let llm = ScriptedSummarizer::answering("summary");
        let cancel = CancellationToken::new();
        ctx.compact(inputs(Some(&llm), &[], None, &cancel), 4_000)
            .await
            .unwrap();
        let (request, _) = &llm.requests()[0];
        let tokens = estimate_messages(request);
        assert!(tokens < 8_000, "{tokens}");
        // The newest exchanges are the ones kept, and trimming never leaves a
        // tool result without its call.
        assert_eq!(
            request[request.len() - 2].tool_call_id.as_deref(),
            Some("tc19")
        );
        assert_ne!(request[1].role, Role::Tool);
    }

    /// With records, the summary points back into them and is itself recorded;
    /// replaying the records reproduces the compacted history.
    #[tokio::test]
    async fn compaction_points_into_the_records_and_replays() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let mut ctx = crowded_context().with_records(Records::new(&path));
        // `crowded_context` added messages before the records were attached;
        // write them as a session would have.
        for message in ctx.messages.clone() {
            Records::new(&path).append(&message).await.unwrap();
        }
        let before_lines = ctx.messages.len();
        let llm = ScriptedSummarizer::answering("summary text");
        let cancel = CancellationToken::new();
        ctx.compact(
            inputs(Some(&llm), &[], None, &cancel),
            ctx.budget.block_threshold(),
        )
        .await
        .unwrap();
        let summary = texts(&ctx)[2].clone();
        assert!(summary.contains(&path.display().to_string()), "{summary}");
        assert!(
            summary.contains(&format!("lines 1–{before_lines}")),
            "{summary}"
        );

        ctx.add_user_message("after compaction");
        ctx.persist().await.unwrap();

        let mut replayed = ContextMemory::new("system", 4_000);
        replayed.replay_messages(Records::new(&path).load().await.unwrap());
        assert_eq!(texts(&replayed), texts(&ctx));
    }

    /// Compaction rewrites the history; persisting afterwards neither panics
    /// nor loses or repeats a message.
    #[tokio::test]
    async fn persisting_after_compaction_writes_each_message_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let mut ctx = ContextMemory::new("system", 400).with_records(Records::new(&path));
        for i in 0..30 {
            ctx.add_user_message(format!("message {i} {}", "pad ".repeat(20)));
        }
        ctx.persist().await.unwrap();
        ctx.evict_protected(0);
        ctx.add_user_message("new");
        ctx.persist().await.unwrap();

        let recorded = Records::new(&path).load().await.unwrap();
        assert_eq!(recorded.len(), 31);
        assert_eq!(recorded.last().unwrap().text_content(), "new");
    }

    #[test]
    fn test_compact_to_evicts_whole_tool_exchange_block() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("first user");
        ctx.add_message(Message::assistant_tool_calls(vec![ToolCall::new(
            "tc1", "bash",
        )]));
        ctx.add_message(Message::tool_result("tc1", "output"));
        ctx.add_user_message("second user");

        // Force compaction down to a single message; the oldest tool-call block
        // must be removed as a unit so the remaining history stays valid.
        let evicted = ctx.compact_to(1, CompactMode::AllButSystem);

        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].text_content(), "second user");
        assert_eq!(evicted.len(), 3);
        assert!(evicted.iter().any(|m| m.role == Role::Assistant));
        assert!(evicted.iter().any(|m| m.role == Role::Tool));
    }

    #[test]
    fn test_compact_emergency_keeps_recent_messages() {
        let mut ctx = ContextMemory::new("system", 1000);
        for i in 0..20 {
            ctx.add_user_message(format!("message {i}"));
        }
        ctx.compact_emergency();
        assert_eq!(ctx.messages.len(), EMERGENCY_PRESERVE_MESSAGES);
        assert_eq!(ctx.messages.last().unwrap().text_content(), "message 19");
    }

    #[test]
    fn test_small_contexts_need_no_compaction() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("hi");
        assert!(!ctx.should_compact_proactively());
        assert!(!ctx.needs_proactive_compaction());
    }

    #[test]
    fn test_compact_to_evicts_and_returns_messages() {
        let mut ctx = ContextMemory::new("system", 1000);
        for i in 0..5 {
            ctx.add_user_message(format!("message {i}"));
        }
        let evicted = ctx.compact_to(0, CompactMode::AllButSystem);
        assert_eq!(evicted.len(), 3); // 5 - MIN_PRESERVED_MESSAGES
        assert_eq!(ctx.messages.len(), MIN_PRESERVED_MESSAGES);
    }

    #[test]
    fn test_compact_to_preserves_head_user_messages_when_pool_is_large() {
        let mut ctx = ContextMemory::new("system", 1_000_000);
        // Create a user-message pool larger than the 20k-token threshold.
        for _ in 0..300 {
            ctx.add_user_message("x".repeat(300));
        }
        // Add a final assistant message so there is something recent to keep.
        ctx.add_assistant_text("latest");
        let before = ctx.messages.len();
        let evicted = ctx.compact_to(0, CompactMode::AllButSystem);
        assert!(!evicted.is_empty());
        // The oldest user messages should still be present (head preservation).
        assert!(
            ctx.messages
                .iter()
                .any(|m| { m.role == Role::User && m.text_content().len() >= 300 })
        );
        assert!(ctx.messages.len() < before);
    }

    #[test]
    fn test_compact_to_tool_results_only_prefers_tool_messages() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("user");
        ctx.add_assistant_text("ok");
        ctx.add_message(Message::tool_result("tc1", "result"));
        ctx.add_message(Message::tool_result("tc2", "result"));
        let evicted = ctx.compact_to(0, CompactMode::ToolResultsOnly);
        assert_eq!(evicted.len(), 2);
        assert!(evicted.iter().all(|m| m.role == Role::Tool));
        assert_eq!(ctx.messages.len(), 2); // user + assistant preserved
    }

    fn image_result(id: &str, path: &str, bytes: usize) -> Message {
        let mut message = Message::tool_result(id, "");
        message.content = vec![
            ContentPart::text(format!(
                "<system>Image file: {path}\nimage/png, {bytes} bytes.</system>"
            )),
            ContentPart::ImageUrl {
                image_url: lm_bridge::message::ImageUrl {
                    url: format!("data:image/png;base64,{}", "A".repeat(bytes)),
                    id: None,
                },
            },
        ];
        message
    }

    /// The oldest half of the inline media goes, each note naming its file.
    #[test]
    fn test_omit_oldest_media_names_the_file() {
        let mut ctx = ContextMemory::new("system", 1_000_000);
        ctx.add_user_message("look");
        ctx.add_message(image_result("tc1", "/shots/one.png", 1_000));
        ctx.add_message(image_result("tc2", "/shots/two.png", 1_000));
        ctx.add_message(image_result("tc3", "/shots/three.png", 1_000));

        assert_eq!(ctx.omit_oldest_media(), 2);
        assert_eq!(
            ctx.messages[1].content[1].clone(),
            ContentPart::text("[image omitted to fit the request size limit — /shots/one.png]")
        );
        assert!(matches!(
            ctx.messages[3].content[1],
            ContentPart::ImageUrl { .. }
        ));
        assert_eq!(ctx.omit_oldest_media(), 1);
        assert_eq!(ctx.omit_oldest_media(), 0, "nothing left to drop");
    }

    #[test]
    fn test_evict_protected_keeps_first_and_latest_user_messages() {
        let mut ctx = ContextMemory::new("system", 1_000);
        ctx.add_user_message("first");
        for i in 0..10 {
            ctx.add_user_message(format!("middle {i}"));
            ctx.add_assistant_text("reply ".repeat(50));
        }
        ctx.add_user_message("latest");
        ctx.add_assistant_text("working on it");
        let removed = ctx.evict_protected(0);
        assert!(removed > 0);
        assert_eq!(texts(&ctx), ["first", "latest", "working on it"]);
    }

    #[test]
    fn test_undo_last_turn_removes_user_and_after() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("first");
        ctx.add_assistant_text("answer one");
        ctx.add_user_message("second");
        ctx.add_assistant_text("answer two");
        ctx.add_message(Message::tool_result("tc", "result"));

        let removed = ctx.undo_last_turn();
        assert_eq!(removed, 3);
        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(ctx.messages[0].text_content(), "first");
        assert_eq!(ctx.messages[1].text_content(), "answer one");
    }

    /// An injected reminder is not the start of a turn.
    #[test]
    fn test_undo_skips_injected_reminders() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("request");
        ctx.add_assistant_text("answer");
        ctx.add_user_message("<system-reminder>\nnote\n</system-reminder>");
        ctx.add_assistant_text("more");
        assert_eq!(ctx.undo_last_turn(), 4);
        assert!(ctx.messages.is_empty());
    }

    /// `/undo <count>` removes that many turns: the cut lands on the
    /// count-th most recent user message, and everything after it goes.
    #[test]
    fn test_undo_turns_removes_several_turns() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("first");
        ctx.add_assistant_text("answer one");
        ctx.add_user_message("second");
        ctx.add_assistant_text("answer two");
        ctx.add_user_message("third");
        ctx.add_assistant_text("answer three");

        assert_eq!(ctx.undoable_turns(), 3);
        // The cut lands on "first", the third user message from the end;
        // everything after it goes.
        let removed = ctx.undo_turns(3).unwrap();
        assert_eq!(removed, 6);
        assert!(ctx.messages.is_empty());
        assert_eq!(ctx.undoable_turns(), 0);
    }

    /// No argument keeps the single-turn behaviour.
    #[test]
    fn test_undo_turns_one_matches_undo_last_turn() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("first");
        ctx.add_assistant_text("answer one");
        ctx.add_user_message("second");
        ctx.add_assistant_text("answer two");

        assert_eq!(ctx.undo_turns(1).unwrap(), 2);
        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(ctx.messages[0].text_content(), "first");
        assert_eq!(ctx.messages[1].text_content(), "answer one");
    }

    /// `/undo <count>`: a count past what can be undone is an error, not a
    /// clamp, and nothing is removed.
    #[test]
    fn test_undo_turns_past_the_available_turns_errors() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("first");
        ctx.add_assistant_text("answer one");
        ctx.add_user_message("second");
        ctx.add_assistant_text("answer two");

        let err = ctx.undo_turns(3).unwrap_err();
        assert_eq!(
            err,
            UndoError::Insufficient {
                requested: 3,
                undoable: 2
            }
        );
        assert_eq!(
            err.to_string(),
            "Nothing to undo: only 2 of 3 requested turn(s) available"
        );
        // The precheck ran before anything was cut.
        assert_eq!(ctx.messages.len(), 4);
        assert_eq!(ctx.undoable_turns(), 2);

        let mut empty = ContextMemory::new("system", 1000);
        assert_eq!(
            empty.undo_turns(1),
            Err(UndoError::Insufficient {
                requested: 1,
                undoable: 0
            })
        );
    }

    /// Two cancels in a row leave a single interruption reminder: the second
    /// sees the first as the newest message and skips it.
    #[test]
    fn interruption_reminder_is_added_once() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("request");
        ctx.add_interruption_reminder();
        ctx.add_interruption_reminder();
        let reminders = ctx
            .messages
            .iter()
            .filter(|m| crate::prompt::is_interruption_reminder(&m.text_content()))
            .count();
        assert_eq!(reminders, 1);
    }

    /// The interruption reminder belongs to the turn it closes: undoing the
    /// turn takes it away with the interrupted request.
    #[test]
    fn undo_removes_the_interruption_reminder() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("request");
        ctx.add_interruption_reminder();
        assert_eq!(ctx.undo_last_turn(), 2);
        assert!(ctx.messages.is_empty());
    }

    /// A partial, empty assistant message is not "the last message" for the
    /// once-only guard: what a cancelled turn leaves behind is skipped.
    #[test]
    fn interruption_reminder_ignores_trailing_partial_assistant() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("request");
        ctx.add_interruption_reminder();
        let mut partial = Message::assistant_text("");
        partial.partial = true;
        ctx.add_message(partial);
        ctx.add_interruption_reminder();
        let reminders = ctx
            .messages
            .iter()
            .filter(|m| crate::prompt::is_interruption_reminder(&m.text_content()))
            .count();
        assert_eq!(reminders, 1);
    }

    #[tokio::test]
    async fn test_blob_store_offloads_large_tool_result() {
        let temp = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(temp.path().join("blobs")).unwrap();
        let mut ctx = ContextMemory::new("system", 10_000).with_blob_store(blob_store);

        ctx.add_user_message("read big file");
        ctx.add_message(Message::assistant_tool_calls(vec![ToolCall::new(
            "tc1",
            "read_file",
        )]));
        let payload = "x".repeat(5_000);
        ctx.add_message_with_blob(Message::tool_result("tc1", payload.clone()))
            .await
            .unwrap();

        // The in-memory message holds the small marker.
        assert!(ctx.messages[0].text_content().contains("read big file"));
        assert!(ctx.messages[2].text_content().starts_with("<blob id="));

        // build_history expands the marker back to the original payload.
        let history = ctx.build_history();
        assert_eq!(history.len(), 4);
        assert_eq!(history[3].text_content(), payload);
    }

    #[tokio::test]
    async fn test_persist_keeps_blob_marker() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path().join("records.jsonl"));
        let mut ctx = ContextMemory::new("system", 10_000).with_records(records);

        let payload = "x".repeat(5_000);
        ctx.add_message_with_blob(Message::tool_result("tc1", payload))
            .await
            .unwrap();
        ctx.persist().await.unwrap();

        let raw = tokio::fs::read_to_string(temp.path().join("records.jsonl"))
            .await
            .unwrap();
        assert!(raw.contains(r#"<blob id=\""#));
        assert!(!raw.contains("xxxx"));
    }

    #[test]
    fn test_build_repaired_history_drops_orphan_tool_results() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("call a tool");
        ctx.add_assistant_text("I will not call a tool.");
        ctx.add_message(Message::tool_result("orphan", "result"));

        let full = ctx.build_history();
        // Standard projection drops the orphan tool result.
        assert_eq!(full.len(), 3);

        let repaired = ctx.build_repaired_history();
        assert_eq!(repaired.len(), 3);
        assert!(
            !repaired.iter().any(|m| m.role == Role::Tool),
            "orphan tool result should be removed"
        );
    }

    #[test]
    fn test_build_repaired_history_keeps_matched_tool_exchange() {
        let mut ctx = ContextMemory::new("system", 1000);
        ctx.add_user_message("read it");
        ctx.add_message(Message::assistant_tool_calls(vec![ToolCall::new(
            "tc1",
            "read_file",
        )]));
        ctx.add_message(Message::tool_result("tc1", "contents"));

        let repaired = ctx.build_repaired_history();
        assert_eq!(repaired.len(), 4);
        assert!(repaired.iter().any(|m| m.role == Role::Tool));
    }

    #[test]
    fn test_close_abandoned_tool_calls_appends_error_results() {
        let mut ctx = ContextMemory::new("system", 1000);
        let calls = vec![
            ToolCall::new("tc1", "read_file"),
            ToolCall::new("tc2", "bash"),
        ];
        ctx.close_abandoned_tool_calls(&calls);

        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(ctx.messages[0].tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(ctx.messages[1].tool_call_id.as_deref(), Some("tc2"));
        assert!(
            ctx.messages[0]
                .text_content()
                .contains("abandoned before a result was received")
        );
    }
}
