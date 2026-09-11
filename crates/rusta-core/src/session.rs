//! Append-only JSONL session persistence — development plan §6.10.
//!
//! One JSON object per line, tagged `{"type": ...}` per the plan's event
//! schema. The log stays small: full before/after text of edits goes to the
//! `<stem>.diffs.jsonl` sidecar (two records per edit, keyed by FNV-1a
//! hash), not here. Appends are flushed immediately, so a crash loses at
//! most the event in flight. Replaying the log is the basis of `/resume`:
//! [`Session::replay_context`] reconstructs the message list, the
//! read-before-edit ledger, the undo journal, and the phase machine.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use rusta_edit::{Ledger, UndoEntry, UndoStack};
use rusta_llm::{Message, Role};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::prompt;
use crate::state::{self, State};

/// Outcome of a tool execution (plan §6.10 `ToolResult.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The tool succeeded (possibly with truncation).
    Ok,
    /// The tool failed; the summary carries a remedy (plan §6.11).
    Error,
}

/// One session event — plan §6.10 schema, verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Session boot: rendered config summary and backend description.
    SessionStart {
        /// Human-readable configuration summary.
        config: String,
        /// Backend kind and endpoint (no secrets).
        backend: String,
    },
    /// A user message.
    UserMessage {
        /// Message text.
        content: String,
    },
    /// A completed assistant turn (text only; tool calls are logged separately).
    AssistantMessage {
        /// Message text.
        content: String,
    },
    /// The model invoked a tool.
    ToolCall {
        /// Tool name.
        name: String,
        /// Tool input as JSON.
        input: serde_json::Value,
    },
    /// A tool finished.
    ToolResult {
        /// Outcome.
        status: Status,
        /// Capped summary for context reconstruction.
        summary: String,
        /// Whether the summary was truncated.
        truncated: bool,
    },
    /// An edit batch was applied and journaled.
    EditApplied {
        /// File path, `/`-separated.
        path: String,
        /// Content hash before the edit.
        before_hash: u64,
        /// Content hash after the edit.
        after_hash: u64,
    },
    /// One validator ran.
    ValidationRun {
        /// The command line.
        command: String,
        /// Process exit code (negative ⇒ killed by signal, Unix convention).
        exit: i32,
        /// Capped output summary.
        summary: String,
    },
    /// The phase machine transitioned.
    StateChange {
        /// Previous phase.
        from: State,
        /// New phase.
        to: State,
        /// Why.
        reason: String,
    },
    /// History compression produced an episodic summary.
    Summary {
        /// How many turns the summary covers.
        covers_turns: u32,
        /// Summary text.
        text: String,
    },
    /// The agent committed to git.
    Commit {
        /// Commit SHA.
        sha: String,
        /// Commit message.
        message: String,
    },
    /// A sub-coder returned a report.
    Dispatch {
        /// Research task label.
        label: String,
        /// The ≤400-token labeled report.
        report: String,
    },
    /// Session closed cleanly.
    SessionEnd,
}

/// One sidecar record: full file content keyed by its FNV-1a hash (plan
/// §6.10 "full before/after text of edits lives in a sidecar, keyed by
/// hash"). Exactly two records are appended per `EditApplied` event, in
/// lockstep — replay consumes them in order, so identical contents in
/// different edits can never be confused.
#[derive(Debug, Serialize, Deserialize)]
struct DiffRecord {
    /// FNV-1a 64 of `content` — matches the `EditApplied` event hashes.
    hash: u64,
    /// Full file content before or after the edit.
    content: String,
    /// True only on the *before* record of an edit that created the file:
    /// undoing a created file deletes it instead of restoring content.
    created: bool,
}

/// FNV-1a 64-bit. Small, forever-stable content hashing for the session
/// journal: logs must replay identically across Rusta versions, so std's
/// `DefaultHasher` (stability not guaranteed across releases) is
/// deliberately not used.
fn fnv1a64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// An append-only session log with in-memory replay.
#[derive(Debug)]
pub struct Session {
    path: PathBuf,
    file: File,
    /// Lazily opened handle to the diffs sidecar (created on first edit).
    sidecar: Option<File>,
    events: Vec<Event>,
}

