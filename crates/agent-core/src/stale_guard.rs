//! Refuse to overwrite files the agent has not read, or that changed since it
//! did.
//!
//! A `write_file` replaces a file wholesale, from context that may be many
//! turns stale: if the file changed after the model last saw it — a human
//! edited it, a formatter ran, a parallel agent wrote it — that change is lost.
//! So each agent records what it has read, and `write_file` checks that record
//! first.
//!
//! The guard covers `write_file` and not `edit_file`: an edit's `old_string`
//! must match the file as it is, exactly once, so the edit itself shows the
//! model knows the text it changes. Guarding edits too would refuse every
//! edit of an agent that read the file with `cat` — a round trip each, where
//! nothing is at risk. An edit re-stamps a file the agent read and that has
//! not changed since; of any other the stamp does not match, so an overwrite
//! still needs a read.
//!
//! The guard is deliberately **per-agent**. A sub-agent starts with an empty
//! record and must read a file itself before writing it: the whole reason it
//! runs in an isolated context is that it does not share the parent's view of
//! the workspace, and inheriting the parent's reads would let it act on exactly
//! the stale picture this guard exists to catch.
//!
//! The record belongs to the conversation, not the process: an agent with a
//! records file saves it beside them, and a resumed session loads it. Without
//! that, a resumed session would refuse every overwrite of a file it read
//! before the resume — its history shows the file, but the guard has never
//! seen it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::AgentError;

/// Above this size the content digest is skipped and the stamp falls back to
/// length and modification time alone.
///
/// A deliberate trade. Digesting means reading the whole file on a write check,
/// which is fine for the source files an agent actually edits and wrong for a
/// multi-megabyte artefact. Past this point the residual blind spot — a
/// same-length rewrite inside one mtime tick — is a price worth paying to keep a
/// write from re-reading 50 MB.
pub const DIGEST_SIZE_LIMIT: u64 = 8 * 1024 * 1024;

/// What a file looked like when the agent last saw it.
///
/// Three signals, because the cheap two are not sufficient on their own. Length
/// misses an equal-length edit; `mtime` misses a rewrite inside one clock tick,
/// which is not exotic — Windows resolves mtime to about 15 ms, and some network
/// filesystems to a whole second. Together they still miss the intersection: an
/// equal-length edit within one tick. That is precisely the shape of a formatter
/// reflowing a line, so the content digest closes it.
///
/// The digest is not a security property. It is a 64-bit FNV-1a hash used only
/// to answer "is this the same file I read". It is FNV-1a rather than std's
/// `DefaultHasher`, whose algorithm may change between Rust releases, because
/// stamps are saved with the session and compared by a later process — possibly
/// a later build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
    /// `None` for a file past [`DIGEST_SIZE_LIMIT`]. Two stamps that both
    /// skipped the digest still compare on length and `mtime`.
    digest: Option<u64>,
}

impl FileStamp {
    /// Stamp a file whose contents the caller already holds.
    ///
    /// The tools that read a file before writing it use this, so the digest
    /// costs nothing beyond hashing bytes already in memory.
    #[must_use]
    pub fn from_parts(metadata: &std::fs::Metadata, contents: Option<&[u8]>) -> Self {
        let len = metadata.len();
        Self {
            modified: metadata.modified().ok(),
            len,
            digest: contents
                .filter(|_| len <= DIGEST_SIZE_LIMIT)
                .map(Self::digest_of),
        }
    }

    /// 64-bit FNV-1a: fixed, so a saved stamp still compares in a later build.
    fn digest_of(bytes: &[u8]) -> u64 {
        const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        bytes.iter().fold(OFFSET_BASIS, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        })
    }

    /// Stamp the file at `path`, if it exists, reading it to digest it.
    ///
    /// Skips the read entirely for a file past [`DIGEST_SIZE_LIMIT`], and falls
    /// back to length and `mtime` if the read fails — an unreadable file must
    /// not make the guard refuse everything.
    pub async fn of(path: &Path) -> Option<Self> {
        let metadata = tokio::fs::metadata(path).await.ok()?;
        let contents = if metadata.len() <= DIGEST_SIZE_LIMIT {
            tokio::fs::read(path).await.ok()
        } else {
            None
        };
        Some(Self::from_parts(&metadata, contents.as_deref()))
    }
}

