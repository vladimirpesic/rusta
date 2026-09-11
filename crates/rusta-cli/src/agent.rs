//! The agent turn lifecycle — development plan §6.1, wired end-to-end (M8).
//!
//! One *user request* spans model turns until a completion carries no
//! actionable item (its prose is the answer) or the §6.1 turn cap forces a
//! wrap-up. Each turn:
//!
//! 1. assembles the prompt — sub-500-token core prompt (§6.6), compressed
//!    history (§6.6 60% rule), JIT skill-card note, loop-mitigation capsules;
//! 2. streams the completion (deltas printed live), collecting native
//!    `tool_calls` passthrough (§6.1 priority 3);
//! 3. parses actionable items — fenced tool calls and SEARCH/REPLACE blocks
//!    interleaved in **document order** (§6.1 step 2), executed through the
//!    phase-gated registry (`rusta-tools`) and the shared apply chain
//!    (`rusta-edit`) — one edit mechanism, two syntaxes (§6.4);
//! 4. on an applied edit batch: `EditsApplied` → auto-commit (§6.9) →
//!    validators (§6.7) → `Gate` verdict → `ValidationPassed`/`Failed`.
//!
//! Ctrl-C aborts the stream, discards the partial turn, and fires
//! `UserInterrupt` (§6.1 step 5). Every phase transition goes through the
//! §6.4 machine and is journaled as a `StateChange` event (§6.10).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rusta_core::{
    Escalation, Event, Machine, PhaseEvent, State, Status, Tool, Trip, core_prompt,
    core_prompt_tokens, corrective_note, prompt, regress_for_escalation,
};
use rusta_dispatch::parse_tool_calls;
use rusta_edit::{AppliedBlock, EditBlock, ParsedResponse, UndoEntry};
use rusta_llm::tokens::estimate_tokens;
use rusta_llm::{ChatRequest, Message, StreamEvent};
use rusta_tools::{Approver, Decision, Responder};
use rusta_validate::Verdict;
use serde_json::Value;

use crate::git;
use crate::repl::{App, Batch};

/// The §6.1 wrap-up capsule injected when the turn cap is reached.
pub(crate) const WRAP_UP: &str = "TURN BUDGET REACHED: summarize what was done, what remains, \
     and stop — no more tool calls or edits.";

/// Terminal-line cap for the commit-message summary of a user request.
const SUMMARY_CHARS: usize = 72;

/// The §6.4 plan-approval gate (`Planning → Editing`): the CLI prompts the
/// user y/n; `/auto` and non-interactive `-c` runs approve automatically.
pub trait PlanGate: Send {
    /// Approve the drafted `plan`?
    fn approve(&mut self, plan: &str) -> bool;
}

/// Auto-approving gate — `/auto` and `-c` mode (§6.4).
pub struct AutoGate;

impl PlanGate for AutoGate {
    fn approve(&mut self, _plan: &str) -> bool {
        true
    }
}

/// The terminal y/n gate. Reads one line from stdin; anything other than an
/// affirmative declines (the model revises or answers without edits).
pub struct TerminalGate {
    /// Shared `/auto` flag — when set, approval is implied (§6.4).
    pub auto: Arc<AtomicBool>,
}

