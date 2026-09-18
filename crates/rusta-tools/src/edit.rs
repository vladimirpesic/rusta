//! The `edit` and `write` tools — §6.4: tool-call forms of the §6.3 edit
//! protocol. Both funnel into the same `rusta-edit` apply chain, undo
//! journal, and read-before-edit ledger as SEARCH/REPLACE blocks — never a
//! second implementation.

use std::sync::Mutex;

use rusta_edit::{ApplyReport, EditBlock, Editor, ParsedResponse};
use serde_json::Value;

use crate::exec::{ToolOutcome, lock, req_nonempty, req_str, safe_rel};

/// Execute `edit(path, search, replace)`: one block through the apply chain
/// (auto-inject, cross-file retry, and the §6.3 failure-feedback contract
/// included). An empty `search` is a create/append per §6.3 rule 2.
pub(crate) fn edit(editor: &Mutex<Editor>, input: &Value) -> ToolOutcome {
    let run = || -> Result<ToolOutcome, ToolOutcome> {
        let raw = req_nonempty(input, "path")?;
        let rel = safe_rel(raw)?;
        let search = req_str(input, "search")?;
        let replace = req_str(input, "replace")?;
        // A42: §6.4 requires the two edit syntaxes to be identical, and they
        // were not. The text path normalizes CRLF in the parser and the file
        // content is normalized at apply time, but this path handed
        // `search`/`replace` over verbatim — so a model emitting \r\n inside
        // a tool-call `search` could never match the normalized file, and
        // failed with a NoMatch that named no cause. Normalizing here is the
        // parser's job done at the other entry point.
        let block = EditBlock {
            candidates: vec![rel.display().to_string()],
            original: search.replace("\r\n", "\n"),
            updated: replace.replace("\r\n", "\n"),
        };
        let report = lock(editor).apply_parsed(ParsedResponse {
            blocks: vec![block],
            commands: Vec::new(),
            notes: Vec::new(),
        });
        Ok(format_report(report))
    };
    run().unwrap_or_else(|outcome| outcome)
}

/// Execute `write(path, content)`: full-file journaled write. An existing
/// file must have been read first (§6.4) — a blind overwrite is the one
/// place the read-before-edit rule denies instead of auto-injecting,
/// because no SEARCH text proves the model ever saw the content. New
/// files are always allowed.
pub(crate) fn write(editor: &Mutex<Editor>, input: &Value) -> ToolOutcome {
    let run = || -> Result<ToolOutcome, ToolOutcome> {
        let raw = req_nonempty(input, "path")?;
        let rel = safe_rel(raw)?;
        let content = req_str(input, "content")?;
        {
            let editor = lock(editor);
            if editor.root().join(&rel).exists() && !editor.ledger().has_read(&rel) {
                return Err(ToolOutcome::error(format!(
                    "{raw} exists but has not been read this session; a full-file write would \
                     overwrite it blind. Read it first, then write — or use edit with a SEARCH block."
                )));
            }
        }
        match lock(editor).write_file(&rel.display().to_string(), content) {
            Ok(true) => Ok(ToolOutcome::ok(format!(
                "overwrote {} (journaled — /undo reverts it)",
                rel.display()
            ))),
            Ok(false) => Ok(ToolOutcome::ok(format!(
                "created {} (journaled — /undo reverts it)",
                rel.display()
            ))),
            Err(err) => Err(ToolOutcome::error(format!("{raw}: write failed: {err}"))),
        }
    };
    run().unwrap_or_else(|outcome| outcome)
}

