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
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    match_segments(&pattern, &path)
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

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        // `**` swallows zero or more whole segments.
        return (0..=path.len()).any(|skip| match_segments(&pattern[1..], &path[skip..]));
    }
    if path.is_empty() {
        return false;
    }
    match_segment(pattern[0], path[0]) && match_segments(&pattern[1..], &path[1..])
}

fn match_segment(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    segment(&pattern, &text)
}

fn segment(pattern: &[char], text: &[char]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    match pattern[0] {
        '*' => segment(&pattern[1..], text) || (!text.is_empty() && segment(pattern, &text[1..])),
        '?' => !text.is_empty() && segment(&pattern[1..], &text[1..]),
        '[' => match class_end(pattern) {
            None => !text.is_empty() && text[0] == '[' && segment(&pattern[1..], &text[1..]),
            Some((negated, end)) => {
                if text.is_empty() {
                    return false;
                }
                let inside = class_match(&pattern[1..end], text[0]);
                (inside != negated) && segment(&pattern[end + 1..], &text[1..])
            }
        },
        literal => !text.is_empty() && text[0] == literal && segment(&pattern[1..], &text[1..]),
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