impl PlanGate for TerminalGate {
    fn approve(&mut self, plan: &str) -> bool {
        if self.auto.load(Ordering::Relaxed) {
            return true;
        }
        println!("--- plan ---\n{plan}\n-------------");
        print!("Approve plan? (y/n) ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut answer = String::new();
        let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer);
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// Interactive `shell` approval (§6.12): `y` / `n` / `a` = always this
/// session; a set `/auto` flag implies `a`.
pub struct TerminalApprover {
    /// Shared `/auto` flag.
    pub auto: Arc<AtomicBool>,
    /// Latched by `a` — approve the rest of the session (§6.12).
    pub always: AtomicBool,
}

impl Approver for TerminalApprover {
    fn approve(&mut self, command: &str) -> Decision {
        if self.auto.load(Ordering::Relaxed) || self.always.load(Ordering::Relaxed) {
            return Decision::Allow;
        }
        println!("shell command: {command}");
        print!("Run it? (y/n/a = always this session) ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut answer = String::new();
        let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer);
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" => Decision::Allow,
            "a" => {
                self.always.store(true, Ordering::Relaxed);
                Decision::Allow
            }
            _ => Decision::Deny,
        }
    }
}

/// Interactive `ask` responder: the question is printed, the reply read from
/// stdin, and the turn continues with it as the next observation (§6.4).
pub struct TerminalResponder;

impl Responder for TerminalResponder {
    fn reply(&mut self, question: &str) -> String {
        println!("? {question}");
        print!("> ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut answer = String::new();
        let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer);
        let trimmed = answer.trim();
        if trimmed.is_empty() {
            "(user gave no answer — proceed with your best judgment)".to_owned()
        } else {
            trimmed.to_owned()
        }
    }
}

/// One actionable thing a completion asked for, in document order (§6.1).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Item {
    /// SEARCH/REPLACE blocks from one prose run (§6.3, applied as a group).
    Blocks(Vec<EditBlock>),
    /// A tool call — fenced JSON or native passthrough (§6.1).
    Call {
        /// Wire tool name.
        name: String,
        /// Input object.
        input: Value,
    },
}

/// A native `tool_calls` entry converted for execution (§6.1 priority 3).
#[derive(Debug, Clone)]
pub(crate) struct NativeCall {
    /// Tool name.
    pub name: String,
    /// Parsed arguments (`{"_raw": ...}` when the model emitted invalid JSON;
    /// the handler then answers with a remedy observation).
    pub input: Value,
}

/// Parses a completion into document-ordered items plus corrective notes.
///
/// The text is split at ` ```tool ` fences: each prose segment goes through
/// `rusta_edit::parse_response` (blocks + suggested commands), the fences
/// through `rusta_dispatch::parse_tool_calls` — both syntaxes keep their
/// proven parsers and still execute in the order the model wrote them.
/// Native `tool_calls` execute after the text items, in server order.
pub(crate) fn parse_items(text: &str, native: &[NativeCall]) -> (Vec<Item>, Vec<String>) {
    let mut items: Vec<Item> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut prose = String::new();

    fn flush(items: &mut Vec<Item>, notes: &mut Vec<String>, prose: &mut String) {
        if !prose.trim().is_empty() {
            let parsed = rusta_edit::parse_response(prose);
            if !parsed.blocks.is_empty() {
                items.push(Item::Blocks(parsed.blocks));
            }
            notes.extend(parsed.notes);
        }
        prose.clear();
    }

    let mut in_fence = false;
    let mut fence_body = String::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !in_fence && trimmed.starts_with("```tool") {
            flush(&mut items, &mut notes, &mut prose);
            in_fence = true;
            fence_body.clear();
        } else if in_fence && trimmed.starts_with("```") {
            // The proven fence parser, scoped to this one block — so calls
            // interleave with prose edit blocks in true document order.
            let calls = parse_tool_calls(&format!("```tool\n{fence_body}\n```"));
            for call in calls.calls {
                items.push(Item::Call {
                    name: call.name,
                    input: call.input,
                });
            }
            notes.extend(calls.notes);
            in_fence = false;
        } else if in_fence {
            fence_body.push_str(line);
        } else {
            prose.push_str(line);
        }
    }
    // Unterminated fence at EOF: parse what was gathered (§6.1 forgiveness).
    if in_fence {
        let calls = parse_tool_calls(&format!("```tool\n{fence_body}\n```"));
        for call in calls.calls {
            items.push(Item::Call {
                name: call.name,
                input: call.input,
            });
        }
        notes.extend(calls.notes);
    }
    flush(&mut items, &mut notes, &mut prose);

    for native_call in native {
        items.push(Item::Call {
            name: native_call.name.clone(),
            input: native_call.input.clone(),
        });
    }
    (items, notes)
}