/// Render an [`ApplyReport`] as an observation: applied lines + notes on
/// success; the §6.3 failure-feedback contract on failure.
fn format_report(report: ApplyReport) -> ToolOutcome {
    if report.is_success() {
        let mut lines = Vec::new();
        for applied in &report.applied {
            let mut line = format!("applied edit: {}", applied.path.display());
            if applied.created {
                line += " (created)";
            }
            if applied.appended {
                line += " (appended)";
            }
            if applied.cross_file {
                line += " (applied in a different file — see notes)";
            }
            lines.push(line);
        }
        for note in &report.notes {
            lines.push(format!("note: {note}"));
        }
        ToolOutcome::ok(lines.join("\n"))
    } else {
        let mut content = report
            .feedback
            .unwrap_or_else(|| "the edit failed to apply".to_owned());
        for note in &report.notes {
            content.push_str(&format!("\nnote: {note}"));
        }
        ToolOutcome::error(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Mutex<Editor>) {
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
        std::fs::write(dir.path().join("src/lib.rs"), "fn old() {}\n").expect("lib");
        let editor = Mutex::new(Editor::new(dir.path()));
        (dir, editor)
    }

    #[test]
    fn edit_applies_through_the_shared_chain() {
        let (dir, editor) = fixture();
        let outcome = edit(
            &editor,
            &json!({"path": "src/lib.rs", "search": "fn old() {}", "replace": "fn new() {}"}),
        );
        assert_eq!(
            outcome.status,
            rusta_core::Status::Ok,
            "{}",
            outcome.content
        );
        assert!(outcome.content.starts_with("applied edit: src/lib.rs"));
        // Unread file: the chain auto-injected a read and said so.
        assert!(
            outcome.content.contains("auto-injected"),
            "{}",
            outcome.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            "fn new() {}\n"
        );
        // One journal entry, undo restores.
        assert_eq!(editor.lock().unwrap().undo_stack().len(), 1);
    }

    #[test]
    fn failed_search_yields_the_6_3_failure_feedback() {
        let (_dir, editor) = fixture();
        let outcome = edit(
            &editor,
            &json!({"path": "src/lib.rs", "search": "fn absent() {}", "replace": "x"}),
        );
        assert_eq!(outcome.status, rusta_core::Status::Error);
        assert!(
            outcome.content.contains("failed to match"),
            "{}",
            outcome.content
        );
        assert!(
            outcome.content.contains("<<<<<<< SEARCH"),
            "{}",
            outcome.content
        );
    }

    #[test]
    fn empty_search_creates_a_new_file() {
        let (dir, editor) = fixture();
        let outcome = edit(
            &editor,
            &json!({"path": "docs/new.md", "search": "", "replace": "# hi\n"}),
        );
        assert_eq!(
            outcome.status,
            rusta_core::Status::Ok,
            "{}",
            outcome.content
        );
        assert!(outcome.content.contains("(created)"));
        assert!(dir.path().join("docs/new.md").exists());
    }

    #[test]
    fn write_requires_a_prior_read_for_existing_files_only() {
        let (dir, editor) = fixture();
        // Existing + unread → refused.
        let refused = write(&editor, &json!({"path": "src/lib.rs", "content": "x"}));
        assert_eq!(refused.status, rusta_core::Status::Error);
        assert!(refused.content.contains("has not been read"));

        // Read credits the ledger; the write then succeeds and is journaled.
        editor.lock().unwrap().record_read("src/lib.rs");
        let overwritten = write(&editor, &json!({"path": "src/lib.rs", "content": "y\n"}));
        assert_eq!(overwritten.status, rusta_core::Status::Ok);
        assert!(overwritten.content.contains("overwrote src/lib.rs"));

        // New files need no read.
        let created = write(&editor, &json!({"path": "notes.md", "content": "z\n"}));
        assert_eq!(created.status, rusta_core::Status::Ok);
        assert!(created.content.contains("created notes.md"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.md")).unwrap(),
            "z\n"
        );
    }

    #[test]
    fn malformed_inputs_name_the_missing_keys() {
        let (_dir, editor) = fixture();
        let edit_cases = [
            (json!({"search": "a", "replace": "b"}), "path"),
            (json!({"path": "a.rs", "replace": "b"}), "search"),
            (json!({"path": "a.rs", "search": "x"}), "replace"),
        ];
        for (input, key) in edit_cases {
            let outcome = edit(&editor, &input);
            assert_eq!(outcome.status, rusta_core::Status::Error);
            assert!(
                outcome
                    .content
                    .contains(&format!("missing required key \"{key}\"")),
                "{} for {input}",
                outcome.content
            );
        }
        let write_cases = [
            (json!({"content": "x"}), "path"),
            (json!({"path": "a.rs"}), "content"),
        ];
        for (input, key) in write_cases {
            let outcome = write(&editor, &input);
            assert_eq!(outcome.status, rusta_core::Status::Error);
            assert!(
                outcome
                    .content
                    .contains(&format!("missing required key \"{key}\"")),
                "{} for {input}",
                outcome.content
            );
        }
    }
}
