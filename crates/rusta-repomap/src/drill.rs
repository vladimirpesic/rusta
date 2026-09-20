//! `map_drill` — ADR.md §6.5 step 8 (tool reference §6.4).
//!
//! Returns one definition's full span (`path` + `name`) or a line window
//! (`path` + `from`/`to`, 1-based inclusive) as a compact text block.
//! Definition spans are padded with ±8 context lines (§6.5 step 8, clamped
//! at file edges) so the model sees doc comments, attributes, and item
//! boundaries — the anchors a SEARCH block needs; the
//! ledger credit means this window may be the model's whole view of the
//! region before an edit. Explicit `Window` requests are the model's own
//! choice and are never padded. The M7 tool handler wraps this and credits
//! the read-before-edit ledger (§6.3); this module stays free of session
//! state.

use std::path::Path;

use crate::lang::Lang;
use crate::tags::{Tag, TagKind, extract_tags};

/// Context lines padded around a drilled definition span (§6.5 step 8
/// DECIDED). Wider than the map renderer's overview window on purpose: the
/// drill credits the read-before-edit ledger, so this may be the model's
/// whole view of the region, and the lines just outside a span — doc
/// comments, attributes, `impl` headers — are what a SEARCH block anchors on.
const CONTEXT_LINES: usize = 8;

/// What to drill: a definition by name, or a raw line window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrillRequest<'a> {
    /// The full span (signature through closing brace) of `name` in `path`.
    Definition { path: &'a str, name: &'a str },
    /// 1-based inclusive line window of `path`.
    Window {
        path: &'a str,
        from: usize,
        to: usize,
    },
}

/// Drill failures surfaced to the tool caller as retry-able errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DrillError {
    #[error("unsupported or unreadable file: {0}")]
    Unreadable(String),
    #[error("no definition named {name} in {path}")]
    NotFound { path: String, name: String },
    #[error("window must satisfy 1 <= from <= to (got {from}..{to})")]
    BadWindow { from: usize, to: usize },
    /// A window whose `from` is past the end of the file. Distinct from
    /// [`DrillError::BadWindow`]: the request is well-formed, the file is
    /// just shorter than it assumes.
    #[error("{path} has {lines} line(s); the window starts at {from}")]
    PastEof {
        path: String,
        from: usize,
        lines: usize,
    },
}

/// Drill `request` against the repo at `root`. Output format:
/// `path:from-to` header followed by the requested source lines.
pub fn drill(root: &Path, request: DrillRequest<'_>) -> Result<String, DrillError> {
    let (rel, from, to) = match &request {
        DrillRequest::Window { path, from, to } => {
            if *from == 0 || from > to {
                return Err(DrillError::BadWindow {
                    from: *from,
                    to: *to,
                });
            }
            ((*path).to_string(), *from, *to)
        }
        DrillRequest::Definition { path, name } => {
            let (from, to) = definition_span(root, path, name)?;
            // Clamped at the top here; the shared EOF clamp below handles
            // the bottom. `Window` requests are never padded.
            let from = from.saturating_sub(CONTEXT_LINES).max(1);
            ((*path).to_string(), from, to + CONTEXT_LINES)
        }
    };

    let source = std::fs::read_to_string(root.join(&rel))
        .map_err(|_| DrillError::Unreadable(rel.clone()))?;
    let lines: Vec<&str> = source.lines().collect();
    // A34, and the worse sibling found while fixing it: clamping `from` into
    // the file turned a window the caller never asked for into a successful
    // answer. `drill(a.rs, from: 10, to: 12)` on a three-line file returned
    // `Ok("a.rs:3-3\nthree\n")` — real content from a different region, and
    // `map_drill` credits the ledger for it, so the model believes it has
    // seen lines 10-12. That is the round-6 `read` regression exactly:
    // silently wrong content is worse than an error, because a SEARCH block
    // gets anchored on it. An empty file is the same bug at zero length,
    // where the header read `path:1-0` — a range naming no line at all.
    //
    // Partial overlap is *not* an error: `from: 2, to: 100` on three lines
    // still answers `a.rs:2-3`, because the caller did ask for line 2.
    if from > lines.len() {
        return Err(DrillError::PastEof {
            path: rel.clone(),
            from,
            lines: lines.len(),
        });
    }
    let to = to.min(lines.len());
    let mut out = format!("{rel}:{from}-{to}\n");
    for line in lines.get(from.saturating_sub(1)..to).into_iter().flatten() {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// Read `rel` and resolve `name` to its definition tag, returning the file
/// text alongside it.
///
/// Shared by [`definition_span`] and [`definition_anchor`] so the drilled span
/// and the enrichment anchor can never disagree about which definition a name
/// resolves to — they are the same tag, found once.
fn definition_tag(root: &Path, rel: &str, name: &str) -> Result<(String, Tag), DrillError> {
    let abs = root.join(rel);
    let lang = Lang::from_path(&abs).ok_or_else(|| DrillError::Unreadable(rel.to_string()))?;
    let source =
        std::fs::read_to_string(&abs).map_err(|_| DrillError::Unreadable(rel.to_string()))?;
    let Some(tags) = extract_tags(&source, lang) else {
        return Err(DrillError::Unreadable(rel.to_string()));
    };
    let tag = tags
        .into_iter()
        .find(|t| t.kind == TagKind::Def && t.name == name)
        .ok_or_else(|| DrillError::NotFound {
            path: rel.to_string(),
            name: name.to_string(),
        })?;
    Ok((source, tag))
}

/// Resolve `name` to its 1-based inclusive definition span.
fn definition_span(root: &Path, rel: &str, name: &str) -> Result<(usize, usize), DrillError> {
    let (_, tag) = definition_tag(root, rel, name)?;
    Ok((tag.line + 1, tag.line_end + 1))
}

/// A definition identifier's position, in the units the LSP boundary wants:
/// **1-based line, 1-based UTF-16 code-unit column**.
///
/// The scaffold produces these from its own tree-sitter tags, so a model never
/// supplies a coordinate. That is the whole reason position-anchored code
/// intelligence is usable here at all: ADR §17.1 records a real run in which
/// `qwen2.5-coder:7b` passed an identifier where a line number was specified,
/// and `map_drill` takes `path` + `name` precisely so it never has to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in UTF-16 code units.
    pub character: u32,
}

/// Resolve `name`'s definition in `rel` to an [`Anchor`].
///
/// Returns `None` for every failure — unreadable file, unsupported language,
/// no such definition, a column that is not a `char` boundary, or a file large
/// enough to overflow `u32`. Callers treat `None` as "no enrichment available",
/// never as an error: this feeds a best-effort annotation whose absence must be
/// indistinguishable from the feature being switched off.
///
/// # Units
///
/// Tree-sitter reports a 0-based **byte** column; the LSP boundary defines
/// character offsets in **UTF-16 code units** and is 1-based on the MCP side.
/// Both conversions happen here, in the one place that owns them.
#[must_use]
pub fn definition_anchor(root: &Path, rel: &str, name: &str) -> Option<Anchor> {
    let (source, tag) = definition_tag(root, rel, name).ok()?;
    let line_text = source.lines().nth(tag.line)?;
    // `get` yields `None` when `col` is not a char boundary, which degrades to
    // "no enrichment" instead of slicing a position into the middle of a
    // multi-byte character.
    let utf16_col = line_text.get(..tag.col)?.encode_utf16().count();
    Some(Anchor {
        line: u32::try_from(tag.line).ok()?.checked_add(1)?,
        character: u32::try_from(utf16_col).ok()?.checked_add(1)?,
    })
}
