//! The `glob` tool — §6.4: matching path list — and the glob matcher shared
//! with `grep`'s path filter.
//!
//! The matcher supports the shell subset models reliably produce: `*`
//! (within one segment), `**` (across segments, gitignore-style, may match
//! zero), `?`, and `[abc]`/`[a-z]`/`[!...]` classes. An unclosed `[` is a
//! literal; matching never fails, never panics, and stays lean — no glob
//! crate (§10).

use std::path::Path;

use serde_json::Value;

use crate::exec::{ToolOutcome, caps, req_nonempty};
use crate::search::{display, effective_pattern, walk};

/// Match `path` (forward slashes, repo-relative) against `pattern`.
///
/// Shared with the CLI's `/add` and `/drop` (§6.9) so the two never drift —
/// and so both inherit the memoized matcher rather than a second copy of
/// the exponential one.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    // `seen[p * (path.len() + 1) + t]` — segment-level states already proven
    // unmatchable, the same memo `match_segment` keeps one level down.
    // Without it stacked `**`s explore C(n+k, k) skip combinations: twenty of
    // them against a twelve-segment path took 43 s — once per file walked.
    let mut seen = vec![false; (pattern.len() + 1) * (path.len() + 1)];
    segments(&pattern, &path, 0, 0, &mut seen)
}

/// Execute `glob(pattern)` against `root`: paths in sorted order, capped at
/// [`caps::GLOB_ENTRIES`]. A pattern without `/` matches at any depth.
pub(crate) fn glob(root: &Path, input: &Value) -> ToolOutcome {
    let pattern = match req_nonempty(input, "pattern") {
        Ok(pattern) => pattern,
        Err(outcome) => return outcome,
    };
    let effective = effective_pattern(pattern);

    let mut paths: Vec<String> = Vec::new();
    let mut truncated = false;
    for rel in walk(root) {
        let shown = display(&rel);
        if glob_match(&effective, &shown) {
            if paths.len() >= caps::GLOB_ENTRIES {
                truncated = true;
                break;
            }
            paths.push(shown);
        }
    }

    if paths.is_empty() {
        return ToolOutcome::ok(format!("(no paths match {pattern:?})"));
    }
    let mut content = paths.join("\n");
    if truncated {
        content.push_str(&format!(
            "\n[... more paths truncated (cap: {})]",
            caps::GLOB_ENTRIES
        ));
    }
    ToolOutcome {
        status: rusta_core::Status::Ok,
        content,
        truncated,
        read_credit: None,
    }
}

/// True when `pattern[p..]` matches `path[t..]`. `seen` marks `(p, t)` pairs
/// already shown not to match, which bounds the whole walk at
/// O(pattern × path) with no change in the accepted language.
fn segments(pattern: &[&str], path: &[&str], p: usize, t: usize, seen: &mut [bool]) -> bool {
    let key = p * (path.len() + 1) + t;
    if seen[key] {
        return false;
    }
    let matched = segments_uncached(pattern, path, p, t, seen);
    if !matched {
        seen[key] = true;
    }
    matched
}

fn segments_uncached(
    pattern: &[&str],
    path: &[&str],
    p: usize,
    t: usize,
    seen: &mut [bool],
) -> bool {
    if p == pattern.len() {
        return t == path.len();
    }
    if pattern[p] == "**" {
        // `**` swallows zero or more whole segments.
        for skip in t..=path.len() {
            if segments(pattern, path, p + 1, skip, seen) {
                return true;
            }
        }
        return false;
    }
    t < path.len()
        && match_segment(pattern[p], path[t])
        && segments(pattern, path, p + 1, t + 1, seen)
}

fn match_segment(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    // `seen[p * (text.len() + 1) + t]` — states already proven unmatchable.
    // Patterns come from the model, and naive `*` backtracking is
    // exponential: `*a*a*a*a*a*a*a*b` against a 44-character name took ~5 s,
    // and two more groups did not finish. Memoization makes the worst case
    // O(pattern × text) with no change in accepted language.
    let mut seen = vec![false; (pattern.len() + 1) * (text.len() + 1)];
    segment(&pattern, &text, 0, 0, &mut seen)
}

