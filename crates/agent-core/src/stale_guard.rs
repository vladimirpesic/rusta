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
//!
//! The same record tells the agent when a file it saw changes
//! ([`StaleGuard::news`]): before each request, every file whose contents
//! moved since is named, with the lines around each change as they are now,
//! so the agent builds on the change instead of on what it saw, and does not
//! undo it. For that the guard keeps the text of the files it saw, up to
//! [`SNAPSHOT_MAX_BYTES`] each; of a larger file, or one seen before a resume,
//! it can only say that it changed.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// The largest file whose text the guard keeps, to show later what changed in
/// it ([`StaleGuard::news`]).
pub const SNAPSHOT_MAX_BYTES: usize = 256 * 1024;
/// The most text the kept files take together; past it, the text kept
/// longest goes first.
const SNAPSHOTS_MAX_BYTES: usize = 8 * 1024 * 1024;
/// The most characters one note gives to showing changes, and to one file's.
const NEWS_MAX_CHARS: usize = 12_000;
const NEWS_FILE_MAX_CHARS: usize = 4_000;
/// The most files one note names; the rest are counted.
const NEWS_MAX_FILES: usize = 20;
/// Unchanged lines shown on each side of a change.
const NEWS_CONTEXT_LINES: usize = 2;
/// A line of a change longer than this is cut.
const NEWS_LINE_MAX_CHARS: usize = 400;
/// After this many notes in a row about one file, it is told of no more until
/// the agent reads it again: a log being written would otherwise be told of
/// at every step.
const NEWS_IN_A_ROW: u8 = 3;
/// What every note about changed files opens with.
const NEWS_HEADER: &str = "Files you have read changed on disk since: the user edited them, a \
     formatter or a command rewrote them, or another agent did. Take the changes as intended: \
     build on them, and do not undo them unless asked. Line numbers are as the files are now.";
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

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

    /// Whether `metadata` shows the length and modification time this stamp
    /// recorded: the cheap half of the comparison, which leaves the contents
    /// unread.
    fn matches_metadata(&self, metadata: &std::fs::Metadata) -> bool {
        self.len == metadata.len() && self.modified == metadata.modified().ok()
    }

    /// Whether both stamps digested their contents, and found them the same.
    fn same_contents(&self, other: &Self) -> bool {
        self.digest.is_some() && self.digest == other.digest
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
    /// The text of each recorded file small enough to keep, as the agent last
    /// saw it, to show what changed in it since. Not saved: after a resume the
    /// guard can only say that a file changed.
    snapshots: HashMap<PathBuf, Arc<str>>,
    /// The files whose text is kept, the one kept longest first.
    snapshot_order: VecDeque<PathBuf>,
    /// The bytes the kept texts take together.
    snapshot_bytes: usize,
    /// What [`StaleGuard::news`] has said of each file since the agent last
    /// saw it.
    told: HashMap<PathBuf, Told>,
}

/// What the notes said of one file since the agent last saw it.
#[derive(Debug, Default, Clone, Copy)]
struct Told {
    /// Notes in a row that told of a change to it.
    in_a_row: u8,
    /// Told of no more until the agent reads or writes it again.
    quiet: bool,
}