impl Session {
    /// Opens (creating if needed) the JSONL log at `path` and replays any
    /// existing events — the basis of `/resume` (plan §6.10).
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let events = replay(&path)?;
        Ok(Self {
            path,
            file,
            sidecar: None,
            events,
        })
    }

    /// Appends one event to the log, flushed immediately.
    pub fn record(&mut self, event: Event) -> Result<(), Error> {
        let line = serde_json::to_string(&event).map_err(|e| Error::Corrupt {
            path: self.path.display().to_string(),
            cause: e.to_string(),
        })?;
        writeln!(self.file, "{line}")?;
        self.file.flush()?;
        self.events.push(event);
        Ok(())
    }

    /// Records an applied edit: two sidecar records (before/after content
    /// keyed by hash) followed by the `EditApplied` event. Sidecar first,
    /// so a torn crash between the two writes leaves an unconsumed sidecar
    /// tail — harmless — rather than an unrestorable event.
    pub fn record_edit(
        &mut self,
        path: &str,
        existed: bool,
        before: &str,
        after: &str,
    ) -> Result<(), Error> {
        let sidecar_path = self.sidecar_path();
        if self.sidecar.is_none() {
            self.sidecar = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&sidecar_path)?,
            );
        }
        let sidecar = self.sidecar.as_mut().expect("sidecar just opened");
        for record in [
            DiffRecord {
                hash: fnv1a64(before),
                content: before.to_owned(),
                created: !existed,
            },
            DiffRecord {
                hash: fnv1a64(after),
                content: after.to_owned(),
                created: false,
            },
        ] {
            let line = serde_json::to_string(&record).map_err(|e| Error::Corrupt {
                path: sidecar_path.display().to_string(),
                cause: e.to_string(),
            })?;
            writeln!(sidecar, "{line}")?;
        }
        sidecar.flush()?;
        self.record(Event::EditApplied {
            path: path.to_owned(),
            before_hash: fnv1a64(before),
            after_hash: fnv1a64(after),
        })
    }

    /// Path of the diffs sidecar: the log path with a `.diffs.jsonl`
    /// extension in the same directory.
    pub fn sidecar_path(&self) -> PathBuf {
        self.path.with_extension("diffs.jsonl")
    }

    /// All events so far, in order.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The log file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reconstructs the live session state from the recorded events — the
    /// `/resume` path (plan §6.10): the message history as the model last
    /// saw it, the read-before-edit ledger, the undo journal (rebuilt from
    /// the sidecar), and the final phase.
    pub fn replay_context(&self) -> Result<Reconstructed, Error> {
        let mut records = load_sidecar(&self.sidecar_path())?.into_iter();
        let mut messages: Vec<Message> = Vec::new();
        let mut ledger = Ledger::new();
        let mut undo_entries: Vec<UndoEntry> = Vec::new();
        let mut phase = State::Exploring;
        let mut pending_call: Option<String> = None;

        for event in &self.events {
            match event {
                Event::SessionStart { .. }
                | Event::ValidationRun { .. }
                | Event::Commit { .. }
                | Event::SessionEnd => {}
                Event::UserMessage { content } => messages.push(Message::user(content.clone())),
                Event::AssistantMessage { content } => {
                    messages.push(Message::assistant(content.clone()));
                }
                Event::ToolCall { name, input } => {
                    if matches!(name.as_str(), "read" | "map_drill") {
                        if let Some(path) = input.get("path").and_then(|value| value.as_str()) {
                            ledger.record_read(Path::new(path));
                        }
                    }
                    pending_call = Some(name.clone());
                }
                Event::ToolResult {
                    status,
                    summary,
                    truncated,
                } => {
                    // An orphan result (torn log) is skipped; a dangling
                    // call without a result never entered the live context
                    // as an observation either.
                    if let Some(name) = pending_call.take() {
                        let mut content = summary.clone();
                        if *truncated {
                            content.push_str("\n… [truncated]");
                        }
                        messages.push(prompt::observation(&name, *status, &content));
                    }
                }
                Event::EditApplied {
                    path,
                    before_hash,
                    after_hash,
                } => {
                    // The editor auto-injects reads for edited files, so the
                    // read-set survives resume.
                    ledger.record_read(Path::new(path));
                    let before = next_record(&mut records, &self.sidecar_path(), *before_hash)?;
                    let after = next_record(&mut records, &self.sidecar_path(), *after_hash)?;
                    undo_entries.push(UndoEntry {
                        path: PathBuf::from(path),
                        existed: !before.created,
                        before: before.content,
                        after: after.content,
                    });
                }
                Event::StateChange { from, to, .. } => {
                    let legal = state::PHASE_EVENTS.iter().any(|event| {
                        matches!(
                            state::pure_transition(*from, *event),
                            Ok(Some(transition)) if transition.to == *to
                        )
                    });
                    if *from != phase || !legal {
                        return Err(Error::Corrupt {
                            path: self.path.display().to_string(),
                            cause: format!(
                                "illegal state_change {from} -> {to} after {} events (machine was in {phase})",
                                self.events.len()
                            ),
                        });
                    }
                    phase = *to;
                }
                Event::Summary { covers_turns, text } => {
                    drop_oldest_turns(&mut messages, *covers_turns);
                    messages.push(Message::assistant(format!(
                        "Summary of earlier turns:\n{text}"
                    )));
                }
                Event::Dispatch { label, report } => {
                    messages.push(Message::user(format!(
                        "SUB-CODER \"{label}\" REPORT:\n{report}"
                    )));
                }
            }
        }
        Ok(Reconstructed {
            messages,
            ledger,
            undo: UndoStack::from_entries(undo_entries),
            state: phase,
        })
    }
}

