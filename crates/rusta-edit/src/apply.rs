//! Apply chain, failure feedback, undo journal, and filename resolution —
//! ADR §6.3 (R4).
//!
//! Port of Aider's *proven* apply sequence, strictly in order:
//!
//! 1. exact line-sequence match;
//! 2. whitespace-flexible match (uniform leading-indentation flexibility);
//! 3. retry after dropping a spurious leading blank SEARCH line (Aider #25);
//! 4. `...` elision piece-matching;
//! 5. cross-file retry against every file in the session read-set;
//! 6. all strategies fail ⇒ structured failure feedback (Aider's format,
//!    verbatim semantics) — this *is* the repair loop.
//!
//! **DECIDED:** no edit-distance matching — Aider deliberately disables its
//! fuzzy-edit-distance path; for small models a clear retry request beats a
//! wrong-guess apply.
//!
//! Every applied block is journaled (path, before, after) to the undo stack
//! **before** the file write, and the read-before-edit ledger auto-injects
//! unread files (notify + apply — smallcode's `read_and_patch` effect).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::ledger::Ledger;
use crate::parser::{EditBlock, ParsedResponse};

/// What [`guarded`] should do to a file.
pub(crate) enum Mutation<'a> {
    /// Replace the file's contents, creating parent directories as needed.
    Write(&'a str),
    /// Delete the file; a file that is already gone is not an error.
    Remove,
}

/// **The single mutation point of this crate.** Every write, create and
/// delete goes through here, and every one is fenced by
/// [`crate::ledger::contains_path`] first.
///
/// The funnel is the point. The second audit fenced what it called "three
/// write paths"; there were four, and the uncounted one — restoring from the
/// undo journal — carried a delete as well as a write, reachable by `/undo`
/// on a resumed session whose log named absolute paths. Hand-enumeration has
/// now failed twice, so `tests/write_paths.rs` scans this crate's production
/// source for raw mutation calls and fails when one appears outside this
/// function. That guard is only as good as its primitive list — it once
/// covered four and missed five — so it now asserts its own proof of life
/// and is itself tested by injecting the calls it must catch.
pub(crate) fn guarded(root: &Path, rel: &Path, what: Mutation<'_>) -> io::Result<()> {
    if !crate::ledger::contains_path(root, rel) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} resolves outside the repository; every mutation must stay inside it",
                rel.display()
            ),
        ));
    }
    let abs = root.join(rel);
    match what {
        Mutation::Write(content) => {
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&abs, content)
        }
        Mutation::Remove => match fs::remove_file(&abs) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    }
}

/// One journaled mutation: enough to restore the file exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoEntry {
    /// Workspace-relative path of the edited file.
    pub path: PathBuf,
    /// Whether the file existed before the edit (`false` ⇒ undo deletes it).
    pub existed: bool,
    /// File content before the edit (empty when the file did not exist).
    pub before: String,
    /// File content after the edit.
    pub after: String,
}

/// The undo journal. Entries are pushed *before* each write; undo pops in
/// LIFO order and restores the previous content on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UndoStack {
    entries: Vec<UndoEntry>,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Journaled entries, oldest first.
    pub fn pending(&self) -> &[UndoEntry] {
        &self.entries
    }

    fn push(&mut self, entry: UndoEntry) {
        self.entries.push(entry);
    }

    fn pop(&mut self) -> Option<UndoEntry> {
        self.entries.pop()
    }

    /// Pop the most recent entry and restore its `before` content under
    /// `root`. Undoing a created file removes it (a leftover parent
    /// directory is harmless and kept). Returns `Ok(None)` when the journal
    /// is empty.
    /// A29: the entry is popped only *after* the restore succeeds. Popping
    /// first meant a failed write (full disk, read-only mount, lost
    /// permission) destroyed the record of what to restore — the `?` below
    /// returns after the pop — so the file stayed modified with no way back.
    /// Leaving it on the stack makes the failure retryable.
    pub fn undo_last(&mut self, root: &Path) -> io::Result<Option<UndoEntry>> {
        let Some(entry) = self.entries.last() else {
            return Ok(None);
        };
        // Fenced like every other mutation (§6.12). This path was the one
        // the second audit's count missed, and it reaches a *delete*: a
        // resumed session whose log named absolute paths could restore over,
        // and remove, files outside the repo.
        let what = if entry.existed {
            Mutation::Write(&entry.before)
        } else {
            Mutation::Remove
        };
        guarded(root, &entry.path, what)?;
        Ok(self.entries.pop())
    }

    /// Rebuilds a journal from recorded entries, oldest first — the
    /// `/resume` path (ADR §6.10): session replay reconstructs the undo
    /// stack from the event log and its diffs sidecar.
    pub fn from_entries(entries: Vec<UndoEntry>) -> Self {
        Self { entries }
    }
}

/// Why a block failed to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureReason {
    /// No apply-chain strategy matched the block against any candidate file.
    NoMatch,
    /// Filename resolution failed (no candidate matched and no continuation).
    MissingFilename,
    /// The resolved path escapes the workspace root (absolute, or `..`).
    /// §6.12 confines every mutation to the repo; this is that rule applied
    /// to the text SEARCH/REPLACE pathway, so both edit syntaxes share one
    /// fence as well as one apply chain (§6.4).
    OutsideRoot(String),
    /// Filesystem error while reading or writing (message included verbatim).
    Io(String),
}

/// One successfully applied block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedBlock {
    /// Workspace-relative path actually edited (may differ from the named
    /// file after cross-file retry).
    pub path: PathBuf,
    /// The edit created the file (empty SEARCH against a missing file).
    pub created: bool,
    /// The edit appended to an existing file (empty SEARCH, file present).
    pub appended: bool,
    /// The block was applied in a different file than the one it named
    /// (cross-file retry) — reported to the model.
    pub cross_file: bool,
}

/// One block that could not be applied; rendered as failure feedback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedBlock {
    pub block: EditBlock,
    /// Display form of the path the block named (or targeted).
    pub path: String,
    pub reason: FailureReason,
}

/// The outcome of applying one model response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Blocks applied, in document order.
    pub applied: Vec<AppliedBlock>,
    /// Blocks that failed, in document order.
    pub failed: Vec<FailedBlock>,
    /// Suggested shell commands surfaced for user confirmation.
    pub commands: Vec<String>,
    /// Corrective notes (malformed blocks, auto-injected reads,
    /// cross-file relocations).
    pub notes: Vec<String>,
    /// Model-facing failure feedback (§6.3 failure-feedback contract).
    /// `None` when every block applied.
    pub feedback: Option<String>,
}

impl ApplyReport {
    /// True when nothing failed (a response with no blocks also succeeds —
    /// it is prose, handled by the caller).
    pub fn is_success(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Session-bound edit engine: owns the workspace root, the read-before-edit
/// ledger, and the undo journal.
pub struct Editor {
    root: PathBuf,
    ledger: Ledger,
    undo: UndoStack,
}

impl Editor {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ledger: Ledger::new(),
            undo: UndoStack::new(),
        }
    }

