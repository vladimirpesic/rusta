//! The agent turn lifecycle — ADR §6.1, wired end-to-end (M8).
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
    Event, PhaseEvent, State, Status, Tool, Trip, core_prompt, core_prompt_tokens, corrective_note,
    prompt,
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

/// Reads one line from the terminal without parking a tokio worker.
///
/// The interactive hooks below run inside the async agent loop, and a bare
/// `stdin().read_line` there blocks a runtime thread for as long as the user
/// takes to answer — delaying any sub-coder actors in flight (§6.8 spawns up
/// to four). `repl.rs` already wraps its reedline read this way; these three
/// did not. `block_in_place` requires the multi-threaded runtime, which
/// `#[tokio::main]` provides and which is the only place these hooks are
/// installed; outside a runtime it falls back to a plain read.
fn read_user_line(answer: &mut String) {
    let mut read = || {
        let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), answer);
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(read);
        }
        // Single-threaded runtime or none: `block_in_place` would panic.
        _ => read(),
    }
}

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
        read_user_line(&mut answer);
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
        read_user_line(&mut answer);
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
        read_user_line(&mut answer);
        let trimmed = answer.trim();
        if trimmed.is_empty() {
            "(user gave no answer — proceed with your best judgment)".to_owned()
        } else {
            trimmed.to_owned()
        }
    }
}

/// Everything one completion asked for: actionable items in document order,
/// corrective notes, and suggested shell commands (§6.3 rule 5 — surfaced
/// for confirmation, never auto-executed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub items: Vec<Item>,
    pub notes: Vec<String>,
    pub commands: Vec<String>,
}

/// One actionable thing a completion asked for, in document order (§6.1).
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
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
pub struct NativeCall {
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
pub fn parse_items(text: &str, native: &[NativeCall]) -> Parsed {
    let mut out = Parsed::default();
    let mut prose = String::new();

    fn flush(out: &mut Parsed, prose: &mut String) {
        if !prose.trim().is_empty() {
            let parsed = rusta_edit::parse_response(prose);
            if !parsed.blocks.is_empty() {
                out.items.push(Item::Blocks(parsed.blocks));
            }
            out.notes.extend(parsed.notes);
            // §6.3 rule 5: a fenced shell block that is not part of an edit
            // is a *suggestion*. It must reach the user — dropping it here
            // made the whole rule dead code.
            out.commands.extend(parsed.commands);
        }
        prose.clear();
    }

    let mut in_fence = false;
    let mut fence_body = String::new();
    // A fence inside a SEARCH/REPLACE body is *content* the model is writing
    // into a file, not a call. `BlockScan` shares the §6.3 parser's marker
    // rules, so the two can never disagree about where a block ends.
    let mut block = rusta_edit::BlockScan::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if in_fence {
            if trimmed.starts_with("```") {
                // The proven fence parser, scoped to this one block — so calls
                // interleave with prose edit blocks in true document order.
                let calls = parse_tool_calls(&format!("```tool\n{fence_body}\n```"));
                for call in calls.calls {
                    out.items.push(Item::Call {
                        name: call.name,
                        input: call.input,
                    });
                }
                out.notes.extend(calls.notes);
                in_fence = false;
            } else {
                fence_body.push_str(line);
            }
            continue;
        }
        // Only advance the block scan outside fences: an `edit` tool call may
        // legitimately carry marker text inside its JSON `search` argument.
        if !block.inside(line) && trimmed.starts_with("```tool") {
            flush(&mut out, &mut prose);
            in_fence = true;
            fence_body.clear();
            continue;
        }
        prose.push_str(line);
    }
    // Unterminated fence at EOF: parse what was gathered (§6.1 forgiveness).
    if in_fence {
        let calls = parse_tool_calls(&format!("```tool\n{fence_body}\n```"));
        for call in calls.calls {
            out.items.push(Item::Call {
                name: call.name,
                input: call.input,
            });
        }
        out.notes.extend(calls.notes);
    }
    flush(&mut out, &mut prose);

    for native_call in native {
        out.items.push(Item::Call {
            name: native_call.name.clone(),
            input: native_call.input.clone(),
        });
    }
    out
}

