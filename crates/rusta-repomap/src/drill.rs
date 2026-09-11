//! `map_drill` — DEVELOPMENT_PLAN.md §6.5 step 8 (tool reference §6.4).
//!
//! Returns one definition's full span (`path` + `name`) or a line window
//! (`path` + `from`/`to`, 1-based inclusive) as a compact text block. The M7
//! tool handler wraps this and credits the read-before-edit ledger (§6.3);
//! this module stays free of session state.

use crate::lang::Lang;
use crate::tags::{TagKind, extract_tags};
use std::path::Path;

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
            let span = definition_span(root, path, name)?;
            ((*path).to_string(), span.0, span.1)
        }
    };

    let source = std::fs::read_to_string(root.join(&rel))
        .map_err(|_| DrillError::Unreadable(rel.clone()))?;
    let lines: Vec<&str> = source.lines().collect();
    let from = from.min(lines.len().max(1));
    let to = to.min(lines.len());
    let mut out = format!("{rel}:{from}-{to}\n");
    for line in lines.get(from.saturating_sub(1)..to).into_iter().flatten() {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// Resolve `name` to its 1-based inclusive definition span.
fn definition_span(root: &Path, rel: &str, name: &str) -> Result<(usize, usize), DrillError> {
    let abs = root.join(rel);
    let lang = Lang::from_path(&abs).ok_or_else(|| DrillError::Unreadable(rel.to_string()))?;
    let source =
        std::fs::read_to_string(&abs).map_err(|_| DrillError::Unreadable(rel.to_string()))?;
    let Some(tags) = extract_tags(&source, lang) else {
        return Err(DrillError::Unreadable(rel.to_string()));
    };
    tags.iter()
        .find(|t| t.kind == TagKind::Def && t.name == name)
        .map(|t| (t.line + 1, t.line_end + 1))
        .ok_or_else(|| DrillError::NotFound {
            path: rel.to_string(),
            name: name.to_string(),
        })
}