    /// Workspace root every relative path resolves against.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn undo_stack(&self) -> &UndoStack {
        &self.undo
    }

    /// Record that a file was read this session (called by the `read` and
    /// `map_drill` tools, and by anything that injects file content).
    pub fn record_read(&mut self, rel: &str) {
        self.ledger.record_read(Path::new(rel));
    }

    /// Remove `rel` from the session read-set — the CLI's `/drop` (plan
    /// §6.9). True when it was present; auto-inject re-protects later edits.
    pub fn drop_read(&mut self, rel: &str) -> bool {
        self.ledger.drop_read(Path::new(rel))
    }

    /// Undo the most recent applied edit; see [`UndoStack::undo_last`].
    pub fn undo_last(&mut self) -> io::Result<Option<UndoEntry>> {
        self.undo.undo_last(&self.root)
    }

    /// Replaces the undo journal wholesale — the `/resume` path (ADR §6.10):
    /// session replay rebuilds the journal from the `.diffs.jsonl` sidecar and
    /// installs it so `/undo` works on a continued session. Replacing (never
    /// appending) keeps the stack consistent with the replayed events.
    pub fn install_undo(&mut self, entries: Vec<UndoEntry>) {
        self.undo = UndoStack::from_entries(entries);
    }

    /// Full-file write (§6.4 `write` tool): journal first, then write — the
    /// same undo mechanism as SEARCH/REPLACE applies. There is no content
    /// matching here; the read-before-edit rule for *existing* files is
    /// enforced by the caller (the tool registry) because a blind overwrite
    /// is destructive. Returns whether the file already existed.
    ///
    /// Like [`Editor::apply_parsed`], a successful write credits the ledger:
    /// the written content is exactly what the model had in context.
    pub fn write_file(&mut self, rel: &str, content: &str) -> io::Result<bool> {
        let rel = crate::ledger::confine(Path::new(rel)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{rel} is outside the repository; use a repo-relative path"),
            )
        })?;
        if !crate::ledger::contains_path(&self.root, &rel) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} resolves outside the repository through a symbolic link; \
                     use a path that stays inside it",
                    rel.display()
                ),
            ));
        }
        let abs = self.root.join(&rel);
        let existed = abs.try_exists().map_err(io::Error::other)?;
        let before = if existed {
            let bytes = fs::read(&abs)?;
            String::from_utf8(bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not valid UTF-8", abs.display()),
                )
            })?
        } else {
            String::new()
        };
        self.undo.push(UndoEntry {
            path: rel.clone(),
            existed,
            before,
            after: content.to_owned(),
        });
        if let Err(err) = guarded(&self.root, &rel, Mutation::Write(content)) {
            self.undo.pop();
            return Err(err);
        }
        self.ledger.record_read(&rel);
        Ok(existed)
    }

    /// Parse a full model response and apply everything actionable in
    /// document order (§6.1 turn lifecycle step 2).
    pub fn apply_response(&mut self, text: &str) -> ApplyReport {
        self.apply_parsed(crate::parser::parse_response(text))
    }

    /// Apply an already-parsed response.
    pub fn apply_parsed(&mut self, parsed: ParsedResponse) -> ApplyReport {
        let ParsedResponse {
            blocks,
            commands,
            mut notes,
        } = parsed;
        let mut report = ApplyReport {
            commands,
            notes: Vec::new(),
            feedback: None,
            ..ApplyReport::default()
        };
        // The file named by the most recent block that named one — chained
        // blocks and continuation blocks reuse it (§6.3 rule 3).
        let mut continuation: Option<PathBuf> = None;

        for block in blocks {
            let named = block
                .candidates
                .first()
                .cloned()
                .unwrap_or_else(|| "(no filename)".to_owned());
            match self.resolve(&block, continuation.as_deref()) {
                Resolved::Path(rel) => {
                    if !block.candidates.is_empty() {
                        continuation = Some(rel.clone());
                    }
                    // Read-before-edit (§6.3): auto-inject unread files
                    // once, notify, and apply. The notification *is* the
                    // corrective cue for the next turn. Files that do not
                    // exist yet (creates) have nothing to read.
                    if !self.ledger.has_read(&rel) && self.root.join(&rel).exists() {
                        self.ledger.record_read(&rel);
                        notes.push(format!(
                            "{} was not read this session — it has been read (auto-injected) \
                             before applying your edit. Read files before editing them.",
                            rel.display()
                        ));
                    }
                    match self.apply_block(&block, &rel) {
                        Ok(applied) => report.applied.push(applied),
                        Err(reason) => {
                            if reason == FailureReason::NoMatch
                                && self.try_cross_file(&block, &rel, &mut report)
                            {
                                notes.push(format!(
                                    "The block did not match {}; it was applied in a different \
                                     file instead (see the applied list).",
                                    rel.display()
                                ));
                            } else {
                                report.failed.push(FailedBlock {
                                    block,
                                    path: rel.display().to_string(),
                                    reason,
                                });
                            }
                        }
                    }
                }
                Resolved::Outside(path) => {
                    report.failed.push(FailedBlock {
                        block,
                        path: path.clone(),
                        reason: FailureReason::OutsideRoot(path),
                    });
                }
                Resolved::Missing => {
                    report.failed.push(FailedBlock {
                        block,
                        path: named,
                        reason: FailureReason::MissingFilename,
                    });
                }
            }
        }

        report.notes = notes;
        report.feedback = build_failure_feedback(&report.failed, &report.applied, &self.root);
        report
    }

    /// Apply one block against `rel` (journal first, then write).
    fn apply_block(
        &mut self,
        block: &EditBlock,
        rel: &Path,
    ) -> Result<AppliedBlock, FailureReason> {
        // §6.12 confinement, filesystem-level: `confine` already rejected
        // `..` and absolute spellings, but a symlink inside the repo still
        // resolves outside it.
        if !crate::ledger::contains_path(&self.root, rel) {
            return Err(FailureReason::OutsideRoot(rel.display().to_string()));
        }
        let abs = self.root.join(rel);
        let existed = abs
            .try_exists()
            .map_err(|e| FailureReason::Io(e.to_string()))?;
        let content = if existed {
            read_file_utf8(&abs)?
        } else {
            String::new()
        };
        let Some(new_content) = do_replace(&content, &block.original, &block.updated) else {
            return Err(FailureReason::NoMatch);
        };
        // Journal before the write; roll the entry back if the write fails.
        self.undo.push(UndoEntry {
            path: rel.to_path_buf(),
            existed,
            before: content,
            after: new_content.clone(),
        });
        if let Err(err) = guarded(&self.root, rel, Mutation::Write(&new_content)) {
            self.undo.pop();
            return Err(FailureReason::Io(err.to_string()));
        }
        self.ledger.record_read(rel);
        Ok(AppliedBlock {
            path: rel.to_path_buf(),
            created: !existed,
            appended: block.is_new_file() && existed,
            cross_file: false,
        })
    }

    /// Cross-file retry (§6.3 rule 5): try the block against every file in
    /// the session read-set, in deterministic order. Only for blocks with a
    /// non-empty SEARCH (an append/create must not land in a random file).
    /// On success the edit is journaled and written there.
    fn try_cross_file(
        &mut self,
        block: &EditBlock,
        named: &Path,
        report: &mut ApplyReport,
    ) -> bool {
        if block.is_new_file() {
            return false;
        }
        let candidates: Vec<PathBuf> = self
            .ledger
            .read_set()
            .filter(|p| *p != named)
            .cloned()
            .collect();
        for cand in candidates {
            if !crate::ledger::contains_path(&self.root, &cand) {
                continue; // a linked-out read-set entry is never a write target
            }
            let abs = self.root.join(&cand);
            let Ok(content) = read_file_utf8(&abs) else {
                continue;
            };
            let Some(new_content) = do_replace(&content, &block.original, &block.updated) else {
                continue;
            };
            self.undo.push(UndoEntry {
                path: cand.clone(),
                existed: true,
                before: content,
                after: new_content.clone(),
            });
            if guarded(&self.root, &cand, Mutation::Write(&new_content)).is_err() {
                self.undo.pop();
                continue;
            }
            report.applied.push(AppliedBlock {
                path: cand,
                created: false,
                appended: false,
                cross_file: true,
            });
            return true;
        }
        false
    }

    /// Filename resolution (§6.3 rule 3): exact path → basename → fuzzy
    /// (similarity ≥ 0.8) → first candidate containing a dot; with no
    /// candidates at all, the previous named file (continuation).
    ///
    /// Whatever the strategy produces is then put through [`confine`]: a
    /// resolved path that escapes the workspace root is refused, never
    /// written. Only the last strategy can realistically produce one (it
    /// trusts a raw model-supplied string), but the fence sits on the single
    /// exit so no future strategy can bypass it.
    fn resolve(&self, block: &EditBlock, continuation: Option<&Path>) -> Resolved {
        let Some(chosen) = self.resolve_raw(block, continuation) else {
            return Resolved::Missing;
        };
        match crate::ledger::confine(&chosen) {
            Some(rel) => Resolved::Path(rel),
            None => Resolved::Outside(chosen.display().to_string()),
        }
    }

    /// The §6.3 rule 3 strategies, before confinement.
    fn resolve_raw(&self, block: &EditBlock, continuation: Option<&Path>) -> Option<PathBuf> {
        let set: Vec<PathBuf> = self.ledger.read_set().cloned().collect();
        for cand in &block.candidates {
            // An unconfined candidate is skipped, not fatal: a later
            // candidate (or a later strategy) may still resolve legally.
            // The chosen path is fenced again on `resolve`'s single exit.
            let Some(path) = crate::ledger::confine(Path::new(cand)) else {
                continue;
            };
            if set.contains(&path) {
                return Some(path);
            }
        }
        for cand in &block.candidates {
            let name = Path::new(cand).file_name();
            for file in &set {
                if file.file_name() == name {
                    return Some(file.clone());
                }
            }
        }
        for cand in &block.candidates {
            for file in &set {
                if similarity(cand, &file.to_string_lossy()) >= 0.8 {
                    return Some(file.clone());
                }
            }
        }
        for cand in &block.candidates {
            if cand.contains('.') {
                return Some(PathBuf::from(cand));
            }
        }
        if block.candidates.is_empty() {
            if let Some(prev) = continuation {
                return Some(prev.to_path_buf());
            }
        }
        None
    }
}

