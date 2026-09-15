//! The `read` tool — §6.4: numbered file slice, capped per §6.1.

use std::io::BufRead as _;
use std::path::Path;

use serde_json::Value;

use crate::exec::{ToolOutcome, caps, opt_usize, req_nonempty, safe_rel_in};

/// Execute `read(path, from?, to?)` against `root`.
pub(crate) fn read(root: &Path, input: &Value) -> ToolOutcome {
    run(root, input).unwrap_or_else(|outcome| outcome)
}

fn run(root: &Path, input: &Value) -> Result<ToolOutcome, ToolOutcome> {
    let raw = req_nonempty(input, "path")?;
    let rel = safe_rel_in(root, raw)?;
    let from = opt_usize(input, "from")?.unwrap_or(1).max(1);
    let to = opt_usize(input, "to")?;
    if let Some(to) = to {
        if to < from {
            return Err(ToolOutcome::error(format!(
                "\"to\" ({to}) must be ≥ \"from\" ({from}); both are 1-based inclusive"
            )));
        }
    }

    // Streamed, not buffered whole. Loading the file first meant a
    // multi-gigabyte allocation to hand back 64 KiB; reading a fixed prefix
    // instead was worse — a window past the prefix silently returned the
    // wrong lines with `status: Ok`, which is exactly the input a SEARCH
    // block gets anchored on. Streaming bounds memory *and* serves any
    // window, because only the requested slice is ever retained.
    let file = match std::fs::File::open(root.join(&rel)) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolOutcome::error(format!(
                "{raw}: no such file — check the path (glob can find it)"
            )));
        }
        Err(err) => return Err(ToolOutcome::error(format!("{raw}: {err}"))),
    };
    let mut reader = std::io::BufReader::new(file);

    let mut shown: Vec<String> = Vec::new();
    let mut bytes_used = 0usize;
    let mut last_line = from.saturating_sub(1); // 1-based last included line
    let mut total_lines = 0usize;
    let mut hit_cap = false;
    let mut raw_line: Vec<u8> = Vec::new();
    loop {
        raw_line.clear();
        // `read_until` returns 0 only at EOF, and a final line without a
        // trailing newline still yields bytes — matching `str::lines`, which
        // `split(b'\n')` would not (it invents a trailing empty line).
        let read = match reader.read_until(b'\n', &mut raw_line) {
            Ok(read) => read,
            Err(err) => return Err(ToolOutcome::error(format!("{raw}: {err}"))),
        };
        if read == 0 {
            break;
        }
        if raw_line.contains(&0) {
            return Err(ToolOutcome::error(format!(
                "{raw}: binary file (contains NUL bytes); read handles text files only"
            )));
        }
        total_lines += 1;
        let number = total_lines;
        if let Some(to) = to {
            if number > to {
                // Past the window. Keep counting so a cap-truncation report
                // can name how many lines it withheld, but retain nothing.
                continue;
            }
        }
        if number < from || hit_cap {
            continue;
        }
        // Strip the line ending the way `str::lines` does.
        let mut text: &[u8] = &raw_line;
        if text.last() == Some(&b'\n') {
            text = &text[..text.len() - 1];
        }
        if text.last() == Some(&b'\r') {
            text = &text[..text.len() - 1];
        }
        let numbered = format!("{number:>4}| {}", String::from_utf8_lossy(text));
        bytes_used += numbered.len() + 1;
        if shown.len() >= caps::READ_LINES || bytes_used > caps::READ_BYTES {
            hit_cap = true;
            continue;
        }
        shown.push(numbered);
        last_line = number;
    }

    if total_lines == 0 {
        let mut outcome = ToolOutcome::ok(format!("{raw}: (empty file)"));
        outcome.read_credit = Some(rel.display().to_string());
        return Ok(outcome);
    }
    // The window the caller actually asked for, clamped to the file.
    let from = from.min(total_lines);
    let requested_to = to.unwrap_or(total_lines).min(total_lines);

    // Truncated means *a §6.1 cap cut the slice short* — not that the caller
    // asked for a window. Reporting a deliberate window as cap-truncated
    // invites pointless re-reads and mislabels the §6.10 ToolResult event.
    let truncated = last_line < requested_to;
    let mut content = format!("{}:{}-{last_line}\n", rel.display(), from);
    content.push_str(&shown.join("\n"));
    if truncated {
        let remaining = requested_to - last_line;
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
