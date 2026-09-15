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
use rusta_llm::Message;
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
        /// The full sub-transcript (§6.8: "goes to the session log only —
        /// never main context"). Carried here because the schema previously
        /// had nowhere to put it, so the clause was doc-only: the field was
        /// built, documented as session-log material, and dropped. Defaults
        /// to empty so logs written before this event shape still replay.
        #[serde(default)]
        transcript: Vec<TranscriptLine>,
    },
    /// A user request began — the boundary that separates one `/undo` batch
    /// from the next (§6.9).
    ///
    /// Batch boundaries used to be inferred from `Commit` events, so in
    /// no-git mode, or after a failed commit, consecutive batches from
    /// *separate* requests merged into one and a single `/undo` popped all of
    /// them. A boundary is a scaffold fact, not a git fact.
    BatchBoundary,
    /// An applied edit batch was undone (§6.9 `/undo`).
    ///
    /// `/undo` used to journal nothing, so replay resurrected entries for
    /// edits already undone: apply A→B, undo to A, edit A→C, restart, and the
    /// next `/undo` wrote B over C — silently rewinding the working tree to
    /// content the user had rejected. Replay honours this as a tombstone.
    UndoApplied {
        /// How many journal entries the undo consumed.
        entries: usize,
    },
    /// Session closed cleanly.
    SessionEnd,
}

/// One message of a sub-coder transcript, flattened for the session log
/// (§6.8). Deliberately not `rusta_llm::Message`: the log is a durable wire
/// format and must not move whenever that type does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptLine {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// The message text.
    pub content: String,
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

/// Restricts a newly created log to its owner (`0600`).
///
/// The log and its sidecar carry whole file contents, tool observations and
/// validator output — anything the agent read, including a `.env` it was
/// asked to look at. At the default umask those files land group- and
/// world-readable; smallcode sets `0600` on its session files for exactly
/// this data. A no-op on non-Unix, where the mode bit does not apply.
fn owner_only(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
}