/// §6.4 plan detection: a completion with no actionable items counts as a
/// drafted change-plan when it mentions a plan and carries a numbered list
/// (the core prompt's format). Conservative by construction — plain answers
/// end the turn with the state unchanged (§6.4 "Read-only Q&A").
pub(crate) fn is_plan(text: &str) -> bool {
    let mentions_plan = text.to_lowercase().contains("plan");
    let numbered = text.lines().any(|line| {
        let trimmed = line.trim_start();
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && matches!(trimmed.chars().nth(digits), Some('.' | ')'))
    });
    mentions_plan && numbered
}

/// The one-line summary of a user request for commit messages (§6.9):
/// first line, whitespace-collapsed, capped at [`SUMMARY_CHARS`] with `…`.
pub(crate) fn request_summary(request: &str) -> String {
    let first = request.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut summary: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if summary.chars().count() > SUMMARY_CHARS {
        let mut end = SUMMARY_CHARS - 1;
        while end > 0 && !summary.is_char_boundary(end) {
            end -= 1;
        }
        summary.truncate(end);
        summary.push('…');
    }
    summary
}

// --------------------------------------------------------- the App agent half

impl App {
    /// Runs one full user request through the §6.1 turn lifecycle: model
    /// turns continue while completions carry actionable items, until a
    /// prose-only answer ends the request, the §6.7 gate finishes it, or the
    /// turn cap forces a wrap-up.
    pub async fn submit(&mut self, request: &str) {
        let summary = request_summary(request);
        let _ = self.session.record(Event::UserMessage {
            content: request.to_owned(),
        });
        self.history.push(Message::user(request));

        let mut turn: u32 = 0;
        loop {
            turn += 1;
            let wrapup = turn > self.config.agent.max_turns;

            let messages = self.assemble(wrapup);
            let chat = ChatRequest {
                messages,
                max_tokens: self.config.model.max_tokens,
                temperature: self.config.model.temperature,
                stop: Vec::new(),
            };
            let Some((text, native)) = self.stream_turn(chat).await else {
                return; // aborted or failed — already reported, turn discarded
            };

            let _ = self.session.record(Event::AssistantMessage {
                content: text.clone(),
            });
            self.history.push(Message::assistant(text.clone()));
            self.guard.end_turn();

            if wrapup {
                self.reporter
                    .line("(turn budget reached — returned control)");
                return;
            }

            let (items, notes) = parse_items(&text, &native);
            if items.is_empty() && notes.is_empty() {
                if self.handle_prose_turn(&text) {
                    continue; // plan drafted/approved — the loop continues
                }
                return; // plain answer: control back to the user
            }

            let undo_before = self.tools.editor().undo_stack().len();
            for item in items {
                match item {
                    Item::Blocks(blocks) => self.apply_blocks(blocks),
                    Item::Call { name, input } => self.exec_call(&name, &input).await,
                }
            }
            if !notes.is_empty() {
                self.push_observation("notes", &notes.join("\n"), Status::Error);
            }

            let new_entries = self.tools.editor().undo_stack().len() - undo_before;
            if new_entries > 0 && !self.finish_batch(&summary, undo_before).await {
                return; // green gate or surfaced failure: request complete
            }
        }
    }

    /// A completion with no actionable items: the §6.4 plan gate (in the
    /// read-only states) or a plain answer. Returns `true` when the request
    /// must continue (a plan was drafted and ruled on — the model redrafts
    /// or proceeds to edits); `false` when the answer ends the request.
    fn handle_prose_turn(&mut self, text: &str) -> bool {
        if matches!(self.machine.state(), State::Exploring | State::Planning) && is_plan(text) {
            if self.machine.state() == State::Exploring {
                self.fire(PhaseEvent::PlanDrafted);
            }
            if self.plan_gate.approve(text) {
                self.fire(PhaseEvent::PlanApproved);
                self.push_observation(
                    "plan gate",
                    "PLAN APPROVED — apply the edits now.",
                    Status::Ok,
                );
            } else {
                self.push_observation(
                    "plan gate",
                    "PLAN DECLINED — revise the plan, or answer without edits.",
                    Status::Error,
                );
            }
            return true; // either way the model gets another turn
        }
        false // plain prose answer (§6.1 step 3)
    }