/// Everything `/resume` rebuilds from a recorded session (plan §6.10).
#[derive(Debug, PartialEq)]
pub struct Reconstructed {
    /// Message history as the model last saw it.
    pub messages: Vec<Message>,
    /// The read-before-edit ledger.
    pub ledger: Ledger,
    /// The undo journal, restored from the sidecar.
    pub undo: UndoStack,
    /// The final phase; feed to [`crate::state::Machine::resume_at`].
    pub state: State,
}

/// Consumes the next sidecar record, verifying its hash matches the one
/// recorded in the event. A missing or mismatched record means the journal
/// cannot be restored exactly — surfaced as a corrupt-log error with the
/// remedy in the message (§6.11).
fn next_record<I>(records: &mut I, sidecar: &Path, expected: u64) -> Result<DiffRecord, Error>
where
    I: Iterator<Item = DiffRecord>,
{
    let record = records.next().ok_or_else(|| Error::Corrupt {
        path: sidecar.display().to_string(),
        cause: format!(
            "no diff record left for hash {expected}: restore the sidecar or drop the trailing edit events from the session log"
        ),
    })?;
    if record.hash != expected {
        return Err(Error::Corrupt {
            path: sidecar.display().to_string(),
            cause: format!(
                "diff record hash {} does not match event hash {expected}: the sidecar and the session log are out of sync",
                record.hash
            ),
        });
    }
    Ok(record)
}

/// Loads the sidecar; a missing file simply means no edits were recorded.
fn load_sidecar(path: &Path) -> Result<Vec<DiffRecord>, Error> {
    let Ok(file) = File::open(path) else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue; // tolerate a torn final line after a crash
        }
        let record: DiffRecord = serde_json::from_str(&line).map_err(|e| Error::Corrupt {
            path: path.display().to_string(),
            cause: format!("sidecar line {}: {e}", index + 1),
        })?;
        records.push(record);
    }
    Ok(records)
}

/// Drops the oldest `turns` conversation turns (a turn = one user message
/// plus everything up to the next user message) — replaying a `Summary`
/// event reproduces the live context, where those turns were compressed
/// away. Fewer turns than requested drops everything.
fn drop_oldest_turns(messages: &mut Vec<Message>, turns: u32) {
    if turns == 0 {
        return;
    }
    let mut seen = 0usize;
    let mut split = messages.len();
    for (index, message) in messages.iter().enumerate() {
        if message.role == Role::User {
            seen += 1;
            if seen > turns as usize {
                split = index;
                break;
            }
        }
    }
    messages.drain(..split);
}

