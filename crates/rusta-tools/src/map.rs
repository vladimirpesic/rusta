//! `map_refresh` + `map_drill` — §6.4 wiring of the §6.5 repo map.
//!
//! `map_drill` credits the read-before-edit ledger (§6.5 step 8): the padded
//! span may be the model's whole view of a region before an edit.

use std::path::Path;

use rusta_repomap::{DrillError, DrillRequest, RepoMap};
use serde_json::Value;

use crate::exec::{ToolOutcome, opt_usize, req_nonempty, safe_rel};

/// Shown when the map renders nothing. The old wording blamed the language
/// filter or a zero budget, which was wrong whenever fitting was the cause —
/// it told the model a repo full of source had none.
pub const EMPTY_MAP: &str = "(repo map is empty: no source files in the configured languages, or [repomap] max_tokens is 0)";

/// Execute `map_refresh()` — re-render the repo map. Session chat-files
/// (the ledger read-set) steer ranking but never render (§6.5 step 5).
pub(crate) fn refresh(map: &mut RepoMap, chat_files: &[String]) -> ToolOutcome {
    let rendered = map.render_map(chat_files, None, &[], &[]);
    if rendered.is_empty() {
        return ToolOutcome::ok(EMPTY_MAP);
    }
    ToolOutcome::ok(rendered)
}

/// Execute `map_drill(path, name | from, to)` against `root`.
pub(crate) fn drill(root: &Path, input: &Value) -> ToolOutcome {
    let raw = match req_nonempty(input, "path") {
        Ok(raw) => raw,
        Err(outcome) => return outcome,
    };
    let rel = match safe_rel(raw) {
        Ok(rel) => rel,
        Err(outcome) => return outcome,
    };
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty());
    let from = match opt_usize(input, "from") {
        Ok(from) => from,
        Err(outcome) => return outcome,
    };
    let to = match opt_usize(input, "to") {
        Ok(to) => to,
        Err(outcome) => return outcome,
    };
    let request = match (name, from, to) {
        (Some(name), None, None) => DrillRequest::Definition {
            path: raw,
            name: name.trim(),
        },
        (None, Some(from), Some(to)) => DrillRequest::Window {
            path: raw,
            from,
            to,
        },
        _ => {
            return ToolOutcome::error(
                "map_drill takes either {\"name\": \"identifier\"} or both \"from\" and \"to\" (1-based inclusive) — not both, not neither.",
            );
        }
    };

    match rusta_repomap::drill(root, request) {
        Ok(text) => ToolOutcome {
            status: rusta_core::Status::Ok,
            content: text,
            truncated: false,
            read_credit: Some(rel.display().to_string()),
        },
        Err(DrillError::NotFound { path, name }) => ToolOutcome::error(format!(
            "no definition named {name:?} in {path} — check the identifier in the repo map (map_refresh) or drill an exact window with from/to"
        )),
        Err(DrillError::BadWindow { from, to }) => ToolOutcome::error(format!(
            "window must satisfy 1 <= from <= to (got {from}..{to})"
        )),
        Err(DrillError::Unreadable(path)) => ToolOutcome::error(format!(
            "{path}: unsupported language or unreadable file — use read for raw lines"
        )),
    }
}