    /// Journals an applied batch (§6.10 `EditApplied` + sidecar), fires the
    /// §6.4 exit gate, auto-commits (§6.9), and runs the §6.7 validation
    /// round. `undo_before` is the journal length before the batch. Returns
    /// `false` when the request must stop (green gate, or surfaced failure).
    async fn finish_batch(&mut self, summary: &str, undo_before: usize) -> bool {
        let entries: Vec<UndoEntry> =
            self.tools.editor().undo_stack().pending()[undo_before..].to_vec();
        for entry in &entries {
            let _ = self.session.record_edit(
                &entry.path.display().to_string(),
                entry.existed,
                &entry.before,
                &entry.after,
            );
        }
        // §6.4 exit gate: the batch is applied → Verifying.
        self.fire(PhaseEvent::EditsApplied);
        // §6.9 auto-commit after each applied edit batch.
        let mut paths: Vec<String> = Vec::new();
        for entry in &entries {
            let display = entry.path.display().to_string();
            if !paths.contains(&display) {
                paths.push(display);
            }
        }
        let message = format!("{}{}", git::COMMIT_PREFIX, summary);
        let sha = git::commit_batch(&self.git, &paths, summary).await;
        if let Some(sha) = &sha {
            let _ = self.session.record(Event::Commit {
                sha: sha.clone(),
                message,
            });
        }
        self.batches.push(Batch {
            entries: entries.len(),
            paths,
            sha,
        });
        // §6.7 validators + the Reflexion gate decide the rest.
        self.validate_round().await
    }

    /// Assembles the request messages: core prompt, compressed history
    /// (§6.6), the JIT skill-card note, loop-mitigation capsules, and — at
    /// the cap — the §6.1 wrap-up capsule.
    fn assemble(&mut self, wrapup: bool) -> Vec<Message> {
        let state = self.machine.state();
        let capsule = self.guard.capsule_note();
        let cues: Vec<&str> = self.card_cues.iter().map(String::as_str).collect();
        let skill = self.deck.skill_note(&cues);
        self.card_cues.clear();

        let mut reserved = core_prompt_tokens(state);
        if let Some(note) = &capsule {
            reserved += estimate_tokens(&note.content);
        }
        if let Some(note) = &skill {
            reserved += estimate_tokens(&note.content);
        }
        let compression = self
            .compressor
            .compress(std::mem::take(&mut self.history), reserved);
        if let Some(plan) = &compression.summary {
            let _ = self.session.record(Event::Summary {
                covers_turns: plan.covers_turns,
                text: plan.text.clone(),
            });
        }
        self.history = compression.messages;

        let mut messages = vec![core_prompt(state)];
        messages.extend(self.history.iter().cloned());
        if let Some(note) = skill {
            messages.push(note);
        }
        if let Some(note) = capsule {
            messages.push(note);
        }
        if wrapup {
            messages.push(Message::system(WRAP_UP));
        }
        messages
    }