/// True when `pattern[p..]` matches `text[t..]`. `seen` marks `(p, t)` pairs
/// already shown not to match.
fn segment(pattern: &[char], text: &[char], p: usize, t: usize, seen: &mut [bool]) -> bool {
    let stride = text.len() + 1;
    let key = p * stride + t;
    if seen[key] {
        return false;
    }
    let matched = segment_uncached(pattern, text, p, t, seen);
    if !matched {
        seen[key] = true;
    }
    matched
}

fn segment_uncached(
    pattern: &[char],
    text: &[char],
    p: usize,
    t: usize,
    seen: &mut [bool],
) -> bool {
    if p == pattern.len() {
        return t == text.len();
    }
    let at_end = t == text.len();
    match pattern[p] {
        '*' => {
            segment(pattern, text, p + 1, t, seen)
                || (!at_end && segment(pattern, text, p, t + 1, seen))
        }
        '?' => !at_end && segment(pattern, text, p + 1, t + 1, seen),
        '[' => match class_end(&pattern[p..]) {
            None => !at_end && text[t] == '[' && segment(pattern, text, p + 1, t + 1, seen),
            Some((negated, end)) => {
                if at_end {
                    return false;
                }
                let inside = class_match(&pattern[p + 1..p + end], text[t]);
                (inside != negated) && segment(pattern, text, p + end + 1, t + 1, seen)
            }
        },
        literal => !at_end && text[t] == literal && segment(pattern, text, p + 1, t + 1, seen),
    }
}

/// Parse a `[...]` class starting at `pattern[0] == '['`; returns
/// `(negated, index_of_closing_bracket)`. A `]` first in the body is a
/// literal member; no closing bracket ⇒ `None` (the `[` is literal).
fn class_end(pattern: &[char]) -> Option<(bool, usize)> {
    let mut i = 1;
    let negated = matches!(pattern.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    if matches!(pattern.get(i), Some(']')) {
        i += 1;
    }
    while i < pattern.len() {
        if pattern[i] == ']' {
            return Some((negated, i));
        }
        i += 1;
    }
    None
}

/// Membership in the class body (between the brackets), with `a-z` ranges.
fn class_match(body: &[char], c: char) -> bool {
    let mut i = 0;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == '-' {
            if c >= body[i] && c <= body[i + 2] {
                return true;
            }
            i += 3;
        } else {
            if body[i] == c {
                return true;
            }
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_within_segment_and_any_depth_without_slash() {
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "main.rs"));
        assert!(!glob_match("**/*.rs", "src/main.rs.bak"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/ui/panel.rs"));
    }

    #[test]
    fn double_star_matches_zero_segments() {
        assert!(glob_match("a/**/b.rs", "a/b.rs"));
        assert!(glob_match("a/**/b.rs", "a/x/y/b.rs"));
        assert!(!glob_match("a/**/b.rs", "x/a/b.rs"));
    }

    #[test]
    fn question_mark_and_classes() {
        assert!(glob_match("a?c.rs", "abc.rs"));
        assert!(!glob_match("a?c.rs", "ac.rs"));
        assert!(glob_match("[abc].rs", "b.rs"));
        assert!(!glob_match("[abc].rs", "d.rs"));
        assert!(glob_match("[a-z][0-9].rs", "x7.rs"));
        assert!(glob_match("[!a].rs", "b.rs"));
        assert!(!glob_match("[!a].rs", "a.rs"));
    }

    #[test]
    fn unclosed_class_is_literal() {
        assert!(glob_match("a[.rs", "a[.rs"));
        assert!(!glob_match("a[.rs", "ab.rs"));
    }

    #[test]
    fn exact_paths_match_exactly() {
        assert!(glob_match("src/main.rs", "src/main.rs"));
        assert!(!glob_match("src/main.rs", "src/main.rs.orig"));
    }
}
