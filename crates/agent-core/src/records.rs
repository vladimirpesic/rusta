use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lm_bridge::message::Message;
use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// Current record-file schema version.
pub const CURRENT_RECORD_VERSION: u32 = 1;

/// A single event stored in the JSONL record file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum RecordEvent {
    /// A conversation message (user, assistant, tool, or system).
    Message(Message),
}

/// Envelope written to each JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordEnvelope {
    version: u32,
    event: RecordEvent,
}

/// The session state an agent keeps beside its records file, as
/// `<records file>.<kind>.json`: the todo list, goals, active skills, the
/// files it has read, and its scheduled prompts. A resume loads them; an
/// export carries them. A fork does not carry cron tasks — they schedule
/// prompts into this session, so a fork would double-fire the parent's
/// reminders.
pub const SESSION_SIDECARS: [&str; 5] = ["todos", "goals", "skills", "reads", "cron"];

/// The `kind` sidecar ([`SESSION_SIDECARS`]) of the records file `records`.
#[must_use]
pub fn sidecar_path(records: &Path, kind: &str) -> PathBuf {
    let name = records
        .file_name()
        .map_or_else(|| "records.jsonl".into(), |n| n.to_string_lossy());
    records.with_file_name(format!("{name}.{kind}.json"))
}

/// Append-only record store for conversation replay.
#[derive(Debug, Clone)]
pub struct Records {
    path: PathBuf,
    /// This process's view of the file, shared by its clones.
    view: Arc<Mutex<View>>,
}

/// Where a process's view of a records file begins — at its start for a
/// resumed session, at the end it had for a fresh one under the same file —
/// and how long the process last saw the file. A file longer than that was
/// written by another process (see [`crate::session_lock`]).
#[derive(Debug, Default, Clone, Copy)]
struct View {
    base: u64,
    len: u64,
}