    /// Streams one completion: deltas print live, native `tool_calls` are
    /// collected (§6.1 priority 3), Ctrl-C aborts and discards the partial
    /// turn (§6.1 step 5). `None` means the turn must not be recorded.
    async fn stream_turn(&mut self, request: ChatRequest) -> Option<(String, Vec<NativeCall>)> {
        let mut rx = match self.tools.backend().stream(request).await {
            Ok(rx) => rx,
            Err(err) => {
                self.reporter.line(&format!("backend error: {err}"));
                self.fire(PhaseEvent::UserInterrupt);
                return None;
            }
        };
        let mut text = String::new();
        let mut native: Vec<NativeCall> = Vec::new();
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    drop(rx); // consumer dropped — the SSE pump ends
                    self.reporter.line("\n^C aborted — partial turn discarded");
                    self.fire(PhaseEvent::UserInterrupt);
                    return None;
                }
                event = rx.recv() => match event {
                    Some(StreamEvent::Delta(delta)) => {
                        self.reporter.raw(&delta);
                        text.push_str(&delta);
                    }
                    Some(StreamEvent::ToolCall { name, arguments, .. }) => {
                        let input = match serde_json::from_str::<Value>(&arguments) {
                            Ok(value @ Value::Object(_)) => value,
                            Ok(other) => serde_json::json!({ "_raw": other.to_string() }),
                            Err(_) => serde_json::json!({ "_raw": arguments }),
                        };
                        native.push(NativeCall { name, input });
                    }
                    Some(StreamEvent::Finish(_)) => break,
                    Some(StreamEvent::Failed(cause)) => {
                        self.reporter.line(&format!("\nstream failed: {cause}"));
                        self.fire(PhaseEvent::UserInterrupt);
                        return None;
                    }
                    None => break, // channel closed; keep what streamed
                }
            }
        }
        if !text.is_empty() && !text.ends_with('\n') {
            self.reporter.raw("\n");
        }
        Some((text, native))
    }

    /// Executes one model tool call through the phase-gated registry (§6.4),
    /// journals the call/result pair (§6.10), and pushes the §6.1 observation.
    async fn exec_call(&mut self, name: &str, input: &Value) {
        let trip = self.guard.observe_tool_call(name, input);
        self.handle_trip(trip);
        let _ = self.session.record(Event::ToolCall {
            name: name.to_owned(),
            input: input.clone(),
        });
        let outcome = self.tools.exec(self.machine.state(), name, input).await;
        let flag = if outcome.status == Status::Ok {
            "ok"
        } else {
            "error"
        };
        self.reporter.line(&format!("* {name} ({flag})"));
        let _ = self.session.record(Event::ToolResult {
            status: outcome.status,
            summary: outcome.content.clone(),
            truncated: outcome.truncated,
        });
        // §6.10 audit: successful dispatch results also become `Dispatch`
        // events, split back out of the labeled-render observation.
        if name == "dispatch" && outcome.status == Status::Ok {
            for (label, report) in split_labeled(&outcome.content) {
                let _ = self.session.record(Event::Dispatch { label, report });
            }
        }
        self.history.push(outcome.observation(name));
        self.card_cues.push(name.to_owned());
        if outcome.status == Status::Error {
            self.card_cues.push("error".to_owned());
        }
    }

    /// Applies one group of SEARCH/REPLACE blocks (§6.3) through the shared
    /// apply chain. In read-only states the blocks are never executed — the
    /// §6.4 corrective note comes back instead (never a silent drop).
    fn apply_blocks(&mut self, blocks: Vec<EditBlock>) {
        if self.machine.state() != State::Editing {
            let note = corrective_note(self.machine.state(), Tool::Edit);
            self.push_observation("edit", &note, Status::Error);
            return;
        }
        for block in &blocks {
            let trip = self.guard.observe_edit(&block.original, &block.updated);
            self.handle_trip(trip);
        }
        let parsed = ParsedResponse {
            blocks,
            commands: Vec::new(),
            notes: Vec::new(),
        };
        let report = self.tools.editor().apply_parsed(parsed);
        for applied in &report.applied {
            self.reporter.line(&format!(
                "* applied {}{}",
                applied.path.display(),
                marker_for(applied)
            ));
        }
        for command in &report.commands {
            self.reporter
                .line(&format!("! suggested (not run): {command}"));
        }

        let mut content = String::new();
        if report.is_success() {
            use std::fmt::Write as _;
            let _ = writeln!(content, "APPLIED {} edit(s):", report.applied.len());
            for applied in &report.applied {
                let _ = writeln!(
                    content,
                    "- {}{}",
                    applied.path.display(),
                    marker_for(applied)
                );
            }
            if !report.commands.is_empty() {
                let _ = writeln!(
                    content,
                    "suggested commands (NOT run): {}",
                    report.commands.join("; ")
                );
            }
            if !report.notes.is_empty() {
                content.push_str(&report.notes.join("\n"));
            }
            self.push_observation("edit", content.trim_end_matches('\n'), Status::Ok);
        } else {
            // §6.3 failure feedback is already the model-facing repair text.
            let mut feedback = report.feedback.unwrap_or_else(|| "edit failed".to_owned());
            if !report.notes.is_empty() {
                feedback.push('\n');
                feedback.push_str(&report.notes.join("\n"));
            }
            self.push_observation("edit", &feedback, Status::Error);
        }
    }

    /// One §6.7 validation round: runs the configured validators, journals
    /// `ValidationRun` events, feeds the detectors (§6.6 c), and routes the
    /// [`Gate`](rusta_validate::Gate) verdict. Returns `false` when the
    /// request must stop (green, or red with the repair budget exhausted).
    async fn validate_round(&mut self) -> bool {
        let outcome = match self.config.validate.run(&self.root).await {
            Ok(outcome) => outcome,
            Err(err) => {
                // Config misuse is pre-checked at load; treat the impossible
                // case as a green no-validators round, loudly.
                self.reporter
                    .line(&format!("validation config error: {err}"));
                rusta_validate::Outcome::default()
            }
        };
        for report in &outcome.reports {
            let _ = self.session.record(report.session_event());
            let trip = self
                .guard
                .observe_validation(&report.command, &report.output);
            self.handle_trip(trip);
        }
        match self.gate.assess(&outcome) {
            Verdict::Pass { note } => {
                self.fire(PhaseEvent::ValidationPassed);
                if let Some(text) = note {
                    self.reporter.line(text);
                    self.push_observation("validation", text, Status::Ok);
                }
                false
            }
            Verdict::Repair {
                feedback,
                attempts_left,
            } => {
                self.fire(PhaseEvent::ValidationFailed);
                let content = format!("{feedback}\nRepair attempts left: {attempts_left}.");
                self.push_observation("validation", &content, Status::Error);
                true // §6.7: the model gets the repair attempt first
            }
            Verdict::Surface { feedback } => {
                self.fire(PhaseEvent::ValidationFailed);
                self.reporter
                    .line("validation failed (repair budget exhausted):");
                self.reporter.line(&feedback);
                false
            }
        }
    }

    /// Pushes a synthetic observation into history + session (recorded as a
    /// `ToolCall`/`ToolResult` pair so `/resume` replays it verbatim, §6.10).
    fn push_observation(&mut self, name: &str, content: &str, status: Status) {
        let _ = self.session.record(Event::ToolCall {
            name: name.to_owned(),
            input: Value::Object(serde_json::Map::new()),
        });
        let _ = self.session.record(Event::ToolResult {
            status,
            summary: content.to_owned(),
            truncated: false,
        });
        self.history
            .push(prompt::observation(name, status, content));
    }

    /// Fires a scaffold event on the §6.4 machine, journaling the
    /// `StateChange`. Illegal transitions are surfaced, never fatal.
    fn fire(&mut self, event: PhaseEvent) {
        match self.machine.fire(event) {
            Ok(Some(transition)) => {
                let _ = self.session.record(Event::StateChange {
                    from: transition.from,
                    to: transition.to,
                    reason: transition.reason.to_owned(),
                });
                self.reporter
                    .line(&format!("({} → {})", transition.from, transition.to));
            }
            Ok(None) => {}
            Err(err) => self.reporter.line(&format!("(illegal {event}: {err})")),
        }
    }

    /// §6.6 escalation (a detector at 2× its threshold): `Editing → Planning`
    /// regression plus the user notification. Deliberately *not* journaled —
    /// the machine table has no such scaffold edge, and the loop guard is
    /// task-scoped and unjournaled by design ("`/resume` starts detectors
    /// fresh, which is the safe side").
    fn handle_trip(&mut self, trip: Trip) {
        if !trip.escalate {
            return;
        }
        if let Ok(target) = regress_for_escalation(self.machine.state()) {
            self.machine = Machine::resume_at(target);
            let escalation = Escalation {
                reason: trip.capsules.first().copied().unwrap_or("repeat_breaker"),
                notify: format!(
                    "loop detected ({}) — regressed to Planning for a fresh plan",
                    trip.capsules.join(", ")
                ),
            };
            self.reporter.line(&format!("! {}", escalation.notify));
        }
        // Not in Editing: the capsule note still rides the next prompt
        // (§6.6 mitigation without the regression).
    }
}