enum Resolved {
    Path(PathBuf),
    /// The resolved path escapes the workspace root; carries its display form.
    Outside(String),
    Missing,
}

/// Read a file, requiring valid UTF-8 with CRLF normalized. Rusta edits
/// text; binary or non-UTF-8 files fail with a clear reason instead of
/// corrupting them.
fn read_file_utf8(abs: &Path) -> Result<String, FailureReason> {
    let bytes = fs::read(abs).map_err(|e| FailureReason::Io(e.to_string()))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| FailureReason::Io(format!("{} is not valid UTF-8", abs.display())))?;
    Ok(normalize_crlf(text))
}

fn normalize_crlf(text: String) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n")
    } else {
        text
    }
}

/// Aider's `do_replace`: fence-stripped SEARCH/REPLACE applied to `content`.
/// An empty (whitespace-only) SEARCH creates a missing file or appends to
/// an existing one; everything else goes through the match chain.
fn do_replace(content: &str, original: &str, updated: &str) -> Option<String> {
    let before = strip_fence_pair(original);
    let after = strip_fence_pair(updated);
    if before.trim().is_empty() {
        // New file when the caller passes empty content; append otherwise.
        return Some(format!("{content}{after}"));
    }
    replace_most_similar_chunk(content, &before, &after)
}

