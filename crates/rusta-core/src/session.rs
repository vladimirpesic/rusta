//! Append-only JSONL session persistence — development plan §6.10.
//!
//! One JSON object per line, tagged `{"type": ...}` per the plan's event
//! schema. The log stays small: full before/after text of edits goes to the
//! `<slug>.diffs.jsonl` sidecar (M2), not here. Appends are flushed
//! immediately, so a crash loses at most the event in flight. Replaying the
//! log is the basis of `/resume`.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;

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
        from: String,
        /// New phase.
        to: String,
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

/// An append-only session log with in-memory replay.
#[derive(Debug)]
pub struct Session {
    path: PathBuf,
    file: File,
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
        Ok(Self { path, file, events })
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

    /// All events so far, in order.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The log file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
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
            from: "Exploring".into(),
            to: "Planning".into(),
            reason: "plan drafted".into(),
        })
        .expect("serialize");
        assert!(state.contains("\"type\":\"state_change\""), "{state}");
    }
}
