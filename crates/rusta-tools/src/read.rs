//! The `read` tool — §6.4: numbered file slice, capped per §6.1.

use std::path::Path;

use serde_json::Value;

use crate::exec::{ToolOutcome, caps, opt_usize, req_nonempty, safe_rel};

/// Execute `read(path, from?, to?)` against `root`.
pub(crate) fn read(root: &Path, input: &Value) -> ToolOutcome {
    run(root, input).unwrap_or_else(|outcome| outcome)
}

fn run(root: &Path, input: &Value) -> Result<ToolOutcome, ToolOutcome> {
    let raw = req_nonempty(input, "path")?;
    let rel = safe_rel(raw)?;
    let from = opt_usize(input, "from")?.unwrap_or(1).max(1);
    let to = opt_usize(input, "to")?;
    if let Some(to) = to {
        if to < from {
            return Err(ToolOutcome::error(format!(
                "\"to\" ({to}) must be ≥ \"from\" ({from}); both are 1-based inclusive"
            )));
        }
    }

    let bytes = match std::fs::read(root.join(&rel)) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolOutcome::error(format!(
                "{raw}: no such file — check the path (glob can find it)"
            )));
        }
        Err(err) => return Err(ToolOutcome::error(format!("{raw}: {err}"))),
    };
    if bytes.contains(&0) {
        return Err(ToolOutcome::error(format!(
            "{raw}: binary file (contains NUL bytes); read handles text files only"
        )));
    }

    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        let mut outcome = ToolOutcome::ok(format!("{raw}: (empty file)"));
        outcome.read_credit = Some(rel.display().to_string());
        return Ok(outcome);
    }

    let from = from.min(lines.len());
    let to = to.unwrap_or(from + caps::READ_LINES - 1).min(lines.len());
    let mut shown: Vec<String> = Vec::new();
    let mut bytes_used = 0usize;
    let mut last_line = from.saturating_sub(1); // 1-based last included line
    for (index, line) in lines.iter().enumerate().take(to).skip(from - 1) {
        let numbered = format!("{:>4}| {line}", index + 1);
        bytes_used += numbered.len() + 1;
        if shown.len() >= caps::READ_LINES || bytes_used > caps::READ_BYTES {
            break;
        }
        shown.push(numbered);
        last_line = index + 1;
    }

    let truncated = last_line < lines.len();
    let mut content = format!("{}:{}-{last_line}\n", rel.display(), from);
    content.push_str(&shown.join("\n"));
    if truncated {
        let remaining = lines.len() - last_line;
        content.push_str(&format!(
            "\n[... {remaining} more lines truncated (caps: {} lines / {} KiB)]",
            caps::READ_LINES,
            caps::READ_BYTES / 1024
        ));
    }
    Ok(ToolOutcome {
        status: rusta_core::Status::Ok,
        content,
        truncated,
        read_credit: Some(rel.display().to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn read_at(dir: &TempDir, input: serde_json::Value) -> ToolOutcome {
        read(dir.path(), &input)
    }

    fn dir_with(content: &str) -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), content).expect("fixture");
        dir
    }

    #[test]
    fn reads_numbered_slice_and_credits_the_ledger_path() {
        let dir = dir_with("one\ntwo\nthree\n");
        let outcome = read_at(&dir, json!({"path": "a.rs"}));
        assert_eq!(outcome.status, rusta_core::Status::Ok);
        assert_eq!(
            outcome.content,
            "a.rs:1-3\n   1| one\n   2| two\n   3| three"
        );
        assert!(!outcome.truncated);
        assert_eq!(outcome.read_credit.as_deref(), Some("a.rs"));
    }

    #[test]
    fn from_to_window_and_clamping() {
        let dir = dir_with(&"line\n".repeat(50));
        let outcome = read_at(&dir, json!({"path": "a.rs", "from": 10, "to": 12}));
        assert!(outcome.content.starts_with("a.rs:10-12\n  10| line"));
        assert!(outcome.content.contains("  12| line"));
        assert!(!outcome.content.contains("  13|"));
        // Window past EOF clamps.
        let clamped = read_at(&dir, json!({"path": "a.rs", "from": 49, "to": 500}));
        assert!(clamped.content.starts_with("a.rs:49-50\n"));
        // Reversed windows are refused with a remedy.
        let reversed = read_at(&dir, json!({"path": "a.rs", "from": 5, "to": 2}));
        assert_eq!(reversed.status, rusta_core::Status::Error);
        assert!(reversed.content.contains("must be ≥"));
    }

    #[test]
    fn caps_marker_reports_remaining_lines() {
        let dir = dir_with(&"filler\n".repeat(3_000));
        let outcome = read_at(&dir, json!({"path": "a.rs"}));
        assert!(outcome.truncated);
        // header + 2000 numbered lines + truncation marker.
        assert_eq!(outcome.content.lines().count(), 2_002);
        assert!(outcome.content.contains("[... 1000 more lines truncated"));
    }

    #[test]
    fn binary_missing_and_traversal_errors_carry_remedies() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("blob.bin"), [0x00, 0x01]).expect("bin");

        let missing = read_at(&dir, json!({"path": "nope.rs"}));
        assert_eq!(missing.status, rusta_core::Status::Error);
        assert!(missing.content.contains("no such file"));

        let binary = read_at(&dir, json!({"path": "blob.bin"}));
        assert_eq!(binary.status, rusta_core::Status::Error);
        assert!(binary.content.contains("binary file"));

        for bad in ["/etc/passwd", "../outside.rs"] {
            let blocked = read_at(&dir, json!({"path": bad}));
            assert_eq!(blocked.status, rusta_core::Status::Error, "{bad}");
            assert!(blocked.content.contains("repo-relative"), "{bad}");
        }

        let missing_key = read_at(&dir, json!({"from": 1}));
        assert!(
            missing_key
                .content
                .contains("missing required key \"path\"")
        );
    }

    #[test]
    fn empty_file_is_an_explicit_ok() {
        let dir = dir_with("");
        let outcome = read_at(&dir, json!({"path": "a.rs"}));
        assert_eq!(outcome.status, rusta_core::Status::Ok);
        assert_eq!(outcome.content, "a.rs: (empty file)");
        assert_eq!(outcome.read_credit.as_deref(), Some("a.rs"));
    }
}