/// Strip one stray wrapping fence pair (Aider's `strip_quoted_wrapping`):
/// when the section starts *and* ends with a ```` ``` ```` line, both are
/// dropped. Unpaired fences are kept — they may be real content.
fn strip_fence_pair(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let wrapped = lines.first().is_some_and(|l| l.starts_with("```"))
        && lines.last().is_some_and(|l| l.starts_with("```"));
    let kept: &[&str] = if wrapped && lines.len() > 2 {
        &lines[1..lines.len() - 1]
    } else if wrapped {
        &[]
    } else {
        &lines
    };
    let mut out = kept.concat();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Aider's `prep`: ensure a trailing newline, then split keeping endings.
fn prep(text: &str) -> (String, Vec<String>) {
    let mut owned = text.to_owned();
    if !owned.is_empty() && !owned.ends_with('\n') {
        owned.push('\n');
    }
    let lines: Vec<String> = owned.split_inclusive('\n').map(str::to_owned).collect();
    (owned, lines)
}

/// Strategy 1 + 2 (Aider's `perfect_or_whitespace`).
fn perfect_or_whitespace(
    whole_lines: &[String],
    part_lines: &[String],
    replace_lines: &[String],
) -> Option<String> {
    perfect_replace(whole_lines, part_lines, replace_lines).or_else(|| {
        replace_part_with_missing_leading_whitespace(whole_lines, part_lines, replace_lines)
    })
}

/// Exact line-sequence match (Aider's `perfect_replace`).
fn perfect_replace(
    whole_lines: &[String],
    part_lines: &[String],
    replace_lines: &[String],
) -> Option<String> {
    let n = part_lines.len();
    if n == 0 || whole_lines.len() < n {
        return None;
    }
    for i in 0..=(whole_lines.len() - n) {
        if whole_lines[i..i + n].iter().eq(part_lines) {
            return Some(
                whole_lines[..i]
                    .iter()
                    .chain(replace_lines)
                    .chain(&whole_lines[i + n..])
                    .map(String::as_str)
                    .collect(),
            );
        }
    }
    None
}

/// Whitespace-flexible match (Aider's
/// `replace_part_with_missing_leading_whitespace`): outdent SEARCH and
/// REPLACE by their common minimum, then locate a window that matches
/// except for uniformly added leading whitespace; the replacement keeps
/// the file's actual indentation.
///
/// Deviation from Aider (byte-safety): the added prefix is taken as each
/// whole line's own leading-whitespace run and must be identical across
/// the window, instead of a raw byte-length difference — same behavior for
/// the ASCII indentation that occurs in practice, panic-free for exotic
/// Unicode whitespace.
fn replace_part_with_missing_leading_whitespace(
    whole_lines: &[String],
    part_lines: &[String],
    replace_lines: &[String],
) -> Option<String> {
    let mut part = part_lines.to_vec();
    let mut replace = replace_lines.to_vec();
    let leading: Vec<usize> = part
        .iter()
        .chain(&replace)
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .collect();
    if let Some(&min) = leading.iter().min() {
        if min > 0 {
            for line in &mut part {
                if !line.trim().is_empty() {
                    *line = line.chars().skip(min).collect();
                }
            }
            for line in &mut replace {
                if !line.trim().is_empty() {
                    *line = line.chars().skip(min).collect();
                }
            }
        }
    }

    let n = part.len();
    if n == 0 || whole_lines.len() < n {
        return None;
    }
    for i in 0..=(whole_lines.len() - n) {
        let window = &whole_lines[i..i + n];
        let Some(prefix) = match_but_for_leading_whitespace(window, &part) else {
            continue;
        };
        let replaced: Vec<String> = replace
            .iter()
            .map(|r| {
                if r.trim().is_empty() {
                    r.clone()
                } else {
                    format!("{prefix}{r}")
                }
            })
            .collect();
        return Some(
            whole_lines[..i]
                .iter()
                .chain(&replaced)
                .chain(&whole_lines[i + n..])
                .map(String::as_str)
                .collect(),
        );
    }
    None
}

/// Aider's `match_but_for_leading_whitespace`: every line matches after
/// `trim_start`, and the per-line added prefix — `w[..len(w)-len(p)]` — is
/// identical across all non-blank lines (returned as the common prefix).
/// The only deviation is a char-boundary guard on the prefix slice: exotic
/// non-ASCII leading whitespace that would slice mid-character simply fails
/// to match instead of panicking.
fn match_but_for_leading_whitespace(window: &[String], part: &[String]) -> Option<String> {
    for (w, p) in window.iter().zip(part) {
        if w.trim_start() != p.trim_start() {
            return None;
        }
    }
    let prefixes: Option<Vec<&str>> = window
        .iter()
        .zip(part)
        .filter(|(w, _)| !w.trim().is_empty())
        .map(|(w, p)| {
            let diff = w
                .len()
                .checked_sub(p.len())
                .filter(|&d| w.is_char_boundary(d))?;
            Some(&w[..diff])
        })
        .collect();
    let prefixes = prefixes?;
    let first = prefixes.first()?;
    if prefixes.iter().all(|p| *p == *first) {
        Some((*first).to_owned())
    } else {
        None
    }
}

/// The full chain minus cross-file retry (Aider's
/// `replace_most_similar_chunk` with the edit-distance tail *removed*).
fn replace_most_similar_chunk(whole: &str, part: &str, replace: &str) -> Option<String> {
    let (whole_text, whole_lines) = prep(whole);
    let (part_text, part_lines) = prep(part);
    // `replace` is prepped too (Aider reassigns it before `try_dotdotdots`):
    // a REPLACE piece without a trailing newline would otherwise be spliced
    // onto the following line.
    let (replace_text, replace_lines) = prep(replace);

    if let Some(result) = perfect_or_whitespace(&whole_lines, &part_lines, &replace_lines) {
        return Some(result);
    }
    // Retry after dropping a spurious leading blank SEARCH line (Aider #25).
    if part_lines.len() > 2 && part_lines[0].trim().is_empty() {
        if let Some(result) = perfect_or_whitespace(&whole_lines, &part_lines[1..], &replace_lines)
        {
            return Some(result);
        }
    }
    try_dotdotdots(&whole_text, &part_text, &replace_text)
}

/// `...` elision handling (Aider's `try_dotdotdots`): split SEARCH and
/// REPLACE on standalone `...` lines; piece counts must pair, all `...`
/// separator pieces must be identical on both sides, then each content
/// piece pair is applied by exact substring match. A piece must occur
/// exactly once (ambiguous ⇒ fall through to failure). An empty SEARCH
/// piece with non-empty REPLACE piece appends. Any mismatch returns `None`
/// so the apply chain falls through to cross-file retry / failure.
///
/// A block with **no** `...` lines is not this strategy's business: Aider
/// bails out (`if len(part_pieces) == 1: return`) and so must this port.
/// Without that guard the single-piece path degenerates into a raw
/// substring replace that ignores line boundaries — a wrong-guess apply of
/// exactly the kind §6.3's DECIDED clause rules out.
fn try_dotdotdots(whole: &str, part: &str, replace: &str) -> Option<String> {
    let part_pieces = split_on_dot_lines(part);
    let replace_pieces = split_on_dot_lines(replace);

    if part_pieces.len() == 1 {
        return None; // no `...` in this block — not our strategy
    }
    if part_pieces.len() != replace_pieces.len() {
        return None;
    }
    // Odd pieces are the `...` separators; they must match exactly.
    for k in (1..part_pieces.len()).step_by(2) {
        if part_pieces[k] != replace_pieces[k] {
            return None;
        }
    }

    let mut result = whole.to_owned();
    for k in (0..part_pieces.len()).step_by(2) {
        let piece = &part_pieces[k];
        let replacement = &replace_pieces[k];
        if piece.is_empty() && replacement.is_empty() {
            continue;
        }
        if piece.is_empty() {
            // Pure insertion: append the replacement piece.
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(replacement);
            continue;
        }
        let occurrences = result.matches(piece.as_str()).count();
        if occurrences != 1 {
            return None; // absent or ambiguous — no guessing (DECIDED)
        }
        let start = result.find(piece.as_str())?; // count checked above
        result.replace_range(start..start + piece.len(), replacement);
    }
    Some(result)
}

/// Split into alternating (content, `...` separator) pieces, mirroring
/// Aider's `re.split(r"(^\s*\.\.\.\n)")`: *each* `...` line becomes its own
/// separator piece; a separator is a full line that is only dots and ends
/// with a newline. A trailing `...` without a newline stays content,
/// exactly like the reference.
fn split_on_dot_lines(text: &str) -> Vec<String> {
    // Leading content piece (empty when the text starts with a separator).
    let mut pieces: Vec<String> = vec![String::new()];
    let mut in_dots = false;
    for line in text.split_inclusive('\n') {
        let is_dot = line.ends_with('\n') && line.trim() == "...";
        if is_dot {
            if in_dots {
                // Consecutive separators: an empty content piece between.
                pieces.push(String::new());
            }
            pieces.push(String::new());
            if let Some(current) = pieces.last_mut() {
                current.push_str(line);
            }
            in_dots = true;
        } else {
            if in_dots {
                pieces.push(String::new());
                in_dots = false;
            }
            if let Some(current) = pieces.last_mut() {
                current.push_str(line);
            }
        }
    }
    // Trailing content piece after a final separator (re.split parity).
    if in_dots {
        pieces.push(String::new());
    }
    pieces
}

/// The failure-feedback contract (§6.3 rule 6, Aider's format verbatim).
/// Returned to the model as the next observation — this *is* the repair
/// loop; `None` means every block applied.
fn build_failure_feedback(
    failed: &[FailedBlock],
    applied: &[AppliedBlock],
    root: &Path,
) -> Option<String> {
    if failed.is_empty() {
        return None;
    }
    let mut res = format!(
        "# {} SEARCH/REPLACE {} failed to match!\n",
        failed.len(),
        block_word(failed.len())
    );
    for failure in failed {
        match &failure.reason {
            FailureReason::NoMatch => {
                res += &format!(
                    "\n## SearchReplaceNoExactMatch: This SEARCH block failed to exactly match lines in {}\n<<<<<<< SEARCH\n{}=======\n{}>>>>>>> REPLACE\n\n",
                    failure.path, failure.block.original, failure.block.updated
                );
                let content = read_file_lossy(&root.join(&failure.path));
                if let Some(content) = &content {
                    if let Some(window) = best_window(&failure.block.original, content) {
                        res += &format!(
                            "Did you mean to match some of these actual lines from {}?\n\n```\n{}\n```\n\n",
                            failure.path, window
                        );
                    }
                    if !failure.block.updated.trim().is_empty()
                        && content.contains(&failure.block.updated)
                    {
                        res += &format!(
                            "Are you sure you need this SEARCH/REPLACE block?\nThe REPLACE lines are already in {}!\n\n",
                            failure.path
                        );
                    }
                }
                res += "The SEARCH section must exactly match an existing block of lines including all white space, comments, indentation, docstrings, etc\n";
            }
            FailureReason::MissingFilename => {
                res += &format!(
                    "\n## MissingFilename: no filename found for a SEARCH/REPLACE block\n<<<<<<< SEARCH\n{}=======\n{}>>>>>>> REPLACE\nPut the filename alone on its own line directly above `<<<<<<< SEARCH`, like this:\n\n```\nsrc/main.rs\n<<<<<<< SEARCH\nexisting lines\n=======\nreplacement lines\n>>>>>>> REPLACE\n```\n",
                    failure.block.original, failure.block.updated
                );
            }
            FailureReason::OutsideRoot(path) => {
                res += &format!(
                    "\n## OutsideRepo: {path} is outside the repository and was not written\nEvery edit must target a path inside the repo, written relative to its root (no leading `/`, no `..`).\nName the file as e.g. `src/main.rs` on its own line directly above `<<<<<<< SEARCH`.\n"
                );
            }
            FailureReason::Io(err) => {
                res += &format!(
                    "\n## IOError: could not read or write {}\n{}\n",
                    failure.path, err
                );
            }
        }
    }
    if !applied.is_empty() {
        res += &format!(
            "\n# The other {} SEARCH/REPLACE {} were applied successfully.\nDon't re-send them.\nJust reply with fixed versions of the {} above that failed to match.\n",
            applied.len(),
            block_word(applied.len()),
            block_word(failed.len())
        );
    }
    Some(res)
}

fn read_file_lossy(abs: &Path) -> Option<String> {
    let bytes = fs::read(abs).ok()?;
    Some(normalize_crlf(String::from_utf8_lossy(&bytes).into_owned()))
}

fn block_word(n: usize) -> &'static str {
    if n == 1 { "block" } else { "blocks" }
}