/// Why a write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    /// The agent has never read this file in this session.
    NeverRead,
    /// The file changed on disk after the agent read it.
    ChangedSinceRead,
}

impl Staleness {
    /// The message handed to the model.
    ///
    /// Both name the remedy, because a refusal the model cannot act on just
    /// becomes a retry of the same call.
    #[must_use]
    pub fn message(self, path: &Path) -> String {
        let path = path.display();
        match self {
            Self::NeverRead => format!(
                "Refusing to write {path}: this file has not been read with read_file in this \
                 session, so its current contents are unknown; what a shell command printed \
                 does not count. Read it with read_file, then retry."
            ),
            Self::ChangedSinceRead => format!(
                "Refusing to write {path}: the file changed on disk after it was read, so the \
                 content this edit is based on is out of date. Read it again with read_file, \
                 then retry."
            ),
        }
    }
}

/// Per-agent record of the files that have been read, and how they looked.
#[derive(Debug, Default)]
pub struct StaleGuard {
    seen: HashMap<PathBuf, FileStamp>,
    /// Where [`StaleGuard::persist`] saves the record; `None` keeps it in
    /// memory, as for a sub-agent.
    persist_path: Option<PathBuf>,
}

/// The version of the saved form; a file of another version is not loaded.
const SAVED_VERSION: u32 = 1;

/// The saved form of a [`StaleGuard`].
#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    files: Vec<SavedFile>,
}

#[derive(Serialize, Deserialize)]
struct SavedFile {
    path: PathBuf,
    #[serde(flatten)]
    stamp: FileStamp,
}

impl StaleGuard {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty record that [`StaleGuard::persist`] saves to `path`.
    #[must_use]
    pub fn saved_to(path: impl Into<PathBuf>) -> Self {
        Self {
            seen: HashMap::new(),
            persist_path: Some(path.into()),
        }
    }

    /// The record saved at `path`, which later saves go back to.
    ///
    /// # Errors
    /// When the file cannot be read, does not parse, or is of another version.
    pub fn load(path: &Path) -> Result<Self, AgentError> {
        let bytes = std::fs::read(path).map_err(|e| AgentError::Config(e.to_string()))?;
        let saved: Saved =
            serde_json::from_slice(&bytes).map_err(|e| AgentError::Config(e.to_string()))?;
        if saved.version != SAVED_VERSION {
            return Err(AgentError::Config(format!(
                "{}: version {} of the read-file record is not supported",
                path.display(),
                saved.version
            )));
        }
        Ok(Self {
            seen: saved
                .files
                .into_iter()
                .map(|file| (file.path, file.stamp))
                .collect(),
            persist_path: Some(path.to_path_buf()),
        })
    }