/// Parses an existing JSONL log, rejecting corrupt lines with a line-numbered
/// remedy (plan §6.11).
fn replay(path: &Path) -> Result<Vec<Event>, Error> {
    let file = File::open(path)?;
    let mut events = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue; // tolerate a torn final line after a crash
        }
        let event: Event = serde_json::from_str(&line).map_err(|e| Error::Corrupt {
            path: path.display().to_string(),
            cause: format!("line {}: {e}", index + 1),
        })?;
        events.push(event);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::Event::*;
    use super::*;

    #[test]
    fn records_and_replays_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        {
            let mut session = Session::open(&path).expect("open");
            session
                .record(SessionStart {
                    config: "auto_approve=false".into(),
                    backend: "http http://127.0.0.1:8080/v1".into(),
                })
                .expect("record");
            session
                .record(UserMessage {
                    content: "fix the bug".into(),
                })
                .expect("record");
            session
                .record(ToolCall {
                    name: "read".into(),
                    input: serde_json::json!({ "path": "src/main.rs" }),
                })
                .expect("record");
            session
                .record(ToolResult {
                    status: Status::Ok,
                    summary: "42 lines".into(),
                    truncated: false,
                })
                .expect("record");
            session.record(SessionEnd).expect("record");
        }
        let reopened = Session::open(&path).expect("reopen");
        assert_eq!(reopened.events().len(), 5);
        assert_eq!(
            reopened.events()[2],
            ToolCall {
                name: "read".into(),
                input: serde_json::json!({ "path": "src/main.rs" }),
            }
        );
    }

    #[test]
    fn rejects_corrupt_lines_with_line_numbered_remedy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corrupt.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user_message\",\"content\":\"ok\"}\nnot json at all\n",
        )
        .expect("write");
        let error = Session::open(&path).unwrap_err();
        assert!(error.to_string().contains("line 2"), "{error}");
    }

    #[test]
    fn wire_format_is_tagged_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionEnd).expect("serialize"),
            "{\"type\":\"session_end\"}"
        );
        let state = serde_json::to_string(&StateChange {
            from: State::Exploring,
            to: State::Planning,
            reason: "plan drafted".into(),
        })
        .expect("serialize");
        assert_eq!(
            state,
            "{\"type\":\"state_change\",\"from\":\"exploring\",\"to\":\"planning\",\"reason\":\"plan drafted\"}"
        );
    }

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        // Reference test vectors from the FNV-1a 64 specification.
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn drop_oldest_turns_drops_user_anchored_turns() {
        let messages = |parts: &[(Role, &str)]| -> Vec<Message> {
            parts
                .iter()
                .map(|&(role, content)| Message::new(role, content))
                .collect()
        };
        let mut turns = messages(&[
            (Role::User, "t1"),
            (Role::Assistant, "a1"),
            (Role::User, "t2"),
            (Role::Assistant, "a2"),
            (Role::User, "t3"),
        ]);
        drop_oldest_turns(&mut turns, 1);
        assert_eq!(
            turns,
            messages(&[
                (Role::User, "t2"),
                (Role::Assistant, "a2"),
                (Role::User, "t3"),
            ])
        );

        // More turns than exist drops everything.
        let mut turns = messages(&[(Role::User, "t1"), (Role::Assistant, "a1")]);
        drop_oldest_turns(&mut turns, 5);
        assert!(turns.is_empty());

        // Zero is a no-op.
        let mut turns = messages(&[(Role::User, "t1")]);
        drop_oldest_turns(&mut turns, 0);
        assert_eq!(turns.len(), 1);
    }

    #[test]
    fn record_edit_writes_event_and_two_sidecar_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("proj.jsonl");
        let mut session = Session::open(&path).expect("open");
        session
            .record_edit("src/lib.rs", true, "old\n", "new\n")
            .expect("record edit");

        assert_eq!(session.events().len(), 1);
        assert_eq!(session.sidecar_path(), dir.path().join("proj.diffs.jsonl"));
        let sidecar = std::fs::read_to_string(session.sidecar_path()).expect("sidecar");
        let lines: Vec<&str> = sidecar.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains(&fnv1a64("old\n").to_string()),
            "{sidecar}"
        );
        assert!(lines[1].contains("\"created\":false"), "{sidecar}");
        assert!(lines[0].contains("\"created\":false"), "{sidecar}");

        // A created file flags the before record.
        session
            .record_edit("src/new.rs", false, "", "fresh\n")
            .expect("record edit");
        let sidecar = std::fs::read_to_string(session.sidecar_path()).expect("sidecar");
        let lines: Vec<&str> = sidecar.lines().collect();
        assert!(lines[2].contains("\"created\":true"), "{sidecar}");
        assert!(lines[3].contains("\"created\":false"), "{sidecar}");
    }

    /// Plan §8 M3 acceptance: a recorded session replays into the full
    /// `/resume` context — messages (with observations), the read ledger,
    /// a working undo journal restored from the sidecar, and the final
    /// phase.
    #[test]
    fn resume_replays_messages_ledger_undo_and_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");

        {
            let mut session = Session::open(&path).expect("open");
            session
                .record(SessionStart {
                    config: "auto_approve=false".into(),
                    backend: "http http://127.0.0.1:8080/v1".into(),
                })
                .expect("record");
            session
                .record(UserMessage {
                    content: "fix the bug in src/lib.rs".into(),
                })
                .expect("record");
            session
                .record(ToolCall {
                    name: "read".into(),
                    input: serde_json::json!({ "path": "src/lib.rs" }),
                })
                .expect("record");
            session
                .record(ToolResult {
                    status: Status::Ok,
                    summary: "12 lines".into(),
                    truncated: true,
                })
                .expect("record");
            session
                .record(StateChange {
                    from: State::Exploring,
                    to: State::Planning,
                    reason: "plan drafted".into(),
                })
                .expect("record");
            session
                .record(AssistantMessage {
                    content: "Plan: replace old() with new().".into(),
                })
                .expect("record");
            session
                .record(StateChange {
                    from: State::Planning,
                    to: State::Editing,
                    reason: "plan approved".into(),
                })
                .expect("record");
            session
                .record_edit("src/lib.rs", true, "fn old() {}\n", "fn new() {}\n")
                .expect("record edit");
            session
                .record(StateChange {
                    from: State::Editing,
                    to: State::Verifying,
                    reason: "edit batch applied".into(),
                })
                .expect("record");
            assert_eq!(session.events().len(), 9);
        }

        // `/resume`: a fresh open, exactly what happens after a restart.
        let session = Session::open(&path).expect("reopen");
        let context = session.replay_context().expect("replay");

        // Messages: user request, tool observation, assistant plan.
        assert_eq!(context.messages.len(), 3);
        assert_eq!(context.messages[0].role, Role::User);
        assert_eq!(context.messages[0].content, "fix the bug in src/lib.rs");
        assert_eq!(context.messages[1].role, Role::User);
        assert!(
            context.messages[1]
                .content
                .starts_with("TOOL RESULT read (ok)\n12 lines\n… [truncated]"),
            "{}",
            context.messages[1].content
        );
        assert_eq!(context.messages[2].role, Role::Assistant);
        assert_eq!(
            context.messages[2].content,
            "Plan: replace old() with new()."
        );

        // Ledger: the read and the edit both register.
        assert!(context.ledger.has_read(Path::new("src/lib.rs")));
        assert_eq!(context.ledger.len(), 1);

        // Undo journal: one entry rebuilt from the sidecar, and it works —
        // undoing restores the exact before-content on disk.
        std::fs::create_dir_all(dir.path().join("src")).expect("create src");
        std::fs::write(dir.path().join("src/lib.rs"), "fn new() {}\n").expect("write after");
        let mut undo = context.undo;
        assert_eq!(undo.pending().len(), 1);
        let undone = undo.undo_last(dir.path()).expect("undo").expect("entry");
        assert_eq!(undone.before, "fn old() {}\n");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read"),
            "fn old() {}\n"
        );
        assert!(undo.is_empty());

        // Final phase: the machine resumes in Verifying (Shell, no Edit).
        assert_eq!(context.state, State::Verifying);
        let machine = crate::state::Machine::resume_at(context.state);
        assert!(machine.tools().contains(&crate::state::Tool::Shell));
        assert!(!machine.tools().contains(&crate::state::Tool::Edit));
    }

    #[test]
    fn replay_rejects_state_changes_the_machine_would_not_fire() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        {
            let mut session = Session::open(&path).expect("open");
            // Planning → Editing without the PlanDrafted that reaches Planning.
            session
                .record(StateChange {
                    from: State::Planning,
                    to: State::Editing,
                    reason: "tampered".into(),
                })
                .expect("record");
        }
        let session = Session::open(&path).expect("reopen");
        let error = session.replay_context().unwrap_err();
        assert!(
            error.to_string().contains("illegal state_change"),
            "{error}"
        );
        assert!(error.to_string().contains("Exploring"), "{error}");
    }

    #[test]
    fn replay_of_edit_without_sidecar_record_is_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        {
            let mut session = Session::open(&path).expect("open");
            session
                .record_edit("src/lib.rs", true, "a\n", "b\n")
                .expect("record edit");
        }
        // The sidecar is lost — the edit cannot be restored exactly.
        std::fs::remove_file(path.with_extension("diffs.jsonl")).expect("remove sidecar");
        let session = Session::open(&path).expect("reopen");
        let error = session.replay_context().unwrap_err();
        assert!(error.to_string().contains("no diff record"), "{error}");
    }
}
