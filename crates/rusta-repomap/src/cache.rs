//! In-memory tag cache — DEVELOPMENT_PLAN.md §6.5 step 7.
//!
//! `(path, mtime, size, query_version) → Vec<Tag>`. Restarts re-scan (no
//! persistence in v1); an mtime or size bump invalidates the entry, and a
//! query-version bump invalidates everything.

use crate::lang::Lang;
use crate::tags::{QUERY_VERSION, Tag, extract_tags};
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

struct Entry {
    version: u32,
    mtime: SystemTime,
    size: u64,
    tags: Vec<Tag>,
}

pub(crate) struct TagCache {
    entries: HashMap<String, Entry>,
}

impl std::fmt::Debug for TagCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TagCache {{ entries: {} }}", self.entries.len())
    }
}

impl TagCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Cached tags for `abs` (keyed repo-relative), extracting on miss.
    /// `None` = unreadable/unsupported/unparseable — not cached, retried on
    /// the next map refresh (files can appear or become valid).
    pub(crate) fn get_or_extract(&mut self, abs: &Path, rel: &str, lang: Lang) -> Option<&[Tag]> {
        let meta = std::fs::metadata(abs).ok()?;
        let mtime = meta.modified().ok()?;
        let size = meta.len();
        let fresh = |e: &Entry| e.version == QUERY_VERSION && e.mtime == mtime && e.size == size;
        if self.entries.get(rel).is_some_and(&fresh) {
            return self.entries.get(rel).map(|e| e.tags.as_slice());
        }
        let source = std::fs::read_to_string(abs).ok()?;
        let tags = extract_tags(&source, lang)?;
        self.entries.insert(
            rel.to_string(),
            Entry {
                version: QUERY_VERSION,
                mtime,
                size,
                tags,
            },
        );
        self.entries.get(rel).map(|e| e.tags.as_slice())
    }
}