    /// Save the record where [`StaleGuard::saved_to`] or [`StaleGuard::load`]
    /// said; without such a path, do nothing.
    ///
    /// Written to a temporary file and renamed into place, so a crash leaves
    /// the previous record rather than a torn one. A path that is not valid
    /// UTF-8 cannot be written to JSON and is left out: after a resume that
    /// file must be read again before it is written, which is the safe side.
    ///
    /// # Errors
    /// When the file cannot be written.
    pub fn persist(&self) -> Result<(), AgentError> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        let mut files: Vec<SavedFile> = self
            .seen
            .iter()
            .filter(|(path, _)| path.to_str().is_some())
            .map(|(path, stamp)| SavedFile {
                path: path.clone(),
                stamp: *stamp,
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let json = serde_json::to_vec_pretty(&Saved {
            version: SAVED_VERSION,
            files,
        })
        .map_err(|e| AgentError::Config(e.to_string()))?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, json)
            .and_then(|()| std::fs::rename(&temporary, path))
            .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))
    }

    /// Record that `path` was read and looked like `stamp`.
    ///
    /// Also called after a successful write, so a run of edits to one file does
    /// not trip the guard on its own previous write.
    pub fn record(&mut self, path: impl Into<PathBuf>, stamp: FileStamp) {
        self.seen.insert(path.into(), stamp);
    }

    /// Forget `path` — used when a write fails, so a partially applied change
    /// cannot leave a stamp claiming the agent has seen the result.
    pub fn forget(&mut self, path: &Path) {
        self.seen.remove(path);
    }

    /// Whether writing `path` is allowed, given how it looks on disk now.
    ///
    /// `current` is `None` when the file does not exist, which is always
    /// allowed: creating a file destroys nothing, and demanding a read of
    /// something that is not there would be a refusal with no remedy.
    pub fn check(&self, path: &Path, current: Option<FileStamp>) -> Result<(), Staleness> {
        let Some(current) = current else {
            return Ok(());
        };
        match self.seen.get(path) {
            None => Err(Staleness::NeverRead),
            Some(seen) if *seen != current => Err(Staleness::ChangedSinceRead),
            Some(_) => Ok(()),
        }
    }

    /// Number of files recorded. For tests and diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(len: u64, secs: u64) -> FileStamp {
        FileStamp {
            modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
            len,
            digest: Some(len),
        }
    }

    /// Creating a file destroys nothing, so a path that does not exist is always
    /// writable — refusing would be a refusal with no remedy.
    #[test]
    fn a_file_that_does_not_exist_may_always_be_written() {
        assert_eq!(
            StaleGuard::new().check(Path::new("/tmp/new.txt"), None),
            Ok(())
        );
    }

    #[test]
    fn an_unread_existing_file_is_refused() {
        assert_eq!(
            StaleGuard::new().check(Path::new("/tmp/a.txt"), Some(stamp(10, 1))),
            Err(Staleness::NeverRead)
        );
    }

    #[test]
    fn a_file_read_and_unchanged_may_be_written() {
        let mut guard = StaleGuard::new();
        guard.record("/tmp/a.txt", stamp(10, 1));
        assert_eq!(
            guard.check(Path::new("/tmp/a.txt"), Some(stamp(10, 1))),
            Ok(())
        );
    }

    /// The two cheap halves catch different edits: an equal-length rewrite
    /// changes only `mtime`, and a same-timestamp change only the length.
    #[test]
    fn either_half_of_the_stamp_detects_a_change() {
        let mut guard = StaleGuard::new();
        guard.record("/tmp/a.txt", stamp(10, 1));
        assert_eq!(
            guard.check(Path::new("/tmp/a.txt"), Some(stamp(10, 2))),
            Err(Staleness::ChangedSinceRead)
        );
        assert_eq!(
            guard.check(Path::new("/tmp/a.txt"), Some(stamp(11, 1))),
            Err(Staleness::ChangedSinceRead)
        );
    }

    /// **The case length and mtime both miss.** An equal-length rewrite inside
    /// one clock tick — a formatter reflowing a line — is invisible to both
    /// cheap signals, and is the edit most likely to happen under an agent's
    /// feet. Only the content digest catches it.
    #[test]
    fn an_equal_length_rewrite_in_one_tick_is_caught_by_the_digest() {
        let same_stat = |digest: u64| FileStamp {
            modified: Some(std::time::UNIX_EPOCH),
            len: 11,
            digest: Some(digest),
        };
        let mut guard = StaleGuard::new();
        guard.record("/tmp/a.txt", same_stat(1));
        assert_eq!(
            guard.check(Path::new("/tmp/a.txt"), Some(same_stat(2))),
            Err(Staleness::ChangedSinceRead)
        );
        assert_eq!(
            guard.check(Path::new("/tmp/a.txt"), Some(same_stat(1))),
            Ok(())
        );
    }

    /// Two real files with identical bytes stamp identically, and differing
    /// bytes stamp differently — the digest actually reads the content.
    #[tokio::test]
    async fn the_digest_reflects_the_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::write(&a, "hello world").unwrap();
        let first = FileStamp::of(&a).await.expect("the file exists");

        std::fs::write(&a, "hello there").unwrap(); // same length, different bytes
        let second = FileStamp::of(&a).await.expect("the file exists");
        assert_ne!(
            first.digest, second.digest,
            "an equal-length rewrite must change the digest"
        );

        std::fs::write(&a, "hello world").unwrap();
        assert_eq!(first.digest, FileStamp::of(&a).await.unwrap().digest);
    }

    /// Re-recording after a write is what lets a run of edits to one file
    /// proceed without the agent's own previous write tripping the guard.
    #[test]
    fn re_recording_after_a_write_allows_the_next_edit() {
        let mut guard = StaleGuard::new();
        guard.record("/tmp/a.txt", stamp(10, 1));
        guard.record("/tmp/a.txt", stamp(20, 5));
        assert_eq!(
            guard.check(Path::new("/tmp/a.txt"), Some(stamp(20, 5))),
            Ok(())
        );
    }

    /// One file's record says nothing about another's.
    #[test]
    fn records_are_per_path() {
        let mut guard = StaleGuard::new();
        guard.record("/tmp/a.txt", stamp(10, 1));
        assert_eq!(
            guard.check(Path::new("/tmp/b.txt"), Some(stamp(10, 1))),
            Err(Staleness::NeverRead)
        );
        assert_eq!(guard.len(), 1);
    }

    /// The saved digests are compared by a later process, possibly a later
    /// build, so the algorithm is pinned to the published FNV-1a vectors.
    #[test]
    fn the_digest_is_the_same_in_every_build() {
        assert_eq!(FileStamp::digest_of(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(FileStamp::digest_of(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(FileStamp::digest_of(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// What a session read survives into the next process: the saved record
    /// loads back, still allows the unchanged file, and still catches a change.
    #[tokio::test]
    async fn a_saved_record_loads_back_and_still_catches_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "pub fn f() {}\n").unwrap();
        let saved = dir.path().join("records.jsonl.reads.json");

        let mut guard = StaleGuard::saved_to(&saved);
        guard.record(&file, FileStamp::of(&file).await.unwrap());
        guard.persist().unwrap();

        let loaded = StaleGuard::load(&saved).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.check(&file, FileStamp::of(&file).await), Ok(()));

        std::fs::write(&file, "pub fn g() {}\n").unwrap();
        assert_eq!(
            loaded.check(&file, FileStamp::of(&file).await),
            Err(Staleness::ChangedSinceRead)
        );
    }

    /// A sub-agent's record lives in memory only.
    #[test]
    fn a_guard_without_a_path_saves_nothing() {
        let mut guard = StaleGuard::new();
        guard.record("/tmp/a.txt", stamp(10, 1));
        assert!(guard.persist().is_ok());
    }

    #[test]
    fn a_record_of_another_version_is_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let saved = dir.path().join("records.jsonl.reads.json");
        std::fs::write(&saved, r#"{"version":2,"files":[]}"#).unwrap();
        assert!(StaleGuard::load(&saved).is_err());
    }

    /// The messages must tell the model what to do next; a refusal it cannot act
    /// on just becomes a retry of the same call. The remedy is `read_file`: a
    /// file seen through `cat` is not a file the guard knows was read.
    #[test]
    fn both_refusals_name_the_remedy() {
        let path = Path::new("/tmp/a.txt");
        for reason in [Staleness::NeverRead, Staleness::ChangedSinceRead] {
            let msg = reason.message(path);
            assert!(msg.contains("/tmp/a.txt"), "{msg}");
            assert!(
                msg.contains("with read_file, then retry"),
                "must name the remedy: {msg}"
            );
        }
        assert!(
            Staleness::NeverRead
                .message(path)
                .contains("what a shell command printed does not count")
        );
    }
}