/// What became of one file since the agent saw it, before it is told.
enum News {
    /// Its contents changed, and the change can be shown: `changes` is the
    /// text, `now` how the file looks, `text` what it holds.
    Changed {
        path: PathBuf,
        changes: String,
        now: FileStamp,
        text: String,
    },
    /// Its contents changed, and the change cannot be shown.
    ChangedUnseen { path: PathBuf },
    /// It is gone, or no longer a file.
    Gone { path: PathBuf, what: &'static str },
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
            persist_path: Some(path.into()),
            ..Self::default()
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
            ..Self::default()
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
        crate::private_fs::write(path, json)
            .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))
    }

    /// Record that `path` was read and looked like `stamp`, its contents
    /// unknown to the guard.
    ///
    /// Also called after a successful write, so a run of edits to one file does
    /// not trip the guard on its own previous write.
    pub fn record(&mut self, path: impl Into<PathBuf>, stamp: FileStamp) {
        let path = path.into();
        self.drop_snapshot(&path);
        self.told.remove(&path);
        self.seen.insert(path, stamp);
    }

    /// [`Self::record`] a file whose text the agent saw as `text`: kept, when
    /// small enough, to show what changes in it later ([`Self::news`]).
    pub fn record_text(&mut self, path: impl Into<PathBuf>, stamp: FileStamp, text: &str) {
        let path = path.into();
        self.record(path.clone(), stamp);
        self.keep_snapshot(path, text);
    }

    /// Forget `path` — used when a write fails, so a partially applied change
    /// cannot leave a stamp claiming the agent has seen the result.
    pub fn forget(&mut self, path: &Path) {
        self.seen.remove(path);
        self.drop_snapshot(path);
        self.told.remove(path);
    }

    /// Keep `text` as what `path` holds, when it is small enough; the texts
    /// kept longest go when they take more than [`SNAPSHOTS_MAX_BYTES`].
    fn keep_snapshot(&mut self, path: PathBuf, text: &str) {
        self.drop_snapshot(&path);
        // As a file is read back for a note: without its byte-order mark.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        if text.len() > SNAPSHOT_MAX_BYTES {
            return;
        }
        self.snapshot_bytes += text.len();
        self.snapshots.insert(path.clone(), Arc::from(text));
        self.snapshot_order.push_back(path);
        while self.snapshot_bytes > SNAPSHOTS_MAX_BYTES {
            let Some(oldest) = self.snapshot_order.pop_front() else {
                break;
            };
            if let Some(text) = self.snapshots.remove(&oldest) {
                self.snapshot_bytes -= text.len();
            }
        }
    }

    fn drop_snapshot(&mut self, path: &Path) {
        if let Some(text) = self.snapshots.remove(path) {
            self.snapshot_bytes -= text.len();
            self.snapshot_order.retain(|kept| kept != path);
        }
    }

    /// What became of the files this agent saw since it saw them, as a note
    /// for the model, or `None` when nothing did. Paths are shown relative to
    /// `cwd`; files under `skip` (the session's own records and spill files)
    /// are not watched.
    ///
    /// A file whose contents changed is shown at each change, its lines
    /// numbered as they are now, and counts from then on as seen: the agent
    /// may overwrite it without reading it again. One whose change cannot be
    /// shown — too large, not UTF-8 text, seen before a resume, or past the
    /// note's room — is named, stays unseen until read again, and is not told
    /// of again until then. A file told of in [`NEWS_IN_A_ROW`] notes running
    /// is told of no more until read again. Files only touched, their contents
    /// the same, are not told of.
    pub fn news(&mut self, cwd: &Path, skip: &[PathBuf]) -> Option<String> {
        let mut paths: Vec<PathBuf> = self
            .seen
            .keys()
            .filter(|path| !skip.iter().any(|root| path.starts_with(root)))
            .filter(|path| !self.told.get(*path).is_some_and(|told| told.quiet))
            .cloned()
            .collect();
        paths.sort();
        let mut sections = Vec::new();
        let mut unnamed = 0usize;
        let mut room = NEWS_MAX_CHARS;
        for path in paths {
            let Some(news) = self.news_of(&path) else {
                continue;
            };
            let named = sections.len() < NEWS_MAX_FILES;
            match news {
                News::Changed {
                    path,
                    changes,
                    now,
                    text,
                } if named && changes.chars().count() <= NEWS_FILE_MAX_CHARS.min(room) => {
                    room -= changes.chars().count();
                    let in_a_row = self.told.get(&path).map_or(0, |told| told.in_a_row) + 1;
                    let quiet = in_a_row >= NEWS_IN_A_ROW;
                    let mut section = format!(
                        "{} changed. Around each change it now reads:\n{changes}",
                        shown(&path, cwd)
                    );
                    if quiet {
                        section.push_str(
                            "\nIt keeps changing: it is not told of again until you read it.",
                        );
                    }
                    self.seen.insert(path.clone(), now);
                    self.keep_snapshot(path.clone(), &text);
                    self.told.insert(path, Told { in_a_row, quiet });
                    sections.push(section);
                }
                News::Changed { path, .. } | News::ChangedUnseen { path } => {
                    self.told.insert(
                        path.clone(),
                        Told {
                            in_a_row: 0,
                            quiet: true,
                        },
                    );
                    if named {
                        sections.push(format!(
                            "{} changed; read it again before you rely on what you saw of it.",
                            shown(&path, cwd)
                        ));
                    } else {
                        unnamed += 1;
                    }
                }
                News::Gone { path, what } => {
                    self.forget(&path);
                    if named {
                        sections.push(format!("{} {what}.", shown(&path, cwd)));
                    } else {
                        unnamed += 1;
                    }
                }
            }
        }
        if unnamed > 0 {
            sections.push(format!(
                "{unnamed} more file(s) you have read changed or went; read any you need again."
            ));
        }
        if sections.is_empty() {
            return None;
        }
        Some(format!(
            "<system-reminder>\n{NEWS_HEADER}\n\n{}\n</system-reminder>",
            sections.join("\n\n")
        ))
    }

    /// What became of `path` since the agent saw it; `None` when its contents
    /// are what it saw, or it cannot be looked at now. A file whose length and
    /// modification time moved and whose contents did not is re-stamped.
    fn news_of(&mut self, path: &Path) -> Option<News> {
        let seen = *self.seen.get(path)?;
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                return Some(News::Gone {
                    path: path.to_path_buf(),
                    what: "is no longer a file",
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Some(News::Gone {
                    path: path.to_path_buf(),
                    what: "was deleted",
                });
            }
            Err(_) => return None,
        };
        if seen.matches_metadata(&metadata) {
            // Quiet since the last note: a later change starts a new run.
            self.told.remove(path);
            return None;
        }
        let bytes = (metadata.len() <= DIGEST_SIZE_LIMIT)
            .then(|| std::fs::read(path).ok())
            .flatten();
        let now = FileStamp::from_parts(&metadata, bytes.as_deref());
        if now.same_contents(&seen) {
            self.seen.insert(path.to_path_buf(), now);
            return None;
        }
        let text = bytes
            .as_deref()
            .filter(|bytes| bytes.len() <= SNAPSHOT_MAX_BYTES)
            .and_then(|bytes| {
                std::str::from_utf8(bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes)).ok()
            });
        match (self.snapshots.get(path), text) {
            (Some(before), Some(text)) => Some(News::Changed {
                path: path.to_path_buf(),
                changes: changed_lines(before, text),
                now,
                text: text.to_string(),
            }),
            _ => Some(News::ChangedUnseen {
                path: path.to_path_buf(),
            }),
        }
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