/// Tightens an *existing* log to `0600`.
///
/// `OpenOptions::mode` applies only when the file is created, so a session
/// written before that guard existed — or under a looser umask — keeps its
/// permissions for the life of the file. Sessions are auto-resumed per repo
/// per day, so those are exactly the logs still in use. Best-effort: a log
/// whose mode cannot be read or set is left alone rather than failing the
/// session.
fn restrict_existing(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o177 != 0 {
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(path, perms);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
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
        let file =
            owner_only(OpenOptions::new().create(true).append(true).read(true)).open(&path)?;
        // A log created before the 0600 guard — or under a looser umask —
        // keeps its permissions for the life of the file, and sessions are
        // auto-resumed per repo per day, so those are exactly the logs still
        // in use. `OpenOptions::mode` only applies at creation.
        restrict_existing(&path);
        // Repair a torn tail before anything appends to it (see
        // `repair_torn_tail`): tolerating it on read is not enough.
        let sidecar_path = path.with_extension("diffs.jsonl");
        repair_torn_tail::<Event>(&path)?;
        repair_torn_tail::<DiffRecord>(&sidecar_path)?;
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
            self.sidecar =
                Some(owner_only(OpenOptions::new().create(true).append(true)).open(&sidecar_path)?);
            restrict_existing(&sidecar_path);
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
        let mut skipped_edits: Vec<String> = Vec::new();
        let mut pending_call: Option<(String, Option<String>, Vec<String>)> = None;

        for event in &self.events {
            match event {
                Event::SessionStart { .. }
                | Event::ValidationRun { .. }
                | Event::Commit { .. }
                | Event::BatchBoundary
                | Event::SessionEnd => {}
                // §6.9 `/undo` tombstone: the entries it consumed are gone
                // from the working tree, so replay must not resurrect them.
                Event::UndoApplied { entries } => {
                    let keep = undo_entries.len().saturating_sub(*entries);
                    undo_entries.truncate(keep);
                }
                Event::UserMessage { content } => messages.push(Message::user(content.clone())),
                Event::AssistantMessage { content } => {
                    messages.push(Message::assistant(content.clone()));
                }
                Event::ToolCall { name, input } => {
                    // The credit is deferred to the paired result: a read
                    // that *failed* never put the file in the model's
                    // context, so crediting it here would let a resumed
                    // session edit a file it has not actually seen.
                    let path = input
                        .get("path")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned);
                    // `/add` credits many files at once, so it carries a
                    // `paths` array rather than a single `path`.
                    let paths: Vec<String> = input
                        .get("paths")
                        .and_then(|value| value.as_array())
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    pending_call = Some((name.clone(), path, paths));
                }
                Event::ToolResult {
                    status,
                    summary,
                    truncated,
                } => {
                    // An orphan result (torn log) is skipped; a dangling
                    // call without a result never entered the live context
                    // as an observation either.
                    if let Some((name, path, paths)) = pending_call.take() {
                        // `/add` credits the ledger directly and journals
                        // its observation under this name, so replay must
                        // honour it too — otherwise the chat-set silently
                        // vanished across a restart while the model was
                        // still told about it.
                        if *status == Status::Ok
                            && matches!(name.as_str(), "read" | "map_drill" | "add")
                        {
                            if let Some(path) = &path {
                                ledger.record_read(Path::new(path));
                            }
                            for path in &paths {
                                ledger.record_read(Path::new(path));
                            }
                        }
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
                    // The sidecar records must be consumed in lockstep with
                    // the events whatever happens to the path, or every later
                    // edit replays against the wrong pair — so confinement is
                    // checked *after* reading, and only the journal entry is
                    // dropped. Logs written before the §6.12 fence existed
                    // legitimately carry absolute paths, and `/undo` writes
                    // and deletes through this entry.
                    match rusta_edit::confine(Path::new(path)) {
                        Some(rel) => undo_entries.push(UndoEntry {
                            path: rel,
                            existed: !before.created,
                            before: before.content,
                            after: after.content,
                        }),
                        None => skipped_edits.push(path.clone()),
                    }
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
                    // The same code path as live history compression (§6.6):
                    // replay reproduces the model-visible context
                    // byte-for-byte, keepers included.
                    crate::context::Compressor::apply_summary(&mut messages, *covers_turns, text);
                }
                // The report already replayed inside the `dispatch`
                // ToolResult observation above. Replaying it again here
                // duplicated every report in a resumed context, breaking the
                // byte-for-byte replay fidelity §6.10 exists to provide.
                // These events remain the §6.10 audit record of what each
                // sub-coder returned; they are not a second message source.
                Event::Dispatch { .. } => {}
            }
        }
        Ok(Reconstructed {
            messages,
            ledger,
            undo: UndoStack::from_entries(undo_entries),
            state: phase,
            skipped_edits,
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
    /// Paths from `EditApplied` events that escape the workspace and were
    /// therefore left out of the undo journal (§6.12). Non-empty only for
    /// logs written before the confinement fence, or hand-edited ones; the
    /// CLI reports them so the lost undo depth is visible rather than silent.
    pub skipped_edits: Vec<String>,
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
///
/// A torn final record is dropped for the same reason as in [`replay`] — and
/// more readily here, since each record carries a whole file's contents and
/// so spans many more bytes of non-atomic append. `next_record` then reports
/// the missing half as a hash mismatch with its own remedy.
fn load_sidecar(path: &Path) -> Result<Vec<DiffRecord>, Error> {
    let Ok(file) = File::open(path) else {
        return Ok(Vec::new());
    };
    let lines: Vec<String> = BufReader::new(file).lines().collect::<Result<_, _>>()?;
    let last = lines.len().saturating_sub(1);
    let mut records = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<DiffRecord>(line) {
            Ok(record) => records.push(record),
            Err(_) if index == last => break, // torn tail: the write never landed
            Err(e) => {
                return Err(Error::Corrupt {
                    path: path.display().to_string(),
                    cause: format!(
                        "sidecar line {}: {e}. Remedy: delete or repair that \
                         line; /undo depth is lost but the session still replays",
                        index + 1
                    ),
                });
            }
        }
    }
    Ok(records)
}

/// Parses an existing JSONL log, rejecting corrupt lines with a line-numbered
/// remedy (plan §6.11).
///
/// A torn **final** line is forgiven and dropped: `writeln!` + `flush` is not
/// atomic, so a crash, a SIGKILL, or ENOSPC mid-append leaves a partial
/// record, and the event it described never completed. Anything torn
/// *earlier* in the file is real corruption and still fails loudly — the
/// events after it would replay against the wrong state.
///
/// The distinction matters because a session is auto-resumed from
/// `~/.rusta/sessions/<slug>-<UTC date>.jsonl`: failing the whole log for a
/// half-written tail made `rusta` refuse to start in that repo until the date
/// rolled over.
/// Truncates `path` to the end of its last complete, parseable line,
/// returning the number of bytes discarded.
///
/// Forgiving a torn tail on *read* is not enough, and on its own is unsafe.
/// `record` appends through `O_APPEND`, so the next write fuses its JSON onto
/// the unterminated remnant. One such append is survivable — the fused line
/// is still last, so the read-side forgiveness covers it and exactly one
/// event is silently lost. The **second** append puts that fused line
/// mid-file, where forgiveness correctly does not apply, and the session
/// refuses to open for the rest of the repo-day: precisely the failure the
/// torn-write erratum was written to eliminate, re-created by the fix for it.
/// Repairing on open is what makes the forgiveness safe.
///
/// Only a torn *tail* is removed. A line that fails to parse anywhere else is
/// left untouched for [`replay`] to reject loudly, because the events after
/// it would replay against the wrong state.
fn repair_torn_tail<T: serde::de::DeserializeOwned>(path: &Path) -> Result<u64, Error> {
    let Ok(file) = File::open(path) else {
        return Ok(0); // nothing written yet
    };
    let total = file.metadata()?.len();
    let lines: Vec<String> = BufReader::new(file).lines().collect::<Result<_, _>>()?;
    let Some(bad) = lines
        .iter()
        .position(|line| !line.trim().is_empty() && serde_json::from_str::<T>(line).is_err())
    else {
        return Ok(0); // every line parses
    };
    // Only a torn *tail* is repairable. If anything follows the bad line, the
    // damage is mid-file: leave the bytes alone so `replay` rejects it loudly
    // rather than silently discarding the events after it.
    if bad + 1 != lines.len() {
        return Ok(0);
    }
    // Byte offset just past the last line that parsed. Every line is written
    // by `writeln!`, so each consumed line is exactly `len + 1` bytes.
    let valid: u64 = lines[..bad].iter().map(|l| l.len() as u64 + 1).sum();
    if valid >= total {
        return Ok(0);
    }
    OpenOptions::new().write(true).open(path)?.set_len(valid)?;
    Ok(total - valid)
}

fn replay(path: &Path) -> Result<Vec<Event>, Error> {
    let file = File::open(path)?;
    let lines: Vec<String> = BufReader::new(file).lines().collect::<Result<_, _>>()?;
    let last = lines.len().saturating_sub(1);
    let mut events = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(line) {
            Ok(event) => events.push(event),
            Err(_) if index == last => break, // torn tail: the write never landed
            Err(e) => {
                return Err(Error::Corrupt {
                    path: path.display().to_string(),
                    cause: format!(
                        "line {}: {e}. Remedy: the log is append-only JSON \
                         Lines — delete or repair that line, or move the file \
                         aside to start a fresh session",
                        index + 1
                    ),
                });
            }
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::Event::*;
    use super::*;
    use rusta_llm::Role;

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
        // Corruption *before* the end is real: the events after it would
        // replay against the wrong state, so the log must fail loudly.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corrupt.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user_message\",\"content\":\"ok\"}\nnot json at all\n{\"type\":\"session_end\"}\n",
        )
        .expect("write");
        let error = Session::open(&path).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("line 2"), "{text}");
        assert!(text.contains("Remedy:"), "§6.11 needs a remedy: {text}");
    }

    #[test]
    fn a_torn_final_line_is_dropped_not_fatal() {
        // `writeln!` + `flush` is not atomic: a crash, SIGKILL or ENOSPC
        // mid-append leaves a partial record for an event that never
        // completed. Sessions auto-resume per repo per UTC day, so failing
        // the whole log here made `rusta` refuse to start in that repo until
        // the date rolled over.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("torn.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user_message\",\"content\":\"hello\"}\n{\"type\":\"assistant_message\",\"cont",
        )
        .expect("write");

        let session = Session::open(&path).expect("a torn tail must still open");
        assert_eq!(
            session.events(),
            [Event::UserMessage {
                content: "hello".to_owned()
            }]
        );
        // And the surviving prefix still replays.
        assert_eq!(session.replay_context().expect("replays").messages.len(), 1);
    }

    #[test]
    fn a_torn_final_sidecar_record_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s.jsonl");
        {
            let mut session = Session::open(&path).expect("open");
            session
                .record_edit("a.rs", true, "before", "after")
                .expect("edit");
        }
        // Simulate a crash part-way through appending a third record.
        let sidecar = path.with_extension("diffs.jsonl");
        let mut text = std::fs::read_to_string(&sidecar).expect("read");
        text.push_str("{\"hash\":123,\"cont");
        std::fs::write(&sidecar, text).expect("write");

        let session = Session::open(&path).expect("open");
        let replayed = session
            .replay_context()
            .expect("torn sidecar tail is benign");
        assert_eq!(replayed.undo.pending().len(), 1);
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
    fn summary_event_replays_through_the_live_compression_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        {
            let mut session = Session::open(&path).expect("open");
            session
                .record(UserMessage {
                    content: "fix the bug".into(),
                })
                .expect("record");
            session
                .record(AssistantMessage {
                    content: "src/lib.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE"
                        .into(),
                })
                .expect("record");
            session
                .record(AssistantMessage {
                    content: "done thinking".into(),
                })
                .expect("record");
            session
                .record(Summary {
                    covers_turns: 1,
                    text: "1. user: fix the bug; edit src/lib.rs".into(),
                })
                .expect("record");
        }
        let session = Session::open(&path).expect("reopen");
        let context = session.replay_context().expect("replay");
        // The compressed turn's edit block survives verbatim, after the
        // episodic summary; the kept turn follows untouched.
        assert_eq!(context.messages.len(), 3);
        assert_eq!(
            context.messages[0].content,
            "Summary of earlier turns:\n1. user: fix the bug; edit src/lib.rs"
        );
        assert_eq!(context.messages[0].role, Role::Assistant);
        assert_eq!(
            context.messages[1].content,
            "src/lib.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE"
        );
        assert_eq!(context.messages[1].role, Role::Assistant);
        assert_eq!(context.messages[2].content, "done thinking");
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
