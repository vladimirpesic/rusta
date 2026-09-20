//! `map_refresh` + `map_drill` — §6.4 wiring of the §6.5 repo map.
//!
//! `map_drill` credits the read-before-edit ledger (§6.5 step 8): the padded
//! span may be the model's whole view of a region before an edit.
//!
//! A definition drill may carry one appended line of resolved type
//! information when the opt-in `lsp` feature is built and enabled. The
//! coordinate for it comes from the repo map's own tree-sitter tag, never
//! from the model.

use std::path::Path;

use rusta_lsp::CodeIntel;
use rusta_repomap::{DrillError, DrillRequest, RepoMap};
use serde_json::Value;

use crate::exec::{ToolOutcome, caps, clip_bytes, opt_usize, req_nonempty, safe_rel_in};

/// Prefix marking an appended type annotation, chosen so it can never be
/// mistaken for a source line: the drill renders source unprefixed, and the
/// repo map's own gutter is `│`.
const TYPE_MARKER: &str = "⟪type⟫ ";

/// Clips a drilled span to the §6.1 `read` caps (2,000 lines / 64 KiB),
/// marking the cut so the model narrows its next drill instead of assuming
/// it saw the whole region.
///
/// The `path:from-to` header is **rewritten to the range actually returned**,
/// the way `read` reports its own last line. Capping the body while leaving
/// the header claiming `1-50000` told the model it had received 50,000 lines
/// when it held 2,000 — and this output is what a SEARCH block anchors on,
/// which is exactly the "actively misleading" failure the §6.5 elision
/// erratum was written about.
fn cap_window(text: &str) -> (String, bool) {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    let body: Vec<&str> = lines.collect();
    let kept = body.len().min(caps::READ_LINES);
    let joined = body[..kept].join("\n");
    let (mut content, clipped) = clip_bytes(
        &joined,
        caps::READ_BYTES,
        "\n[... truncated at the 64 KiB cap]",
    );
    // Whatever survived both caps is what the header must describe. The
    // byte-cap marker `clip_bytes` appends is not a content line; counting
    // it made the header claim one line more than it returned.
    let marker_lines = usize::from(clipped);
    let shown = content
        .lines()
        .count()
        .saturating_sub(marker_lines)
        .min(kept);
    let dropped = body.len() - shown;
    if dropped > 0 {
        content.push_str(&format!(
            "\n[... {dropped} more lines not shown (§6.1 caps: {} lines / {} KiB)]",
            caps::READ_LINES,
            caps::READ_BYTES / 1024
        ));
    }
    (
        format!("{}\n{content}", retitle(header, shown)),
        clipped || dropped > 0,
    )
}

/// Rewrites a `path:from-to` header so `to` names the last line actually
/// included. Anything not in that shape is passed through untouched.
fn retitle(header: &str, shown: usize) -> String {
    let Some((path, range)) = header.rsplit_once(':') else {
        return header.to_owned();
    };
    let Some((from, _)) = range.split_once('-') else {
        return header.to_owned();
    };
    let Ok(start) = from.parse::<usize>() else {
        return header.to_owned();
    };
    // `shown` lines starting at `start`, 1-based inclusive.
    format!("{path}:{start}-{}", start + shown.saturating_sub(1))
}

/// Shown when the map renders nothing. The old wording blamed the language
/// filter or a zero budget, which was wrong whenever fitting was the cause —
/// it told the model a repo full of source had none.
pub const EMPTY_MAP: &str = "(repo map is empty: no source files in the configured languages, or [repomap] max_tokens is 0)";

/// Identifiers a user message plausibly names, for §6.5's mention boosts.
///
/// §6.5 steps 3–4 give a user-mentioned identifier ×10 edge weight and
/// `+100/N` personalization. Every production caller passed `&[], &[]`, so
/// that half of the ranking spec was unreachable — the same shape as the
/// withdrawn `strict_grammar` key. The scan is deliberately the repo map's
/// own: ASCII identifier runs, no tokenizer, so it costs nothing and cannot
/// disagree with the tag vocabulary it is matched against. Short and
/// all-lowercase-common words are dropped: `the`/`fix`/`add` would boost
/// half the graph and steer nothing.
pub fn mentioned_identifiers(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' || ch == '/' {
            current.push(ch);
        } else if !current.is_empty() {
            push_mention(&mut out, std::mem::take(&mut current));
        }
    }
    push_mention(&mut out, current);
    out
}

/// Keeps a candidate when it could plausibly name code: a path-like token,
/// or an identifier long enough and shaped enough to be worth boosting.
fn push_mention(out: &mut Vec<String>, word: String) {
    let looks_pathy = word.contains('.') || word.contains('/');
    // §6.5's own ladder: snake, kebab or camel case.
    let shaped = word.contains('_')
        || word.contains('-')
        || (word.chars().any(|c| c.is_ascii_uppercase())
            && word.chars().any(|c| c.is_ascii_lowercase()));
    let worth_it = looks_pathy || (word.len() >= 4 && shaped);
    if worth_it && !out.contains(&word) {
        out.push(word);
    }
}