/// Best matching window of file lines for a failed SEARCH (Aider's
/// `find_similar_lines`, threshold 0.6): slide a window the size of the
/// SEARCH over the file lines, and when the first/last lines already agree
/// return the window bare — otherwise pad ±5 lines of context.
///
/// Deviation, deliberate: Aider scores each window with difflib's
/// `SequenceMatcher.ratio`; this port scores aligned-line equality
/// (`equal lines / n`). The equality score is the exactly-matched-line
/// component of the ratio — deterministic and allocation-free — and the
/// 0.6 threshold plus the §7 corpus and snapshot tests pin the chosen
/// behavior; a character-level matcher would re-bless all of that for
/// near-miss cases the corpus does not exercise.
fn best_window(search: &str, content: &str) -> Option<String> {
    let search_lines: Vec<&str> = search.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();
    let n = search_lines.len();
    if n == 0 || content_lines.len() < n {
        return None;
    }
    let mut best_ratio = 0.0f64;
    let mut best_start = 0usize;
    for i in 0..=(content_lines.len() - n) {
        let equal = content_lines[i..i + n]
            .iter()
            .zip(&search_lines)
            .filter(|(a, b)| a == b)
            .count();
        let ratio = equal as f64 / n as f64;
        if ratio > best_ratio {
            best_ratio = ratio;
            best_start = i;
        }
    }
    if best_ratio < 0.6 {
        return None;
    }
    let window = &content_lines[best_start..best_start + n];
    if window.first() == search_lines.first() && window.last() == search_lines.last() {
        return Some(window.join("\n"));
    }
    let start = best_start.saturating_sub(5);
    let end = (best_start + n + 5).min(content_lines.len());
    Some(content_lines[start..end].join("\n"))
}