/// §6.4 plan detection: a completion with no actionable items counts as a
/// drafted change-plan when it mentions a plan and carries a numbered list
/// (the core prompt's format). Conservative by construction — plain answers
/// end the turn with the state unchanged (§6.4 "Read-only Q&A").
pub(crate) fn is_plan(text: &str) -> bool {
    // An intent cue plus a numbered list. Requiring the literal word "plan"
    // was too brittle for small models, which routinely write "Steps:" or
    // "Here's what I'll do:" and then loop forever against the §6.4 gate.
    const CUES: [&str; 8] = [
        "plan",
        "step",
        "steps",
        "i will",
        "i'll",
        "here's what",
        "approach",
        "first,",
    ];
    let lower = text.to_lowercase();
    let signals_intent = CUES.iter().any(|cue| mentions_cue(&lower, cue));
    let numbered = text.lines().any(|line| {
        let trimmed = line.trim_start();
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && matches!(trimmed.chars().nth(digits), Some('.' | ')'))
    });
    signals_intent && numbered
}

/// Whether `lower` (already lowercased) carries the intent cue `cue`.
///
/// Single-word cues match at a **word start**, not anywhere in the text: a
/// bare `contains` made "footsteps" and "stepped" read as a drafted plan, so
/// an ordinary answer that happened to carry a numbered list tripped the
/// §6.4 approval gate — against "Read-only Q&A … the state remains
/// Exploring". Word-*start* rather than whole-word keeps the inflections
/// that matter ("planning", "steps"), which is why the literal-"plan" rule
/// was abandoned in the first place. Punctuated cues ("i will", "here's
/// what", "first,") are specific enough to match as written.
fn mentions_cue(lower: &str, cue: &str) -> bool {
    if cue.contains(|c: char| !c.is_ascii_alphanumeric()) {
        return lower.contains(cue);
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word.starts_with(cue))
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
        // Every user request is its own task: a fresh §6.7 repair bound
        // (`Gate::reset`) and fresh §6.6 detector state
        // (`LoopGuard::start_task`) — the previous request's counters never
        // bias the new one. Within a request both accumulate as usual.
        self.gate.reset();
        self.guard.start_task();
        let summary = request_summary(request);
        // §6.9 batch boundary: one user request is one `/undo` unit. This
        // is a scaffold fact, so it is journaled rather than inferred from
        // whether a git commit happened to land.
        // §6.5 steps 3–4: what the user named steers ranking. Nothing fed
        // this before, so the mention half of the spec was unreachable.
        self.tools
            .set_mentions(rusta_tools::mentioned_identifiers(request));
        self.last_request = request.to_owned();
        self.journal(Event::BatchBoundary);
        self.journal(Event::UserMessage {
            content: request.to_owned(),
        });
        self.history.push(Message::user(request));
        // §6.6 lists "keywords" alongside tool names and error kinds as a
        // card trigger axis, but nothing fed the user's own words in, so
        // keyword triggers on `knowledge` cards could never fire. The whole
        // request is one cue; `SkillCard::matches` does the word-boundary
        // work, so "spread" still never fires the `read` card.
        self.card_cues.push(request.to_owned());

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

            self.journal(Event::AssistantMessage {
                content: text.clone(),
            });
            self.history.push(Message::assistant(text.clone()));
            self.guard.end_turn();

            if wrapup {
                self.reporter
                    .line("(turn budget reached — returned control)");
                return;
            }

            let parsed = parse_items(&text, &native);
            let ends_turn = parsed.items.is_empty() && parsed.notes.is_empty();
            // The user always sees a suggested command; the model is only
            // told when the turn continues, so a prose-only answer does not
            // leave an observation with nothing to answer it.
            self.surface_commands(&parsed.commands, !ends_turn);
            if ends_turn {
                if self.handle_prose_turn(&text) {
                    continue; // plan drafted/approved — the loop continues
                }
                return; // plain answer: control back to the user
            }

            let undo_before = self.tools.editor().undo_stack().len();
            // §6.4: "at most one pending `ask` per turn". The bound lives
            // here because turns are this loop's unit — `ask.rs` said so and
            // nothing implemented it, so a completion carrying five asks put
            // five consecutive blocking prompts in front of the user inside
            // a single turn. The second one gets the standard corrective
            // note instead, never a silent drop.
            let mut asked = false;
            for item in parsed.items {
                match item {
                    Item::Blocks(blocks) => self.apply_blocks(blocks),
                    Item::Call { name, input } if name == Tool::Ask.as_str() => {
                        if asked {
                            self.push_observation(
                                &name,
                                "Only one `ask` per turn — the rest of this turn's questions were \
                                 not put to the user. Ask the single most important one, wait for \
                                 the reply, then continue.",
                                Status::Error,
                            );
                        } else {
                            asked = true;
                            self.exec_call(&name, &input).await;
                        }
                    }
                    Item::Call { name, input } => self.exec_call(&name, &input).await,
                }
            }
            if !parsed.notes.is_empty() {
                self.push_observation("notes", &parsed.notes.join("\n"), Status::Error);
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
            // The edit journal is what `/undo` restores from, so a silent
            // failure here costs more than any other dropped write.
            let recorded = self.session.record_edit(
                &entry.path.display().to_string(),
                entry.existed,
                &entry.before,
                &entry.after,
            );
            if let Err(err) = recorded {
                self.report_journal_failure(&err);
            }
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
            self.journal(Event::Commit {
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
            self.journal(Event::Summary {
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
        // §6.6's last line of defence. The compressor is best-effort — it
        // can run out of clippable material — and nothing downstream used to
        // measure the result, so an oversized request simply shipped. Over
        // HTTP that is worse than an error: llama-server and Ollama commonly
        // truncate from the *left*, which silently discards the system
        // prompt and with it every phase gate the §6.4 machine relies on.
        let window = self.tools.backend().context_window();
        let assembled: u64 = messages
            .iter()
            .map(|message| estimate_tokens(&message.content))
            .sum();
        if assembled > window {
            // Tell the user *and* the model. A warning only the human sees
            // leaves the model to wonder why its next observation is
            // truncated, and over HTTP the backend may silently drop the
            // left of the prompt — the system prompt with it.
            self.reporter.line(&format!(
                "! context overflow: {assembled} estimated tokens against a {window}-token \
                 window. Drop files with /drop, or raise [model] context_window."
            ));
            messages.push(Message::system(format!(
                "CONTEXT OVERFLOW: this request is {assembled} estimated tokens against a \
                 {window}-token window. Earlier material may be missing. Work from what is \
                 here, narrow your reads, and do not assume you have seen the whole file."
            )));
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
                // A transport failure is not a user decision. Firing
                // `UserInterrupt` journaled `reason: "user interrupt"` for a
                // dropped connection and regressed Planning/Editing →
                // Exploring, discarding an approved plan the user still
                // wants. See `abandon_turn` for why the phase is kept —
                // and for the one phase where keeping it is not safe.
                self.reporter
                    .line(&format!("backend error: {err} — turn abandoned"));
                self.abandon_turn();
                return None;
            }
        };
        let mut text = String::new();
        let mut native: Vec<NativeCall> = Vec::new();
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    // Both halves of §6.2 cancellation: the flag stops the
                    // embedded inference thread between tokens, the drop ends
                    // the HTTP SSE pump.
                    self.tools.backend().stop();
                    drop(rx);
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
                        // As above: mid-stream transport loss is not a user
                        // interrupt and must not discard plan state.
                        self.reporter
                            .line(&format!("\nstream failed: {cause} — turn abandoned"));
                        self.abandon_turn();
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
        self.journal(Event::ToolCall {
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
        self.journal(Event::ToolResult {
            status: outcome.status,
            summary: outcome.content.clone(),
            truncated: outcome.truncated,
        });
        // §6.8/§6.10 audit: a dispatch also journals one `Dispatch` event per
        // sub-coder, carrying its full transcript. The registry hands these
        // over beside the observation — the transcripts must reach the
        // session log and must never reach main context, so they are taken
        // from the side channel rather than parsed back out of the rendered
        // reports. Failed runs are journaled too: a research thread that
        // produced nothing is exactly what an audit trail should record.
        if name == "dispatch" {
            for (label, report, transcript) in self.tools.take_dispatch_log() {
                self.journal(Event::Dispatch {
                    label,
                    report,
                    transcript: transcript
                        .into_iter()
                        .map(|(role, content)| rusta_core::TranscriptLine { role, content })
                        .collect(),
                });
            }
        }
        self.history.push(outcome.observation(name));
        self.card_cues.push(name.to_owned());
        if outcome.status == Status::Error {
            // §6.6 recovery cards trigger on error *kinds*, not on the bare
            // word "error"; `error_cues` is the single place observation
            // text becomes that vocabulary.
            self.card_cues
                .extend(rusta_core::error_cues(name, &outcome.content));
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
        // §6.1 step 5 covers the stream; a validation round can run for
        // `[validate] timeout_secs` *per command* (default 600 s), so it
        // needs the same escape hatch or the user cannot take control back.
        let outcome = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                self.reporter.line("\n^C aborted — validation interrupted");
                self.fire(PhaseEvent::UserInterrupt);
                self.gate.reset();
                return false;
            }
            outcome = self.config.validate.run(&self.root) => outcome,
        };
        let outcome = match outcome {
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
            self.journal(report.session_event());
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
                // The edits are still applied and committed (§6.9 commits
                // the batch before validating), so say plainly how to get
                // back to green rather than leaving a red tree unexplained.
                if let Some(batch) = self.batches.last() {
                    self.reporter.line(&format!(
                        "the last batch ({} file(s)) is still applied{} — /undo reverts it",
                        batch.paths.len(),
                        match &batch.sha {
                            Some(sha) => format!(" and committed as {}", &sha[..7.min(sha.len())]),
                            None => String::new(),
                        }
                    ));
                }
                false
            }
        }
    }

    /// Surfaces §6.3 rule 5 suggested commands to the user and tells the
    /// model they were *not* run, so it does not assume their effects.
    /// Running one is the user's decision: they can paste it, or ask for it
    /// through the approval-gated `shell` tool.
    fn surface_commands(&mut self, commands: &[String], tell_model: bool) {
        if commands.is_empty() {
            return;
        }
        for command in commands {
            self.reporter
                .line(&format!("! suggested (not run): {command}"));
        }
        if !tell_model {
            return;
        }
        self.push_observation(
            "shell suggestion",
            &format!(
                "These commands were shown to the user but NOT run:\n{}\nRun one with the shell tool if you need its output.",
                commands.join("\n")
            ),
            Status::Ok,
        );
    }

    /// Pushes a synthetic observation into history + session (recorded as a
    /// `ToolCall`/`ToolResult` pair so `/resume` replays it verbatim, §6.10).
    pub(crate) fn push_observation(&mut self, name: &str, content: &str, status: Status) {
        self.push_observation_with_input(
            name,
            Value::Object(serde_json::Map::new()),
            content,
            status,
        );
    }

    /// [`Self::push_observation`] with an explicit `ToolCall` input, for the
    /// synthetic observations whose replay needs their arguments — `/add`
    /// carries the paths it credited to the ledger.
    pub(crate) fn push_observation_with_input(
        &mut self,
        name: &str,
        input: Value,
        content: &str,
        status: Status,
    ) {
        self.journal(Event::ToolCall {
            name: name.to_owned(),
            input,
        });
        self.journal(Event::ToolResult {
            status,
            summary: content.to_owned(),
            truncated: false,
        });
        self.history
            .push(prompt::observation(name, status, content));
        self.card_cues.push(name.to_owned());
        if status == Status::Error {
            self.card_cues.extend(rusta_core::error_cues(name, content));
        }
    }

    /// A turn abandoned by transport failure, not by the user.
    ///
    /// The phase is kept — firing `UserInterrupt` for a dropped connection
    /// journaled a network fault as a user decision and discarded an
    /// approved plan. The one exception is `Verifying`: its only exits are
    /// the two validation verdicts and `UserInterrupt`, and `edit` is not
    /// registered there, so a kept phase would leave the model with no
    /// reachable move at all. An abandoned verification is not a passed one
    /// (§6.7 opens the gate only on green), so `ValidationFailed` is the
    /// honest verdict and returns to `Editing`, where work can continue.
    fn abandon_turn(&mut self) {
        if self.machine.state() == State::Verifying {
            self.fire(PhaseEvent::ValidationFailed);
        }
    }

    /// Fires a scaffold event on the §6.4 machine, journaling the
    /// `StateChange`. Illegal transitions are surfaced, never fatal.
    fn fire(&mut self, event: PhaseEvent) {
        match self.machine.fire(event) {
            Ok(Some(transition)) => {
                self.journal(Event::StateChange {
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
    /// regression plus the user notification.
    ///
    /// The regression *is* journaled, through the first-class
    /// [`PhaseEvent::LoopEscalated`] edge — an out-of-band re-seed made the
    /// next `StateChange` fail §6.10 replay validation, which left the
    /// session unresumable and the repo unusable for the day. The loop
    /// guard's own detector state stays unjournaled by design: `/resume`
    /// starts detectors fresh, which is the safe side.
    fn handle_trip(&mut self, trip: Trip) {
        if !trip.escalate {
            return;
        }
        if self.machine.state() == State::Editing {
            // `PhaseEvent::LoopEscalated` is a first-class scaffold event, so
            // `fire` journals the regression like any other transition. The
            // §6.10 replay validator checks every `StateChange` against the
            // table; an out-of-band re-seed made the log unreplayable and
            // therefore the session unresumable.
            let notice = self.guard.escalation().map(|e| e.notify);
            self.fire(PhaseEvent::LoopEscalated);
            self.guard.acknowledge_escalation();
            if let Some(notice) = notice {
                self.reporter.line(&format!("! {notice}"));
            }
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
        let Parsed { items, notes, .. } = parse_items(text, &[]);
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
        let Parsed { items, .. } = parse_items(text, &native);
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
        let Parsed { items, notes, .. } = parse_items(text, &[]);
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
}

#[cfg(test)]
mod audit_regressions {
    use super::*;

    /// F1: a ` ```tool ` fence inside a SEARCH/REPLACE body is file content,
    /// not a call.
    ///
    /// The splitter used to cut the prose segment at the fence, so the
    /// REPLACE payload was silently truncated to the text above it — the
    /// edit still reported success — and the fenced JSON was executed as a
    /// tool call. `rusta-edit` already asserted fences survive inside blocks,
    /// but against `parse_response`; nothing covered this seam, which is the
    /// production path. Any repo documenting Rusta's own tool format (this
    /// one included) contains such a fence.
    #[test]
    fn a_tool_fence_inside_an_edit_block_is_content_not_a_call() {
        let completion = "docs/tools.md\n\
             <<<<<<< SEARCH\n\
             TODO\n\
             =======\n\
             Call a tool like this:\n\
             \n\
             ```tool\n\
             {\"name\": \"shell\", \"input\": {\"command\": \"rm -rf /\"}}\n\
             ```\n\
             \n\
             That is the whole format.\n\
             >>>>>>> REPLACE\n";
        let parsed = parse_items(completion, &[]);

        assert!(
            !parsed
                .items
                .iter()
                .any(|item| matches!(item, Item::Call { .. })),
            "fenced content inside an edit body must never become a call: {:?}",
            parsed.items
        );
        let [Item::Blocks(blocks)] = parsed.items.as_slice() else {
            panic!("expected exactly one edit block, got {:?}", parsed.items);
        };
        assert_eq!(blocks.len(), 1);
        // The whole payload survives, fence and trailing prose included.
        assert_eq!(
            blocks[0].updated,
            "Call a tool like this:\n\n```tool\n{\"name\": \"shell\", \"input\": \
             {\"command\": \"rm -rf /\"}}\n```\n\nThat is the whole format.\n"
        );
    }

    /// F1 corollary: a genuine fence *outside* any block still executes, and
    /// still in document order relative to the edits around it.
    #[test]
    fn fences_outside_edit_blocks_still_execute_in_document_order() {
        let completion = "```tool\n{\"name\": \"read\", \"input\": {\"path\": \"a.rs\"}}\n```\n\
             a.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n\
             ```tool\n{\"name\": \"grep\", \"input\": {\"pattern\": \"x\"}}\n```\n";
        let parsed = parse_items(completion, &[]);
        let names: Vec<String> = parsed
            .items
            .iter()
            .map(|item| match item {
                Item::Call { name, .. } => name.clone(),
                Item::Blocks(_) => "<edit>".to_owned(),
            })
            .collect();
        assert_eq!(names, ["read", "<edit>", "grep"]);
    }

    /// F10: an ordinary answer that happens to carry a numbered list must not
    /// trip the §6.4 plan gate. `contains` matched "step" inside "footsteps".
    #[test]
    fn prose_answers_do_not_trip_the_plan_gate() {
        assert!(
            !is_plan("The parser footsteps through:\n1. lex\n2. parse\n3. apply\n"),
            "mid-word cue must not read as a drafted plan"
        );
        // Real plans, and ordinary inflections, still do.
        assert!(is_plan("Plan:\n1. edit lib.rs\n2. run tests\n"));
        assert!(is_plan("Here are the steps:\n1. edit lib.rs\n2. verify\n"));
        assert!(is_plan("I'll do this:\n1. patch the parser\n"));
    }
}