impl Records {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            view: Arc::default(),
        }
    }

    /// Begin a fresh session's view where the file now ends: what came
    /// before it belongs to earlier sessions under this file.
    pub fn start_fresh(&self) {
        let len = self.file_len();
        *self.view.lock().unwrap_or_else(|e| e.into_inner()) = View { base: len, len };
    }

    /// Whether the file is not as long as this process last saw it: another
    /// process wrote the session since.
    #[must_use]
    pub fn appended_elsewhere(&self) -> bool {
        self.file_len() != self.view.lock().unwrap_or_else(|e| e.into_inner()).len
    }

    /// The messages of this process's view: those of the whole file for a
    /// resumed session, those since it began for a fresh one, another
    /// process's included.
    pub async fn load_view(&self) -> Result<Vec<Message>, crate::errors::AgentError> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(crate::errors::AgentError::Config(e.to_string())),
        };
        let mut view = *self.view.lock().unwrap_or_else(|e| e.into_inner());
        let base = usize::try_from(view.base)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let events = self.parse_events(&String::from_utf8_lossy(&bytes[base..]));
        view.len = bytes.len() as u64;
        *self.view.lock().unwrap_or_else(|e| e.into_inner()) = view;
        Ok(Self::messages(events))
    }

    fn file_len(&self) -> u64 {
        std::fs::metadata(&self.path).map_or(0, |meta| meta.len())
    }

    /// How many damaged lines are named individually before the log switches to
    /// a single summary. A truncated file can be damaged from some point to its
    /// end, and one warning per line would bury everything else.
    const MAX_DAMAGE_WARNINGS: usize = 5;

    /// Append a message to the record file using the current envelope format.
    pub async fn append(&self, message: &Message) -> Result<(), crate::errors::AgentError> {
        let envelope = RecordEnvelope {
            version: CURRENT_RECORD_VERSION,
            event: RecordEvent::Message(message.clone()),
        };
        let line = serde_json::to_string(&envelope)
            .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        file.write_all(b"\n")
            .await
            .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        // `tokio::fs::File` buffers internally and its documentation warns that
        // dropping one without flushing can lose the write. A record is the only
        // durable trace of a turn — a resumed session reads it back — so the last
        // thing written before the process exits is exactly what must not vanish.
        file.flush()
            .await
            .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        if let Ok(meta) = file.metadata().await {
            self.view.lock().unwrap_or_else(|e| e.into_inner()).len = meta.len();
        }
        Ok(())
    }

    /// Load all messages from disk, transparently migrating older formats.
    /// This process's view then covers the whole file.
    pub async fn load(&self) -> Result<Vec<Message>, crate::errors::AgentError> {
        Ok(Self::messages(self.load_events().await?))
    }

    fn messages(events: Vec<RecordEvent>) -> Vec<Message> {
        Self::migrate(events)
            .into_iter()
            .map(|e| match e {
                RecordEvent::Message(m) => m,
            })
            .collect()
    }

    /// Load raw events from disk.
    ///
    /// Reads the enveloped format, and plain-message JSONL (one message per
    /// line).
    pub async fn load_events(&self) -> Result<Vec<RecordEvent>, crate::errors::AgentError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let content = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        *self.view.lock().unwrap_or_else(|e| e.into_inner()) = View {
            base: 0,
            len: content.len() as u64,
        };
        Ok(self.parse_events(&content))
    }

    /// The events of the record lines `content` holds, a damaged line
    /// skipped with a warning.
    fn parse_events(&self, content: &str) -> Vec<RecordEvent> {
        let mut events = Vec::new();
        let mut damaged = 0usize;
        for (number, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match parse_record_line(line) {
                Ok(event) => events.push(event),
                // One unreadable line must not cost the whole session. A record
                // file is an append log: a process killed mid-write, a full
                // disk, or a hand-edit leaves exactly one damaged line, and
                // failing the load would throw away every intact turn around
                // it. Skipping is safe on the wire because the projector
                // synthesizes a placeholder for any tool call left unanswered
                // and drops any result left orphaned.
                Err(e) => {
                    damaged += 1;
                    if damaged <= Self::MAX_DAMAGE_WARNINGS {
                        tracing::warn!(
                            "skipping unreadable record at {}:{}: {e}",
                            self.path.display(),
                            number + 1
                        );
                    }
                }
            }
        }
        if damaged > 0 {
            tracing::warn!(
                "{damaged} unreadable record(s) skipped in {}; {} loaded",
                self.path.display(),
                events.len()
            );
        }
        events
    }

    /// Migrate a vector of loaded events to the current schema version.
    #[must_use]
    pub fn migrate(events: Vec<RecordEvent>) -> Vec<RecordEvent> {
        // Currently there is only one schema version, so the migration is a
        // no-op. Future versions can rewrite older events here.
        events
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn parse_record_line(line: &str) -> Result<RecordEvent, crate::errors::AgentError> {
    // Try the current enveloped format first.
    if let Ok(envelope) = serde_json::from_str::<RecordEnvelope>(line) {
        return Ok(envelope.event);
    }
    // Fall back to plain-message JSONL.
    let message: Message =
        serde_json::from_str(line).map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
    Ok(RecordEvent::Message(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write by another process is noticed, and the view reloads it.
    #[tokio::test]
    async fn another_processs_write_is_noticed_and_reloaded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let ours = Records::new(&path);
        ours.start_fresh();
        assert!(!ours.appended_elsewhere(), "an empty file is as seen");
        ours.append(&Message::user("ours")).await.unwrap();
        assert!(!ours.appended_elsewhere(), "our own write is seen");

        Records::new(&path)
            .append(&Message::assistant_text("theirs"))
            .await
            .unwrap();
        assert!(ours.appended_elsewhere());
        let view = ours.load_view().await.unwrap();
        let texts: Vec<String> = view.iter().map(Message::text_content).collect();
        assert_eq!(texts, ["ours", "theirs"]);
        assert!(!ours.appended_elsewhere(), "the reload is the new view");
        // The clone the context keeps shares the view.
        assert!(!ours.clone().appended_elsewhere());
    }

    /// A fresh session's view begins where the file ended; a loaded one
    /// covers the whole file.
    #[tokio::test]
    async fn a_view_begins_where_its_session_does() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let earlier = Records::new(&path);
        earlier
            .append(&Message::user("an earlier session"))
            .await
            .unwrap();

        let fresh = Records::new(&path);
        fresh.start_fresh();
        earlier
            .append(&Message::user("another process"))
            .await
            .unwrap();
        let view = fresh.load_view().await.unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].text_content(), "another process");

        let resumed = Records::new(&path);
        assert_eq!(resumed.load().await.unwrap().len(), 2);
        assert!(!resumed.appended_elsewhere());
        assert_eq!(resumed.load_view().await.unwrap().len(), 2);
    }
    use lm_bridge::message::Message;

    #[tokio::test]
    async fn test_append_and_load_writes_envelope() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path().join("records.jsonl"));

        records.append(&Message::user("hello")).await.unwrap();
        records
            .append(&Message::assistant_text("hi there"))
            .await
            .unwrap();

        let loaded = records.load().await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text_content(), "hello");
        assert_eq!(loaded[1].text_content(), "hi there");

        let raw = tokio::fs::read_to_string(records.path()).await.unwrap();
        for line in raw.lines() {
            let envelope: RecordEnvelope = serde_json::from_str(line).unwrap();
            assert_eq!(envelope.version, CURRENT_RECORD_VERSION);
        }
    }

    #[tokio::test]
    async fn test_load_legacy_plain_messages() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let legacy = [
            serde_json::to_string(&Message::user("legacy user")).unwrap(),
            serde_json::to_string(&Message::assistant_text("legacy assistant")).unwrap(),
        ]
        .join("\n");
        tokio::fs::write(&path, legacy).await.unwrap();

        let records = Records::new(path);
        let loaded = records.load().await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text_content(), "legacy user");
        assert_eq!(loaded[1].text_content(), "legacy assistant");
    }

    #[tokio::test]
    async fn test_migrate_is_identity_for_current_events() {
        let events = vec![
            RecordEvent::Message(Message::user("hello")),
            RecordEvent::Message(Message::assistant_text("world")),
        ];
        let migrated = Records::migrate(events.clone());
        assert_eq!(migrated.len(), 2);
    }

    /// A record file is an append log: a process killed mid-write, a full disk,
    /// or a hand-edit damages one line. Failing the whole load would throw away
    /// every intact turn around it — the session becomes unresumable because of
    /// one bad byte.
    #[tokio::test]
    async fn a_damaged_line_does_not_cost_the_whole_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let records = Records::new(path.clone());
        records.append(&Message::user("first")).await.unwrap();
        records
            .append(&Message::assistant_text("second"))
            .await
            .unwrap();

        // Simulate a torn write: append a truncated line, then a good one.
        let mut raw = tokio::fs::read_to_string(&path).await.unwrap();
        raw.push_str("{\"version\":1,\"event\":{\"type\":\"Mess\n");
        tokio::fs::write(&path, raw).await.unwrap();
        records.append(&Message::user("third")).await.unwrap();

        let loaded = records
            .load()
            .await
            .expect("a damaged line must not fail the load");
        let texts: Vec<String> = loaded.iter().map(|m| m.text_content()).collect();
        assert_eq!(
            texts,
            vec!["first", "second", "third"],
            "every intact record must survive the damaged one"
        );
    }

    /// The last line is the one a killed process truncates.
    #[tokio::test]
    async fn a_truncated_final_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let records = Records::new(path.clone());
        records.append(&Message::user("kept")).await.unwrap();
        let mut raw = tokio::fs::read_to_string(&path).await.unwrap();
        raw.push_str("{\"version\":1,\"eve");
        tokio::fs::write(&path, raw).await.unwrap();

        let loaded = records.load().await.expect("must load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text_content(), "kept");
    }
}
