//! The `grep` tool — §6.4: case-sensitive regex matches as `file:line: text`
//! — plus the deterministic repo walker shared with `glob`.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use crate::exec::{ToolOutcome, caps, clip_chars, req_nonempty};
use crate::glob::glob_match;

/// Directories never descended into — §6.5 step 1's ignore set.
pub(crate) const SKIP_DIRS: [&str; 4] = [".git", "target", "node_modules", "dist"];

/// Files above this size are skipped (generated blobs, vendored dumps).
pub(crate) const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Sorted, repo-relative file paths under `root`, skipping `.git`,
/// `target`, `node_modules` and `dist`.
///
/// Shared with the CLI's `/add` and `/drop` (§6.9): one walker and one
/// ignore set, so the two agree about what the repo *contains*. They
/// deliberately differ on how a pattern is read — `glob` normalizes a
/// slash-free pattern to `**/pattern`, `/add` keeps shell/Aider path
/// semantics — which is documented at `walk_matching` in `commands.rs`.
pub fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dir(root, root, &mut out);
    out
}

fn walk_dir(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let name = entry.file_name();
            if SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            walk_dir(&path, root, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_path_buf());
        }
    }
}

/// Normalize a path filter the way the `glob` tool does: a pattern with no
/// `/` matches at any depth (gitignore-style).
pub fn effective_pattern(filter: &str) -> String {
    if filter.contains('/') {
        filter.to_owned()
    } else {
        format!("**/{filter}")
    }
}

/// Repo-relative display form (forward slashes).
pub fn display(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Execute `grep(pattern, glob?)` against `root`.
pub(crate) fn grep(root: &Path, input: &Value) -> ToolOutcome {
    let pattern = match req_nonempty(input, "pattern") {
        Ok(pattern) => pattern,
        Err(outcome) => return outcome,
    };
    let filter = input
        .get("glob")
        .and_then(Value::as_str)
        .filter(|f| !f.trim().is_empty())
        .map(effective_pattern);
    let regex = match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => {
            return ToolOutcome::error(format!(
                "invalid regex {pattern:?}: {err}. Use Rust regex syntax, e.g. \"fn \\w+\\(\""
            ));
        }
    };

    let mut matches: Vec<String> = Vec::new();
    let mut truncated = false;
    'search: for rel in walk(root) {
        if let Some(filter) = &filter {
            if !glob_match(filter, &display(&rel)) {
                continue;
            }
        }
        let abs = root.join(&rel);
        let Ok(meta) = fs::metadata(&abs) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(&abs) else {
            continue;
        };
        if bytes.contains(&0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&bytes);
        let rel = display(&rel);
        for (index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                // §6.1: a cap that truncated must say so. A silently
                // clipped line reads as a complete one, and a model
                // composing a SEARCH block from it would be wrong.
                let clipped = clip_chars(line, caps::LINE_CHARS);
                let marker = if clipped.len() < line.len() {
                    " […]"
                } else {
                    ""
                };
                matches.push(format!("{rel}:{}: {clipped}{marker}", index + 1));
                if matches.len() >= caps::GREP_MATCHES {
                    truncated = true;
                    break 'search;
                }
            }
        }
    }

    if matches.is_empty() {
        return ToolOutcome::ok(format!("(no matches for {pattern:?})"));
    }
    let mut content = matches.join("\n");
    if truncated {
        content.push_str(&format!(
            "\n[... more matches truncated (cap: {})]",
            caps::GREP_MATCHES
        ));
    }
    ToolOutcome {
        status: rusta_core::Status::Ok,
        content,
        truncated,
        read_credit: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
        std::fs::write(dir.path().join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").expect("a");
        std::fs::write(dir.path().join("src/b.rs"), "fn alpha_echo() {}\n").expect("b");
        std::fs::create_dir_all(dir.path().join("target")).expect("ignored");
        std::fs::write(dir.path().join("target/gen.rs"), "fn alpha_leak() {}\n").expect("gen");
        dir
    }

    #[test]
    fn matches_are_file_line_text_and_ignore_build_dirs() {
        let dir = fixture();
        let outcome = grep(dir.path(), &json!({"pattern": "fn alpha\\w*"}));
        assert_eq!(outcome.status, rusta_core::Status::Ok);
        assert_eq!(
            outcome.content,
            "src/a.rs:1: fn alpha() {}\nsrc/b.rs:1: fn alpha_echo() {}"
        );
        assert!(!outcome.truncated);
    }

    #[test]
    fn glob_filter_narrows_the_search() {
        let dir = fixture();
        let outcome = grep(dir.path(), &json!({"pattern": "alpha", "glob": "a.rs"}));
        assert_eq!(outcome.content, "src/a.rs:1: fn alpha() {}");
    }

    #[test]
    fn caps_at_200_matches_with_marker() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("big.rs"), "needle\n".repeat(500)).expect("big");
        let outcome = grep(dir.path(), &json!({"pattern": "needle"}));
        assert!(outcome.truncated);
        assert_eq!(outcome.content.lines().count(), 201);
        assert!(outcome.content.contains("more matches truncated"));
    }

    #[test]
    fn no_matches_invalid_regex_and_missing_pattern() {
        let dir = fixture();
        let none = grep(dir.path(), &json!({"pattern": "zzzz"}));
        assert_eq!(none.status, rusta_core::Status::Ok);
        assert_eq!(none.content, "(no matches for \"zzzz\")");

        let invalid = grep(dir.path(), &json!({"pattern": "(unclosed"}));
        assert_eq!(invalid.status, rusta_core::Status::Error);
        assert!(invalid.content.contains("invalid regex"));

        let missing = grep(dir.path(), &json!({}));
        assert!(missing.content.contains("missing required key \"pattern\""));
    }
}
