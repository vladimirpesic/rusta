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
//! Paths are stored workspace-relative and *only* workspace-relative:
//! [`confine`] normalizes `./` away and refuses absolute paths and `..`.
//! That matters because the read-set is not just bookkeeping — cross-file
//! retry (§6.3 step 5) writes to the files in it.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// The session file-set: every file read this session, deterministically
/// ordered (`BTreeSet`) so resolution and cross-file retry are reproducible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    entries: BTreeSet<PathBuf>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `path` was read. Idempotent. A path outside the
    /// workspace is not recorded: it could never be legally edited, and
    /// admitting one would hand cross-file retry a target outside the repo.
    pub fn record_read(&mut self, path: &Path) {
        if let Some(rel) = confine(path) {
            self.entries.insert(rel);
        }
    }

    /// True when `path` has been read this session (auto-inject satisfies this).
    pub fn has_read(&self, path: &Path) -> bool {
        confine(path).is_some_and(|rel| self.entries.contains(&rel))
    }

    /// Removes `path` from the session read-set — `/drop` (plan §6.9). True
    /// when the file was present. Auto-inject re-protects a later edit of the
    /// dropped file, so dropping is always safe.
    pub fn drop_read(&mut self, path: &Path) -> bool {
        confine(path).is_some_and(|rel| self.entries.remove(&rel))
    }

    /// All read files in deterministic (sorted) order — the session read-set
    /// used by filename resolution and cross-file retry.
    pub fn read_set(&self) -> impl Iterator<Item = &PathBuf> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Canonicalize a workspace-relative path, or refuse it: `.` components are
/// dropped, absolute paths and any `..` yield `None`. Every mutation in this
/// crate resolves through here, so the §6.12 fence sits at the boundary
/// rather than in each caller (`rusta-tools`' `safe_rel` guards tool-call
/// *inputs*, which is a different thing).
pub(crate) fn confine(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    Some(
        path.components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect(),
    )
}

/// Whether `rel`, resolved under `root`, actually stays inside the repo.
///
/// `confine` rejects `..` and absolute paths *lexically*, which stops the
/// spelling but not the filesystem: a symlink committed inside the repo and
/// pointing outside it is followed by `fs::write`, so `docs/notes.md` can
/// land in `/etc`. The deepest existing ancestor of the target is
/// canonicalized and checked against the canonical root, which catches a
/// link anywhere along the path whether or not the final component exists
/// yet (creates must be checked too).
///
/// **This is hardening, not a sandbox.** There is an unavoidable TOCTOU
/// window between this check and the write, and a root that cannot itself be
/// canonicalized falls back to the lexical fence alone. §6.12's framing
/// holds: a guard rail, not an isolation boundary.
pub fn contains_path(root: &Path, rel: &Path) -> bool {
    let Ok(real_root) = root.canonicalize() else {
        return true; // unknowable root — the lexical fence is all there is
    };
    let mut probe = root.join(rel);
    loop {
        if let Ok(real) = probe.canonicalize() {
            return real.starts_with(&real_root);
        }
        // The target does not exist yet; walk up to the nearest ancestor
        // that does. Stops at `root`, which canonicalized above.
        match probe.parent() {
            Some(parent) if parent.starts_with(root) => probe = parent.to_path_buf(),
            _ => return true,
        }
    }
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

        // Recording the same file again is idempotent.
        ledger.record_read(Path::new("src/main.rs"));
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn paths_outside_the_workspace_are_never_recorded() {
        // The read-set feeds cross-file retry, which writes to it.
        let mut ledger = Ledger::new();
        for outside in ["/etc/passwd", "../escape.rs", "a/../../escape.rs"] {
            ledger.record_read(Path::new(outside));
            assert!(
                !ledger.has_read(Path::new(outside)),
                "{outside} was admitted"
            );
        }
        assert!(
            ledger.is_empty(),
            "{:?}",
            ledger.read_set().collect::<Vec<_>>()
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