/// `path` as a note names it: relative to `cwd` when inside it.
fn shown(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// The lines around each change from `before` to `now`, numbered as they
/// are in `now`, as `read_file` numbers them (`<number><TAB><text>`); a gap
/// between changes is a line of `...`.
fn changed_lines(before: &str, now: &str) -> String {
    if now.is_empty() {
        return "(the file is now empty)".to_string();
    }
    let lines: Vec<&str> = now.lines().collect();
    let diff = similar::TextDiff::from_lines(before, now);
    let mut out = Vec::new();
    for group in diff.grouped_ops(NEWS_CONTEXT_LINES) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let (start, end) = (first.new_range().start, last.new_range().end);
        if !out.is_empty() {
            out.push("...".to_string());
        }
        if start == end {
            out.push(format!("(lines were removed after line {start})"));
            continue;
        }
        for (index, line) in lines.iter().enumerate().take(end).skip(start) {
            let line = match line.char_indices().nth(NEWS_LINE_MAX_CHARS) {
                Some((at, _)) => format!("{}…", &line[..at]),
                None => (*line).to_string(),
            };
            out.push(format!("{}\t{line}", index + 1));
        }
    }
    out.join("\n")
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
    /// Read `path` as `read_file` records it: stamped, its text kept.
    fn see(guard: &mut StaleGuard, path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        let metadata = std::fs::metadata(path).unwrap();
        let stamp = FileStamp::from_parts(&metadata, Some(&bytes));
        guard.record_text(path, stamp, std::str::from_utf8(&bytes).unwrap());
    }

    /// Rewrite `path` so that its length or modification time moves too, as
    /// any real edit leaves it.
    fn rewrite(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }

    fn numbered(lines: std::ops::RangeInclusive<usize>) -> String {
        lines.map(|n| format!("line {n}\n")).collect()
    }

    /// A change to a file the agent read is told once, with the lines around
    /// it as they are now, and the file counts as seen from then on.
    #[test]
    fn a_changed_file_is_told_with_its_changed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, numbered(1..=20)).unwrap();
        let mut guard = StaleGuard::new();
        see(&mut guard, &path);
        assert_eq!(guard.news(dir.path(), &[]), None);

        rewrite(&path, &numbered(1..=20).replace("line 10\n", "line ten\n"));
        let note = guard.news(dir.path(), &[]).expect("the change is told");
        assert!(
            note.starts_with("<system-reminder>\nFiles you have read changed"),
            "{note}"
        );
        assert!(
            note.contains("notes.txt changed. Around each change it now reads:\n8\tline 8\n9\tline 9\n10\tline ten\n11\tline 11\n12\tline 12\n"),
            "{note}"
        );
        assert!(!note.contains("line 7"), "{note}");
        // Told once, and now seen: no note, and an overwrite is allowed.
        assert_eq!(guard.news(dir.path(), &[]), None);
        let current = FileStamp::from_parts(
            &std::fs::metadata(&path).unwrap(),
            Some(&std::fs::read(&path).unwrap()),
        );
        assert_eq!(guard.check(&path, Some(current)), Ok(()));
    }

    /// Changes far apart are shown apart, a gap between them.
    #[test]
    fn changes_apart_are_shown_apart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, numbered(1..=30)).unwrap();
        let mut guard = StaleGuard::new();
        see(&mut guard, &path);
        rewrite(
            &path,
            &numbered(1..=30)
                .replace("line 3\n", "line three\n")
                .replace("line 25\n", ""),
        );
        let note = guard.news(dir.path(), &[]).unwrap();
        assert!(note.contains("3\tline three\n"), "{note}");
        assert!(note.contains("\n...\n"), "{note}");
        assert!(note.contains("24\tline 24\n25\tline 26\n"), "{note}");
    }

    /// A file whose change cannot be shown is named, stays unseen until read
    /// again, and is not told of again until then.
    #[test]
    fn a_change_that_cannot_be_shown_asks_for_a_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        std::fs::write(&path, "small\n").unwrap();
        let mut guard = StaleGuard::new();
        // Seen without its text, as after a resume.
        let metadata = std::fs::metadata(&path).unwrap();
        guard.record(&path, FileStamp::from_parts(&metadata, Some(b"small\n")));
        rewrite(&path, "changed\n");
        let note = guard.news(dir.path(), &[]).unwrap();
        assert!(
            note.contains("big.txt changed; read it again before you rely on what you saw of it."),
            "{note}"
        );
        let current = FileStamp::from_parts(&std::fs::metadata(&path).unwrap(), Some(b"changed\n"));
        assert_eq!(
            guard.check(&path, Some(current)),
            Err(Staleness::ChangedSinceRead)
        );
        rewrite(&path, "changed again\n");
        assert_eq!(guard.news(dir.path(), &[]), None, "quiet until read again");
        see(&mut guard, &path);
        rewrite(&path, "and again\n");
        assert!(
            guard
                .news(dir.path(), &[])
                .unwrap()
                .contains("1\tand again")
        );
    }

    #[test]
    fn a_deleted_file_is_told_once_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.txt");
        std::fs::write(&path, "x\n").unwrap();
        let mut guard = StaleGuard::new();
        see(&mut guard, &path);
        std::fs::remove_file(&path).unwrap();
        let note = guard.news(dir.path(), &[]).unwrap();
        assert!(note.contains("gone.txt was deleted."), "{note}");
        assert!(guard.is_empty());
        assert_eq!(guard.news(dir.path(), &[]), None);
    }

    /// A file touched, its contents the same, is no news.
    #[test]
    fn a_touched_file_is_no_news() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.txt");
        std::fs::write(&path, "same\n").unwrap();
        let mut guard = StaleGuard::new();
        see(&mut guard, &path);
        rewrite(&path, "same\n");
        assert_eq!(guard.news(dir.path(), &[]), None);
    }

    /// A file told of in three notes running is told of no more until it is
    /// read again: a log being written is not news at every step.
    #[test]
    fn a_file_that_keeps_changing_goes_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        std::fs::write(&path, "1\n").unwrap();
        let mut guard = StaleGuard::new();
        see(&mut guard, &path);
        let mut text = "1\n".to_string();
        for n in 2..=4 {
            text.push_str(&format!("{n}\n"));
            rewrite(&path, &text);
            let note = guard.news(dir.path(), &[]).expect("told");
            assert_eq!(note.contains("It keeps changing"), n == 4, "{note}");
        }
        text.push_str("5\n");
        rewrite(&path, &text);
        assert_eq!(guard.news(dir.path(), &[]), None);
    }

    /// The session's own records and spill files change at every step; they
    /// are not watched.
    #[test]
    fn skipped_folders_are_not_watched() {
        let dir = tempfile::tempdir().unwrap();
        let records = dir.path().join("records");
        std::fs::create_dir(&records).unwrap();
        let path = records.join("session.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let mut guard = StaleGuard::new();
        see(&mut guard, &path);
        rewrite(&path, "{}\n{}\n");
        assert_eq!(guard.news(dir.path(), &[records]), None);
    }

    /// The kept texts stay within their budget, the ones kept longest going
    /// first, and a file too large to keep is not kept.
    #[test]
    fn kept_texts_stay_within_their_budget() {
        let mut guard = StaleGuard::new();
        let stamp = stamp(1, 1);
        let big = "x".repeat(SNAPSHOT_MAX_BYTES);
        let count = SNAPSHOTS_MAX_BYTES / SNAPSHOT_MAX_BYTES + 2;
        for n in 0..count {
            guard.record_text(format!("/f{n}"), stamp, &big);
        }
        assert!(guard.snapshot_bytes <= SNAPSHOTS_MAX_BYTES);
        assert!(!guard.snapshots.contains_key(Path::new("/f0")));
        assert!(
            guard
                .snapshots
                .contains_key(Path::new(&format!("/f{}", count - 1)))
        );
        guard.record_text("/huge", stamp, &"y".repeat(SNAPSHOT_MAX_BYTES + 1));
        assert!(!guard.snapshots.contains_key(Path::new("/huge")));
        guard.forget(Path::new(&format!("/f{}", count - 1)));
        assert_eq!(
            guard.snapshot_bytes,
            guard
                .snapshots
                .values()
                .map(|text| text.len())
                .sum::<usize>()
        );
    }

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
