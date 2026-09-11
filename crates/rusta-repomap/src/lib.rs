//! Tree-sitter repository map for Rusta — development plan §6.5 (R6).
//!
//! `rusta-repomap` gives the model AST-level awareness of the whole repository
//! within a token budget. The pipeline (semantics ported from Aider's
//! `repomap.py`; the plan's numbers are normative):
//!
//! 1. file discovery — git-tracked sources filtered to configured languages
//!    (`discover`), with an ignore-aware walk fallback outside git repos;
//! 2. tag extraction — tree-sitter `.scm` queries yielding def/ref tags per
//!    file (`tags`), with a word-scan ref backfill for def-only languages;
//! 3. graph construction — edge weights `mul / (|D| · n_r)` with mention and
//!    multi-case boosts (`graph`);
//! 4. personalized PageRank ranking (damping 0.85, personalization `100/N`
//!    plus chat/mention boosts);
//! 5. budget-fitted rendering — 8 context lines per definition, 100-char line
//!    truncation, 100-line sampling cost estimation, middle-drop fitting
//!    (`render`);
//! 6. an in-memory `(path, mtime, size, query_version)` cache (`cache`).
//!
//! [`drill`] backs the `map_drill` tool (§6.4): a definition's full span or a
//! line window of a file. The M7 tool registry wires it to the session and
//! the read-before-edit ledger.

mod cache;
mod discover;
mod drill;
mod graph;
mod lang;
mod render;
mod tags;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub use drill::{DrillError, DrillRequest, drill};
use graph::{Mentions, RankedFile, rank_files};

/// Default map budget in estimated tokens (Aider's default).
const DEFAULT_MAP_TOKENS: usize = 1024;

/// The repo map for one working tree.
#[derive(Debug)]
pub struct RepoMap {
    root: PathBuf,
    max_map_tokens: usize,
    cache: cache::TagCache,
}

impl RepoMap {
    /// Map for the repo rooted at `root`, with the default token budget.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_map_tokens: DEFAULT_MAP_TOKENS,
            cache: cache::TagCache::new(),
        }
    }

    /// Override the token budget (a budget of 0 disables the map).
    #[must_use]
    pub fn with_budget(mut self, max_map_tokens: usize) -> Self {
        self.max_map_tokens = max_map_tokens;
        self
    }

    /// Render the repo map (§6.5 steps 1–6).
    ///
    /// * `chat_files` — repo-relative paths already in the session context;
    ///   they still steer ranking but never render;
    /// * `other_files` — repo-relative paths to consider; `None` discovers
    ///   them (`git ls-files`, ignore-aware walk fallback);
    /// * `mentioned_*` — files/identifiers the user mentioned, boosting
    ///   personalization and edge weights.
    pub fn render_map(
        &mut self,
        chat_files: &[String],
        other_files: Option<&[String]>,
        mentioned_files: &[String],
        mentioned_idents: &[String],
    ) -> String {
        if self.max_map_tokens == 0 {
            return String::new();
        }
        let others = other_files
            .map(|f| {
                let mut v = f.to_vec();
                v.sort_unstable();
                v.dedup();
                v
            })
            .unwrap_or_else(|| discover::source_files(&self.root));
        let chat: BTreeSet<String> = chat_files.iter().cloned().collect();

        let mut file_tags: BTreeMap<String, Vec<tags::Tag>> = BTreeMap::new();
        let mut bare: BTreeSet<String> = BTreeSet::new();
        for rel in chat.iter().chain(others.iter()) {
            let abs = self.root.join(rel);
            let Some(lang) = lang::Lang::from_path(&abs) else {
                continue;
            };
            match self.cache.get_or_extract(&abs, rel, lang) {
                Some([]) => {
                    bare.insert(rel.clone());
                }
                Some(t) => {
                    file_tags.insert(rel.clone(), t.to_vec());
                }
                None => {}
            }
        }

        let mentions = Mentions {
            files: mentioned_files.iter().cloned().collect(),
            idents: mentioned_idents.iter().cloned().collect(),
        };
        let mut ranked = rank_files(&file_tags, &chat, &mentions);
        // Tag-less other files still appear as bare paths (Aider's
        // `rel_other_fnames_without_tags`), after every ranked file.
        ranked.extend(
            bare.into_iter()
                .filter(|rel| !chat.contains(rel))
                .map(|rel| RankedFile {
                    rel,
                    lois: Vec::new(),
                }),
        );

        let root = self.root.clone();
        let read = move |rel: &str| std::fs::read_to_string(root.join(rel)).ok();
        render::fit_map(&ranked, &chat, &read, self.max_map_tokens)
    }
}

/// Estimate the token cost of `text` the way the budget-fitting loop does
/// (≤ 100 evenly-spaced lines, scaled by the character ratio).
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    render::estimate_tokens(text)
}