/// Normalized similarity in `[0, 1]` (1 - levenshtein/max_len) — a
/// deterministic, allocation-light stand-in for difflib's ratio used for
/// fuzzy filename resolution (threshold 0.8).
fn similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return f64::from(a == b);
    }
    let distance = levenshtein(a, b);
    1.0 - distance as f64 / a.chars().count().max(b.chars().count()) as f64
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, content).expect("write fixture");
    }

    fn read_file(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).expect("read result")
    }

    /// A canonical one-block response naming `rel`.
    fn response(rel: &str, original: &str, updated: &str) -> String {
        format!("{rel}\n<<<<<<< SEARCH\n{original}=======\n{updated}>>>>>>> REPLACE\n")
    }

    #[test]
    fn from_entries_rebuilds_a_working_journal() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "src/lib.rs", "final\n");
        let entries = vec![UndoEntry {
            path: PathBuf::from("src/lib.rs"),
            existed: true,
            before: "initial\n".into(),
            after: "final\n".into(),
        }];
        let mut journal = UndoStack::from_entries(entries);
        assert_eq!(journal.pending().len(), 1);
        let undone = journal.undo_last(root).expect("undo").expect("entry");
        assert_eq!(undone.before, "initial\n");
        assert_eq!(read_file(root, "src/lib.rs"), "initial\n");
        assert!(journal.is_empty());
    }

    #[test]
    fn write_file_creates_journals_and_undo_removes() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        let mut editor = Editor::new(root);

        let existed = editor
            .write_file("docs/new/notes.md", "hello\n")
            .expect("write");
        assert!(!existed);
        assert_eq!(read_file(root, "docs/new/notes.md"), "hello\n");
        assert_eq!(editor.undo_stack().len(), 1);
        // The write credits the ledger: the content is in the model's context.
        assert!(editor.ledger().has_read(Path::new("docs/new/notes.md")));

        let undone = editor.undo_last().expect("undo").expect("entry");
        assert!(!undone.existed);
        assert!(!root.join("docs/new/notes.md").exists());
    }

    #[test]
    fn write_file_overwrite_journals_previous_content() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "src/lib.rs", "original\n");
        let mut editor = Editor::new(root);

        let existed = editor
            .write_file("src/lib.rs", "replaced\n")
            .expect("write");
        assert!(existed);
        assert_eq!(read_file(root, "src/lib.rs"), "replaced\n");

        editor.undo_last().expect("undo");
        assert_eq!(read_file(root, "src/lib.rs"), "original\n");
    }

    #[test]
    fn write_file_rejects_non_utf8_target() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "blob.bin", "");
        fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00]).expect("binary fixture");
        let mut editor = Editor::new(root);

        let err = editor
            .write_file("blob.bin", "text")
            .expect_err("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // The refused write must not leave a journal entry behind.
        assert!(editor.undo_stack().is_empty());
        assert_eq!(
            fs::read(root.join("blob.bin")).expect("untouched"),
            vec![0xff, 0xfe, 0x00]
        );
    }

    #[test]
    fn exact_match_applies_and_undo_restores() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "src/main.rs", "fn main() {\n    old();\n}\n");
        let mut editor = Editor::new(root);
        editor.record_read("src/main.rs");

        let report = editor.apply_response(&response(
            "src/main.rs",
            "fn main() {\n    old();\n}\n",
            "fn main() {\n    new();\n}\n",
        ));
        assert!(report.is_success());
        assert_eq!(report.applied.len(), 1);
        assert_eq!(
            read_file(root, "src/main.rs"),
            "fn main() {\n    new();\n}\n"
        );
        assert_eq!(editor.undo_stack().len(), 1);

        let undone = editor.undo_last().expect("undo").expect("entry");
        assert_eq!(undone.path, PathBuf::from("src/main.rs"));
        assert_eq!(
            read_file(root, "src/main.rs"),
            "fn main() {\n    old();\n}\n"
        );
        assert!(editor.undo_last().expect("undo").is_none());
    }

    #[test]
    fn empty_search_creates_file_and_undo_removes_it() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        let mut editor = Editor::new(root);

        let report =
            editor.apply_response("docs/new.md\n<<<<<<< SEARCH\n=======\n# Hi\n>>>>>>> REPLACE\n");
        assert!(report.is_success());
        assert_eq!(report.applied.len(), 1);
        assert!(report.applied[0].created);
        assert_eq!(read_file(root, "docs/new.md"), "# Hi\n");

        let undone = editor.undo_last().expect("undo").expect("entry");
        assert!(!undone.existed);
        assert!(!root.join("docs/new.md").exists());
    }

    #[test]
    fn empty_search_appends_to_existing_file() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "a.txt", "one\n");
        let mut editor = Editor::new(root);
        editor.record_read("a.txt");

        let report =
            editor.apply_response("a.txt\n<<<<<<< SEARCH\n=======\ntwo\n>>>>>>> REPLACE\n");
        assert!(report.is_success());
        assert!(report.applied[0].appended);
        assert_eq!(read_file(root, "a.txt"), "one\ntwo\n");
    }

    #[test]
    fn whitespace_flexible_match_keeps_file_indentation() {
        // Case 1: SEARCH names a nested region with its relative indentation
        // but not the enclosing block's uniform prefix — Aider's classic
        // artifact. The replacement keeps the file's actual indentation.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(
            root,
            "lib.rs",
            "fn outer() {\n    fn a() {\n        inner();\n    }\n}\n",
        );
        let mut editor = Editor::new(root);
        editor.record_read("lib.rs");

        let report = editor.apply_response(&response(
            "lib.rs",
            "fn a() {\n    inner();\n}\n",
            "fn a() {\n    fixed();\n}\n",
        ));
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert_eq!(
            read_file(root, "lib.rs"),
            "fn outer() {\n    fn a() {\n        fixed();\n    }\n}\n"
        );

        // Case 2: SEARCH uniformly over-indented relative to the file — the
        // common minimum outdent makes it match exactly.
        write_file(root, "flat.rs", "one\ntwo\nthree\n");
        editor.record_read("flat.rs");
        let report = editor.apply_response(&response(
            "flat.rs",
            "    two\n    three\n",
            "    TWO\n    THREE\n",
        ));
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert_eq!(read_file(root, "flat.rs"), "one\nTWO\nTHREE\n");
    }

    #[test]
    fn spurious_leading_blank_search_line_is_retried() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "b.rs", "one\ntwo\nthree\n");
        let mut editor = Editor::new(root);
        editor.record_read("b.rs");

        let report = editor.apply_response(&response("b.rs", "\ntwo\nthree\n", "TWO\nTHREE\n"));
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert_eq!(read_file(root, "b.rs"), "one\nTWO\nTHREE\n");
    }

    #[test]
    fn dotdotdots_pieces_apply_by_exact_match() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        let file =
            "fn start() {\n    a();\n}\n\nfn middle() {\n    b();\n}\n\nfn end() {\n    c();\n}\n";
        write_file(root, "big.rs", file);
        let mut editor = Editor::new(root);
        editor.record_read("big.rs");

        let search = "fn start() {\n...\nfn end() {\n    c();\n}\n";
        let replace = "fn start() {\n...\nfn end() {\n    fixed();\n}\n";
        let report = editor.apply_response(&response("big.rs", search, replace));
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert_eq!(
            read_file(root, "big.rs"),
            "fn start() {\n    a();\n}\n\nfn middle() {\n    b();\n}\n\nfn end() {\n    fixed();\n}\n"
        );
    }

    #[test]
    fn dotdotdots_empty_search_piece_appends() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "c.rs", "tail();\n");
        let mut editor = Editor::new(root);
        editor.record_read("c.rs");

        // Piece pairing gives SEARCH an empty middle content piece while
        // REPLACE inserts text there — Aider's append branch.
        let report = editor.apply_response(&response("c.rs", "...\n...\n", "...\nextra();\n...\n"));
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert_eq!(read_file(root, "c.rs"), "tail();\nextra();\n");
    }

    #[test]
    fn dotdotdots_unpaired_pieces_fail() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "d.rs", "a\nb\nc\n");
        let mut editor = Editor::new(root);
        editor.record_read("d.rs");

        // Two `...` in SEARCH, one in REPLACE ⇒ unpaired ⇒ NoMatch.
        let report = editor.apply_response(&response("d.rs", "a\n...\n...\nc\n", "a\n...\nc\n"));
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].reason, FailureReason::NoMatch);
        assert_eq!(read_file(root, "d.rs"), "a\nb\nc\n");
    }

    #[test]
    fn dotdotdots_ambiguous_piece_fails() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "e.rs", "dup()\ndup()\n");
        let mut editor = Editor::new(root);
        editor.record_read("e.rs");

        // The piece `dup()\n` occurs twice ⇒ ambiguous ⇒ no guessing.
        let report = editor.apply_response(&response("e.rs", "dup()\n...\n", "fixed()\n...\n"));
        assert_eq!(report.failed.len(), 1);
        assert_eq!(read_file(root, "e.rs"), "dup()\ndup()\n");
    }

    #[test]
    fn dotdotdots_mismatched_separator_lines_fail() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "f.rs", "a\nb\nc\n");
        let mut editor = Editor::new(root);
        editor.record_read("f.rs");

        let report = editor.apply_response(&response("f.rs", "a\n...  \nb\n", "a\n...\nx\n"));
        assert_eq!(report.failed.len(), 1);
    }

    #[test]
    fn resolution_exact_basename_and_fuzzy() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "src/deep/lib.rs", "a\n");
        write_file(root, "other.rs", "target\n");
        let mut editor = Editor::new(root);
        editor.record_read("src/deep/lib.rs");
        editor.record_read("other.rs");

        // Exact match wins over everything.
        let report = editor.apply_response(&response("other.rs", "target\n", "done\n"));
        assert!(report.is_success());
        assert_eq!(report.applied[0].path, PathBuf::from("other.rs"));

        // Fuzzy: one typo'd character still resolves (similarity ≥ 0.8).
        write_file(root, "named.rs", "n\n");
        editor.record_read("named.rs");
        let report = editor.apply_response(&response("namd.rs", "n\n", "fuzzy\n"));
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert_eq!(report.applied[0].path, PathBuf::from("named.rs"));
        assert_eq!(read_file(root, "named.rs"), "fuzzy\n");
    }

    #[test]
    fn basename_resolution_matches_read_set_files() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "src/deep/lib.rs", "a\n");
        let mut editor = Editor::new(root);
        editor.record_read("src/deep/lib.rs");

        // A bare basename resolves to the read file with that name.
        let report = editor.apply_response(&response("lib.rs", "a\n", "b\n"));
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert_eq!(report.applied[0].path, PathBuf::from("src/deep/lib.rs"));
        assert_eq!(read_file(root, "src/deep/lib.rs"), "b\n");
    }

    #[test]
    fn unknown_dot_path_with_nonempty_search_fails_safely() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        let mut editor = Editor::new(root);

        // Dot fallback names a brand-new path, but a non-empty SEARCH
        // against a missing file can never match — it must fail, not create.
        let report = editor.apply_response(&response("brand/new/file.rs", "old\n", "new\n"));
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].reason, FailureReason::NoMatch);
        assert!(!root.join("brand/new/file.rs").exists());
    }

    #[test]
    fn continuation_reuses_previous_named_file() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "cont.rs", "one\ntwo\n");
        let mut editor = Editor::new(root);
        editor.record_read("cont.rs");

        let text = "cont.rs\n<<<<<<< SEARCH\none\n=======\nONE\n>>>>>>> REPLACE\n\n<<<<<<< SEARCH\ntwo\n=======\nTWO\n>>>>>>> REPLACE\n";
        let report = editor.apply_response(text);
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert_eq!(report.applied.len(), 2);
        assert_eq!(read_file(root, "cont.rs"), "ONE\nTWO\n");
    }

    #[test]
    fn cross_file_retry_applies_in_read_set_file() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "wrong.rs", "unrelated\n");
        write_file(root, "right.rs", "needle {\n    found();\n}\n");
        let mut editor = Editor::new(root);
        editor.record_read("wrong.rs");
        editor.record_read("right.rs");

        // The block names wrong.rs but its SEARCH only exists in right.rs.
        let report = editor.apply_response(&response(
            "wrong.rs",
            "needle {\n    found();\n}\n",
            "needle {\n    relocated();\n}\n",
        ));
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert_eq!(report.applied.len(), 1);
        assert!(report.applied[0].cross_file);
        assert_eq!(report.applied[0].path, PathBuf::from("right.rs"));
        assert_eq!(
            read_file(root, "right.rs"),
            "needle {\n    relocated();\n}\n"
        );
        assert_eq!(read_file(root, "wrong.rs"), "unrelated\n");
        assert!(report.notes.iter().any(|n| n.contains("wrong.rs")));
    }

    #[test]
    fn read_before_edit_auto_injects_and_notifies() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "fresh.rs", "v1\n");
        let mut editor = Editor::new(root);
        // Note: no record_read — the block itself must trigger auto-inject.

        let report = editor.apply_response(&response("fresh.rs", "v1\n", "v2\n"));
        assert!(report.is_success(), "feedback: {:?}", report.feedback);
        assert!(editor.ledger().has_read(Path::new("fresh.rs")));
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("fresh.rs") && n.contains("auto-injected"))
        );
        assert_eq!(read_file(root, "fresh.rs"), "v2\n");
    }

    #[test]
    fn missing_filename_fails_with_corrective_feedback() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "x.rs", "x\n");
        let mut editor = Editor::new(root);
        editor.record_read("x.rs");

        let report = editor.apply_response(&response("no-such-candidate", "x\n", "y\n"));
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].reason, FailureReason::MissingFilename);
        let feedback = report.feedback.as_deref().expect("feedback");
        assert!(feedback.contains("MissingFilename"));
        assert!(feedback.contains("```"));
        assert_eq!(read_file(root, "x.rs"), "x\n");
    }

    #[test]
    fn io_error_surfaces_as_failed_block() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("blocker.rs")).expect("directory fixture");
        let mut editor = Editor::new(root);
        editor.record_read("blocker.rs");

        // `blocker.rs` is a directory — every read fails with Io.
        let report = editor.apply_response(&response("blocker.rs", "a\n", "b\n"));
        assert_eq!(report.failed.len(), 1);
        assert!(matches!(report.failed[0].reason, FailureReason::Io(_)));
        assert!(
            report
                .feedback
                .as_deref()
                .is_some_and(|f| f.contains("IOError"))
        );
    }

    #[test]
    fn multi_block_response_applies_in_document_order() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "m1.rs", "a\nb\n");
        write_file(root, "m2.rs", "c\nd\n");
        let mut editor = Editor::new(root);
        editor.record_read("m1.rs");
        editor.record_read("m2.rs");

        let text = "m1.rs\n<<<<<<< SEARCH\na\n=======\nA\n>>>>>>> REPLACE\nm2.rs\n<<<<<<< SEARCH\nd\n=======\nD\n>>>>>>> REPLACE\n";
        let report = editor.apply_response(text);
        assert!(report.is_success());
        assert_eq!(report.applied.len(), 2);
        assert_eq!(read_file(root, "m1.rs"), "A\nb\n");
        assert_eq!(read_file(root, "m2.rs"), "c\nD\n");
        assert_eq!(editor.undo_stack().len(), 2);
    }

    #[test]
    fn similarity_and_window_helpers() {
        assert_eq!(similarity("a.rs", "a.rs"), 1.0);
        assert!(similarity("named.rs", "namd.rs") >= 0.8);
        assert!(similarity("a.rs", "zzzzzz.rs") < 0.8);

        let content = "one\ntwo\nthree\n";
        assert_eq!(
            best_window("one\nTWO\nthree\n", content).as_deref(),
            Some("one\ntwo\nthree")
        );
        // Best window crosses the 0.6 threshold but its first line differs
        // from the SEARCH ⇒ context is padded (±5 lines, clamped).
        assert_eq!(
            best_window("X\ntwo\nthree\nfour\n", "zero\none\ntwo\nthree\nfour\n").as_deref(),
            Some("zero\none\ntwo\nthree\nfour")
        );
        assert_eq!(best_window("zzz\n", content), None);
        assert_eq!(best_window("", content), None);
    }

    #[test]
    fn feedback_snapshots() {
        // (1) Near miss ⇒ "Did you mean" window (ratio ≥ 0.6, first/last
        // lines agree ⇒ bare window).
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "snap1.rs", "l1\nl2\nl3\n");
        let mut editor = Editor::new(root);
        editor.record_read("snap1.rs");
        let report =
            editor.apply_response(&response("snap1.rs", "l1\nXX\nl3\n", "l1\nfixed\nl3\n"));
        insta::assert_snapshot!(
            "feedback_did_you_mean",
            report.feedback.as_deref().unwrap_or("<none>")
        );

        // (2) No similar window at all ⇒ no "Did you mean" section.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "snap2.rs", "alpha\nbeta\n");
        let mut editor = Editor::new(root);
        editor.record_read("snap2.rs");
        let report = editor.apply_response(&response("snap2.rs", "qqq\nrrr\n", "sss\n"));
        insta::assert_snapshot!(
            "feedback_no_window",
            report.feedback.as_deref().unwrap_or("<none>")
        );

        // (3) REPLACE already present in the file ⇒ "Are you sure" hint.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "snap3.rs", "a\nb\nc\n");
        let mut editor = Editor::new(root);
        editor.record_read("snap3.rs");
        let report = editor.apply_response(&response("snap3.rs", "x\ny\n", "b\nc\n"));
        insta::assert_snapshot!(
            "feedback_already_present",
            report.feedback.as_deref().unwrap_or("<none>")
        );

        // (4) One block applies, one fails ⇒ the "other blocks" footer.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "snap4.rs", "keep\nmiss\n");
        let mut editor = Editor::new(root);
        editor.record_read("snap4.rs");
        let text = "snap4.rs\n<<<<<<< SEARCH\nkeep\n=======\nKEPT\n>>>>>>> REPLACE\nsnap4.rs\n<<<<<<< SEARCH\nnope\n=======\nNADA\n>>>>>>> REPLACE\n";
        let report = editor.apply_response(text);
        insta::assert_snapshot!(
            "feedback_partial_success",
            report.feedback.as_deref().unwrap_or("<none>")
        );

        // (5) Missing filename ⇒ corrective section with a fence example.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "snap5.rs", "z\n");
        let mut editor = Editor::new(root);
        editor.record_read("snap5.rs");
        let report = editor.apply_response(&response("mystery", "z\n", "w\n"));
        insta::assert_snapshot!(
            "feedback_missing_filename",
            report.feedback.as_deref().unwrap_or("<none>")
        );
    }

    #[test]
    fn no_dots_block_never_degrades_to_a_substring_splice() {
        // Aider's `try_dotdotdots` bails out when there are no `...` pieces.
        // Without that guard a mid-line SEARCH fragment gets spliced into the
        // first matching position and reported as a clean apply.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "a.rs", "fn main() {\n    let x = compute(a, b);\n}\n");
        let mut editor = Editor::new(root);
        editor.record_read("a.rs");

        let report =
            editor.apply_response(&response("a.rs", "compute(a, b);\n", "compute(b, a);\n"));
        assert!(report.applied.is_empty(), "{:?}", report.applied);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].reason, FailureReason::NoMatch);
        // The file is untouched: a clear retry request beats a wrong guess.
        assert_eq!(
            read_file(root, "a.rs"),
            "fn main() {\n    let x = compute(a, b);\n}\n"
        );
    }

    #[test]
    fn dotdotdots_still_applies_when_dots_are_present() {
        // The guard must not disable the real `...` strategy.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "a.rs", "start\nA\nmid\nB\nend\n");
        let mut editor = Editor::new(root);
        editor.record_read("a.rs");

        let report = editor.apply_response(&response("a.rs", "A\n...\nB\n", "AA\n...\nBB\n"));
        assert!(report.is_success(), "{:?}", report.failed);
        assert_eq!(read_file(root, "a.rs"), "start\nAA\nmid\nBB\nend\n");
    }

    #[test]
    fn dotdotdots_replace_tail_keeps_its_line_ending() {
        // Aider preps `replace` before splitting; an unterminated final piece
        // must not be glued onto the following line.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "a.rs", "start\nA\nmid\nB\nend\n");
        let mut editor = Editor::new(root);
        editor.record_read("a.rs");

        let block = EditBlock {
            candidates: vec!["a.rs".to_owned()],
            original: "A\n...\nB\n".to_owned(),
            updated: "AA\n...\nBB".to_owned(), // no trailing newline
        };
        let report = editor.apply_parsed(ParsedResponse {
            blocks: vec![block],
            commands: Vec::new(),
            notes: Vec::new(),
        });
        assert!(report.is_success(), "{:?}", report.failed);
        assert_eq!(read_file(root, "a.rs"), "start\nAA\nmid\nBB\nend\n");
    }

    #[test]
    fn edits_cannot_escape_the_workspace_root() {
        // §6.12 confines every mutation to the repo. The text SEARCH/REPLACE
        // pathway must honour the same fence as the `edit`/`write` tools.
        let base = TempDir::new().expect("tempdir");
        let root = base.path().join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        let outside = base.path().join("outside.txt");

        for name in [
            outside.display().to_string(), // absolute
            "../outside.txt".to_owned(),   // parent traversal
        ] {
            let mut editor = Editor::new(&root);
            let report = editor.apply_response(&response(&name, "", "PWNED\n"));
            assert!(report.applied.is_empty(), "{name}: {:?}", report.applied);
            assert_eq!(report.failed.len(), 1, "{name}");
            assert!(
                matches!(report.failed[0].reason, FailureReason::OutsideRoot(_)),
                "{name}: {:?}",
                report.failed[0].reason
            );
            let feedback = report.feedback.expect("feedback");
            assert!(feedback.contains("OutsideRepo"), "{feedback}");
            assert!(!outside.exists(), "{name} escaped the root");
            assert!(!base.path().join("repo/../outside.txt").exists());
        }
    }

    #[test]
    fn cross_file_retry_cannot_escape_the_root() {
        // §6.3 step 5 writes to whatever the read-set holds, so the fence
        // has to sit on the ledger, not only on filename resolution. A
        // resumed session can carry an absolute path into the ledger — logs
        // written before the fence existed contain them.
        let base = TempDir::new().expect("tempdir");
        let root = base.path().join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        let outside = base.path().join("outside.txt");
        std::fs::write(&outside, "SECRET = 1\n").expect("fixture");
        write_file(&root, "in.rs", "fn a() {}\n");

        let mut editor = Editor::new(&root);
        editor.record_read("in.rs");
        editor.record_read(&outside.display().to_string());

        // SEARCH matches the outside file, not the named one: without the
        // fence, cross-file retry applied it there.
        let report = editor.apply_response(&response("in.rs", "SECRET = 1\n", "SECRET = 666\n"));
        assert!(report.applied.is_empty(), "{:?}", report.applied);
        assert_eq!(
            std::fs::read_to_string(&outside).expect("outside"),
            "SECRET = 1\n",
            "cross-file retry wrote outside the repo root"
        );
    }

    #[test]
    fn write_file_is_confined_too() {
        // `write_file` is `pub`; a caller's `safe_rel` is not this crate's
        // guarantee, so the fence belongs here as well.
        let base = TempDir::new().expect("tempdir");
        let root = base.path().join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        let mut editor = Editor::new(&root);

        for outside in [
            base.path().join("via_write.txt").display().to_string(),
            "../via_parent.txt".to_owned(),
        ] {
            let err = editor
                .write_file(&outside, "PWNED\n")
                .expect_err("must refuse a path outside the repo");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        }
        assert!(!base.path().join("via_write.txt").exists());
        assert!(!base.path().join("via_parent.txt").exists());
        assert!(editor.undo_stack().is_empty(), "a refusal must not journal");
    }

    #[test]
    fn ordinary_relative_paths_still_resolve_through_the_fence() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        let mut editor = Editor::new(root);
        // `./` spelling is normalized, not rejected.
        let report = editor.apply_response(&response("./docs/notes.md", "", "hi\n"));
        assert!(report.is_success(), "{:?}", report.failed);
        assert_eq!(read_file(root, "docs/notes.md"), "hi\n");
    }

    #[test]
    fn install_undo_restores_a_journal_and_replaces_the_old_one() {
        // The `/resume` path: a journal rebuilt from the session sidecar is
        // installed wholesale, and undoing works against the live root.
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        write_file(root, "src/lib.rs", "fn after() {}\n");
        std::fs::create_dir_all(root.join("src")).expect("mkdir");

        let mut editor = Editor::new(root);
        editor.install_undo(vec![UndoEntry {
            path: PathBuf::from("src/lib.rs"),
            existed: true,
            before: "fn before() {}\n".to_owned(),
            after: "fn after() {}\n".to_owned(),
        }]);

        let entry = editor.undo_last().expect("io").expect("entry");
        assert_eq!(entry.before, "fn before() {}\n");
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).expect("read"),
            "fn before() {}\n"
        );
        assert!(editor.undo_stack().is_empty());
    }
}