/// Execute `map_refresh()` — re-render the repo map.
///
/// Session chat-files (the ledger read-set) steer ranking but never render
/// (§6.5 step 5); `mentions` carries the §6.5 steps 3–4 user-mention boosts,
/// which no production caller used to supply.
pub(crate) fn refresh(
    map: &mut RepoMap,
    chat_files: &[String],
    mentions: &[String],
) -> ToolOutcome {
    // A mention that names a file steers personalization; the same token
    // also steers edge weight as an identifier, so both lists get it.
    let rendered = map.render_map(chat_files, None, mentions, mentions);
    if rendered.is_empty() {
        return ToolOutcome::ok(EMPTY_MAP);
    }
    ToolOutcome::ok(rendered)
}

/// Execute `map_drill(path, name | from, to)` against `root`.
///
/// `intel` annotates a *definition* drill with its resolved type signature
/// when enabled; it is never consulted for an explicit `from`/`to` window,
/// which names no definition to resolve.
pub(crate) async fn drill(root: &Path, input: &Value, intel: &CodeIntel) -> ToolOutcome {
    let raw = match req_nonempty(input, "path") {
        Ok(raw) => raw,
        Err(outcome) => return outcome,
    };
    let rel = match safe_rel_in(root, raw) {
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
    // The *validated* spelling is the one that gets used: `safe_rel` checks
    // `raw.trim()`, so passing `raw` on meant validated value ≠ used value.
    let checked = rel.display().to_string();
    let request = match (name, from, to) {
        (Some(name), None, None) => DrillRequest::Definition {
            path: &checked,
            name: name.trim(),
        },
        (None, Some(from), Some(to)) => DrillRequest::Window {
            path: &checked,
            from,
            to,
        },
        _ => {
            return ToolOutcome::error(
                "map_drill takes either {\"name\": \"identifier\"} or both \"from\" and \"to\" (1-based inclusive) — not both, not neither.",
            );
        }
    };

    // Derived from the request rather than re-testing the inputs, so the
    // enrichment path can never disagree with the drill about whether a
    // definition was named.
    let definition_name = match &request {
        DrillRequest::Definition { name, .. } => Some(*name),
        DrillRequest::Window { .. } => None,
    };

    match rusta_repomap::drill(root, request) {
        Ok(text) => {
            // §6.1 caps apply here exactly as they do to `read`. An explicit
            // from/to window is model-supplied and unbounded: drilling
            // 1..50000 of a large file returned 1.5 MB — ~24× the read cap —
            // straight into the context this project exists to protect. The
            // core prompt steers the model here ("prefer map_drill to
            // whole-file reads"), so this is the hot path, not the edge.
            let (mut content, truncated) = cap_window(&text);
            // Appended *after* the caps, deliberately. `cap_window` rewrites
            // the `path:from-to` header from the number of body lines it
            // returns, so an annotation added before it would inflate that
            // range by one and tell the model it had seen a source line that
            // does not exist — the same "header describes something other
            // than the content" defect the A34 comment in
            // `rusta_repomap::drill` was written about. The annotation is one
            // line clipped to the map's own width, so it cannot meaningfully
            // affect the §6.1 byte budget it sits outside.
            if let Some(name) = definition_name {
                append_type_annotation(root, &checked, name, intel, &mut content).await;
            }
            ToolOutcome {
                status: rusta_core::Status::Ok,
                content,
                truncated,
                read_credit: Some(rel.display().to_string()),
            }
        }
        Err(DrillError::NotFound { path, name }) => ToolOutcome::error(format!(
            "no definition named {name:?} in {path} — check the identifier in the repo map (map_refresh) or drill an exact window with from/to"
        )),
        Err(DrillError::BadWindow { from, to }) => ToolOutcome::error(format!(
            "window must satisfy 1 <= from <= to (got {from}..{to})"
        )),
        Err(DrillError::PastEof { path, from, lines }) => ToolOutcome::error(format!(
            "{path} has {lines} line(s); the window starting at {from} is past the end — read a window inside the file, or use map_drill with a definition name"
        )),
        Err(DrillError::Unreadable(path)) => ToolOutcome::error(format!(
            "{path}: unsupported language or unreadable file — use read for raw lines"
        )),
    }
}

/// Append `name`'s resolved type signature to `out`, if one is available.
///
/// Every step is allowed to yield nothing — the feature may be off, the
/// anchor may not resolve, the language server may be missing, indexing,
/// slow or dead — and in every one of those cases `out` is left exactly as it
/// was. A drill without a language server is byte-identical to a drill from a
/// build compiled without this crate's `lsp` feature.
async fn append_type_annotation(
    root: &Path,
    rel: &str,
    name: &str,
    intel: &CodeIntel,
    out: &mut String,
) {
    if !intel.is_enabled() {
        return;
    }
    // The scaffold supplies the coordinate. `map_drill` takes `path` + `name`
    // precisely so the model never has to produce a line and column, which
    // §17.1 records it getting wrong on its first real tool call.
    let Some(anchor) = rusta_repomap::definition_anchor(root, rel, name) else {
        return;
    };
    let Some(signature) = intel.signature(rel, anchor.line, anchor.character).await else {
        return;
    };
    out.push('\n');
    out.push_str(TYPE_MARKER);
    out.push_str(&signature.0);
}
