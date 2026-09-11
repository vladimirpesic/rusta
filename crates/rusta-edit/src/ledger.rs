//! Read-before-edit ledger — development plan §6.3 (R4).
//!
//! Every mutation (`edit`/`write` tools, applied SEARCH/REPLACE blocks)
//! requires its file to have been read during the session (via `read` or
//! `map_drill`). The ledger is that session file-set: it feeds filename
//! resolution and cross-file retry, and it backs the auto-inject rule —
//! a mutation of an unread file injects a read, notifies the model, and
//! retries the block once. This replaces smallcode's compound
//! `read_and_patch` tool with identical effect and less surface (DECIDED).
//!
//! Paths are stored workspace-relative, canonically without a leading
//! `./` so that `read src/a.rs` and a model's `./src/a.rs` agree.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Per-file read accounting. The count lets the UI surface how often a
/// file entered the context (and future loop detectors spot re-reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRecord {
    /// Number of times the file was read this session (≥ 1 when present).
    pub reads: u32,
}

/// The session file-set: every file read this session, deterministically
/// ordered (`BTreeMap`) so resolution and cross-file retry are reproducible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    entries: BTreeMap<PathBuf, ReadRecord>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `path` was read. Idempotent per read event; the counter
    /// increments on each record.
    pub fn record_read(&mut self, path: &Path) {
        let key = canonical(path);
        let entry = self.entries.entry(key).or_insert(ReadRecord { reads: 0 });
        entry.reads += 1;
    }

    /// True when `path` has been read this session (auto-inject satisfies this).
    pub fn has_read(&self, path: &Path) -> bool {
        self.entries.contains_key(&canonical(path))
    }

    /// The recorded file, if any, for `path`.
    pub fn record(&self, path: &Path) -> Option<&ReadRecord> {
        self.entries.get(&canonical(path))
    }

    /// Removes `path` from the session read-set — `/drop` (plan §6.9). True
    /// when the file was present. Auto-inject re-protects a later edit of the
    /// dropped file, so dropping is always safe.
    pub fn drop_read(&mut self, path: &Path) -> bool {
        self.entries.remove(&canonical(path)).is_some()
    }

    /// All read files in deterministic (sorted) order — the session read-set
    /// used by filename resolution and cross-file retry.
    pub fn read_set(&self) -> impl Iterator<Item = &PathBuf> {
        self.entries.keys()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Canonicalize a workspace-relative path: drop `.` components so `./a/b.rs`
/// and `a/b.rs` are the same ledger key. Absolute paths and `..` are kept
/// verbatim — the ledger never second-guesses, it only normalizes spelling.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_queries_reads() {
        let mut ledger = Ledger::new();
        assert!(ledger.is_empty());
        assert!(!ledger.has_read(Path::new("src/main.rs")));

        ledger.record_read(Path::new("src/main.rs"));
        assert!(ledger.has_read(Path::new("src/main.rs")));
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            ledger.record(Path::new("src/main.rs")).map(|r| r.reads),
            Some(1)
        );

        ledger.record_read(Path::new("src/main.rs"));
        assert_eq!(
            ledger.record(Path::new("src/main.rs")).map(|r| r.reads),
            Some(2)
        );
    }

    #[test]
    fn leading_dot_component_is_canonicalized() {
        let mut ledger = Ledger::new();
        ledger.record_read(Path::new("./src/lib.rs"));
        assert!(ledger.has_read(Path::new("src/lib.rs")));
        assert!(ledger.has_read(Path::new("./src/lib.rs")));
    }

    #[test]
    fn drop_read_removes_only_the_named_file() {
        let mut ledger = Ledger::new();
        ledger.record_read(Path::new("src/lib.rs"));
        ledger.record_read(Path::new("src/main.rs"));

        assert!(ledger.drop_read(Path::new("./src/lib.rs"))); // canonical spelling
        assert!(!ledger.has_read(Path::new("src/lib.rs")));
        assert!(ledger.has_read(Path::new("src/main.rs")));
        assert!(!ledger.drop_read(Path::new("src/lib.rs"))); // already gone
    }

    #[test]
    fn read_set_is_deterministically_ordered() {
        let mut ledger = Ledger::new();
        for p in ["zeta.rs", "alpha.rs", "mid/beta.rs"] {
            ledger.record_read(Path::new(p));
        }
        let set: Vec<&PathBuf> = ledger.read_set().collect();
        assert_eq!(
            set,
            vec![
                &PathBuf::from("alpha.rs"),
                &PathBuf::from("mid/beta.rs"),
                &PathBuf::from("zeta.rs")
            ]
        );
    }
}