/// Marker suffix for an applied block in user/model summaries.
fn marker_for(applied: &AppliedBlock) -> &'static str {
    if applied.created {
        " (new file)"
    } else if applied.appended {
        " (appended)"
    } else if applied.cross_file {
        " (cross-file retry)"
    } else {
        ""
    }
}

/// Splits a labeled dispatch observation (§6.8 `SUB-CODER "label" REPORT:`)
/// back into `(label, report)` pairs for the §6.10 `Dispatch` events.
fn split_labeled(content: &str) -> Vec<(String, String)> {
    const HEAD: &str = "SUB-CODER \"";
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find(HEAD) {
        let after = &rest[start + HEAD.len()..];
        let Some(end) = after.find("\" REPORT:") else {
            break;
        };
        let label = after[..end].to_owned();
        let body_start = end + "\" REPORT:".len();
        let body_and_tail = &after[body_start..];
        let body_end = body_and_tail
            .find("\nSUB-CODER \"")
            .unwrap_or(body_and_tail.len());
        out.push((label, body_and_tail[..body_end].trim().to_owned()));
        rest = &body_and_tail[body_end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_preserve_document_order_across_syntaxes() {
        let text = "intro\n\
             ```tool\n{\"name\": \"read\", \"input\": {\"path\": \"a.rs\"}}\n```\n\
             middle\n\
             src/b.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n\
             ```tool\n{\"name\": \"glob\", \"input\": {}}\n```\n";
        let (items, notes) = parse_items(text, &[]);
        assert!(notes.is_empty(), "{notes:?}");
        let shape: Vec<&str> = items
            .iter()
            .map(|item| match item {
                Item::Call { name, .. } => name.as_str(),
                Item::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 1);
                    "BLOCKS"
                }
            })
            .collect();
        assert_eq!(shape, ["read", "BLOCKS", "glob"], "document order");
    }

    #[test]
    fn native_calls_run_after_text_items() {
        let text = "```tool\n{\"name\": \"read\", \"input\": {}}\n```";
        let native = [NativeCall {
            name: "shell".to_owned(),
            input: Value::Null,
        }];
        let (items, _) = parse_items(text, &native);
        let names: Vec<&str> = items
            .iter()
            .map(|i| match i {
                Item::Call { name, .. } => name.as_str(),
                Item::Blocks(_) => "BLOCKS",
            })
            .collect();
        assert_eq!(names, ["read", "shell"]);
    }

    #[test]
    fn malformed_fences_become_notes() {
        let text = "```tool\n{name: bogus}\n```";
        let (items, notes) = parse_items(text, &[]);
        assert!(items.is_empty());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("malformed tool block"), "{}", notes[0]);
    }

    #[test]
    fn plan_detection_is_conservative() {
        assert!(is_plan(
            "Plan:\n1. read src/lib.rs\n2. rename fn one to two"
        ));
        assert!(is_plan("My PLAN\n  3) change the parser"));
        assert!(!is_plan("The plant needs water.")); // no numbered list
        assert!(!is_plan("1. one\n2. two")); // no plan mention
        assert!(!is_plan("Sounds good, I will plan it out."));
    }

    #[test]
    fn request_summary_collapses_and_caps() {
        assert_eq!(
            request_summary("fix the   bug\n\nmore lines"),
            "fix the bug"
        );
        let long = "word ".repeat(40);
        let summary = request_summary(&long);
        assert!(summary.chars().count() <= 72, "{}", summary.chars().count());
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn labeled_reports_split_back_into_dispatch_events() {
        let content = "SUB-CODER \"auth\" REPORT:\nlogin lives in src/auth.rs:12\n\n\
                       SUB-CODER \"db\" REPORT:\nmigrations live in db/";
        let split = split_labeled(content);
        assert_eq!(
            split,
            vec![
                (
                    "auth".to_owned(),
                    "login lives in src/auth.rs:12".to_owned()
                ),
                ("db".to_owned(), "migrations live in db/".to_owned()),
            ]
        );
        assert!(split_labeled("no reports here").is_empty());
    }
}
