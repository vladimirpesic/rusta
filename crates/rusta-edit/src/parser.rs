//! Single-pass SEARCH/REPLACE parser — development plan §6.3 (R4).
//!
//! Semantics borrowed from Aider's `find_original_update_blocks`
//! (reference §3) with the plan's modifications:
//!
//! * markers are matched on `.trim()`ed lines and forgive run lengths
//!   5–9 (`<<<<<<<` … `<<<<<<<<<`) plus a stray trailing `>` on HEAD —
//!   both are proven small-model artifacts;
//! * a new DIVIDER ends REPLACE collection and immediately starts the next
//!   block's SEARCH collection (models chain blocks without UPDATED);
//! * a missing final UPDATED at end-of-stream still commits the block;
//! * the parser never panics and never fails: malformed blocks degrade to
//!   prose plus a corrective note (smallcode's drop-on-malformed policy).
//!
//! Filenames are *not* resolved here: the parser only collects candidate
//! lines (nearest first) from up to three lines above the HEAD marker;
//! [`crate::Editor`] resolves them against the session read-set.

/// Accepted marker run lengths (`^<{5,9}` etc. — Aider's forgiveness).
const MARKER_MIN: usize = 5;
const MARKER_MAX: usize = 9;

/// Lines scanned above a HEAD marker for a filename (§6.3 rule 3, Aider's
/// `lines[max(0, i - 3):i]`). Unrelated to the marker run lengths above —
/// it used to be spelled `MARKER_MIN - 2`, which silently coupled the two.
const FILENAME_SCAN_LINES: usize = 3;

/// Fence languages that mark a *suggested command* block (§6.3 rule 5).
/// Such blocks are surfaced to the user for confirmation, never executed.
const SHELL_FENCES: &str = "bash sh shell cmd batch powershell ps1 zsh fish ksh csh tcsh";

/// One parsed SEARCH/REPLACE block, verbatim from the response (LF endings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditBlock {
    /// Candidate filename lines gathered from up to three lines above the
    /// HEAD marker, **nearest first**, after `strip_filename` cleanup.
    /// Empty when the block named no file (continuation applies at apply time).
    pub candidates: Vec<String>,
    /// SEARCH text, exactly as the model wrote it (line endings normalized
    /// to LF). Fence stripping happens in the apply chain, not here.
    pub original: String,
    /// REPLACE text, exactly as the model wrote it (LF endings).
    pub updated: String,
}

impl EditBlock {
    /// Empty SEARCH ⇒ create-file / append-file edit (§6.3 rule 2).
    pub fn is_new_file(&self) -> bool {
        self.original.trim().is_empty()
    }
}

/// Everything actionable extracted from one model response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedResponse {
    /// SEARCH/REPLACE blocks in document order.
    pub blocks: Vec<EditBlock>,
    /// Suggested shell commands from fenced ` ```bash `-style blocks that
    /// are not part of an edit. Surfaced to the user; never auto-executed.
    pub commands: Vec<String>,
    /// Corrective notes for malformed blocks (fed back to the model).
    pub notes: Vec<String>,
}

/// Line-by-line "am I inside a SEARCH/REPLACE block?" tracker, sharing this
/// module's forgiving marker rules so it can never drift from the parser.
///
/// The CLI splits a completion at ` ```tool ` fences before handing prose to
/// [`parse_response`]. Without this tracker a fence *inside* an edit body
/// tore the block in half: the REPLACE payload was silently truncated to the
/// text before the fence, the edit was still reported as applied, and the
/// fenced content was executed as a tool call. File content the model is
/// writing must never be re-read as an instruction to the agent — and this
/// is ordinary content, since any repo documenting Rusta's own tool format
/// contains such a fence.
#[derive(Debug, Default, Clone)]
pub struct BlockScan {
    inside: bool,
}

impl BlockScan {
    /// A scanner positioned outside any block.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the next line; `true` when it belongs to an edit block — the
    /// HEAD and UPDATED markers included — and must not be reinterpreted.
    ///
    /// Only UPDATED closes a block: a DIVIDER seen mid-REPLACE chains
    /// straight into the next block (§6.3 rule 1), so the scan correctly
    /// stays inside. A block whose UPDATED never arrives holds the scan to
    /// end of input, matching §6.3 rule 4, which commits that trailing text
    /// as the REPLACE payload rather than leaving it loose.
    pub fn inside(&mut self, line: &str) -> bool {
        let trimmed = line.trim();
        if !self.inside {
            self.inside = is_head(trimmed);
            return self.inside;
        }
        if is_updated(trimmed) {
            self.inside = false;
        }
        true
    }
}

/// Parse a full model response into edits, commands, and notes.
///
/// Never panics on any input (§6.3 rule 6); malformed blocks are dropped
/// with a note and parsing continues.
pub fn parse_response(text: &str) -> ParsedResponse {
    // CRLF normalized once, up front (§6.3 forgiveness).
    let text = if text.contains('\r') {
        text.replace("\r\n", "\n")
    } else {
        text.to_owned()
    };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();

    let mut out = ParsedResponse::default();
    let mut candidates: Vec<String> = Vec::new();
    let mut original: Vec<&str> = Vec::new();
    let mut updated: Vec<&str> = Vec::new();
    let mut in_search = false;
    let mut in_replace = false;
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if !in_search && !in_replace {
            // Suggested command: a shell fence whose next two lines are not
            // an edit block HEAD (Aider's `next_is_editblock` guard — the
            // "```sh\npath\n<<<<<<< SEARCH" prelude is not a command).
            if is_shell_fence(trimmed) && !next_is_head(&lines, i) {
                let (command, next) = collect_shell_block(&lines, i);
                if !command.is_empty() {
                    out.commands.push(command);
                }
                i = next;
                continue;
            }
            if is_head(trimmed) {
                candidates = filename_candidates(&lines, i);
                original.clear();
                in_search = true;
                i += 1;
                continue;
            }
            // Plain prose.
            i += 1;
            continue;
        }

        if in_search {
            if is_divider(trimmed) {
                in_search = false;
                in_replace = true;
                updated.clear();
                i += 1;
                continue;
            }
            if is_head(trimmed) {
                // A HEAD while collecting SEARCH means the previous block
                // never saw its divider. Drop it with a note and restart.
                out.notes.push(MISSING_DIVIDER_NOTE.to_owned());
                candidates = filename_candidates(&lines, i);
                original.clear();
                i += 1;
                continue;
            }
            original.push(line);
            i += 1;
            continue;
        }

        // in_replace: UPDATED commits; a new DIVIDER commits *and* chains.
        if is_updated(trimmed) || is_divider(trimmed) {
            out.blocks.push(EditBlock {
                candidates: std::mem::take(&mut candidates),
                original: original.concat(),
                updated: updated.concat(),
            });
            original.clear();
            updated.clear();
            if is_divider(trimmed) {
                // Chained block: SEARCH collection starts immediately; no
                // candidates of its own — continuation applies at apply time.
                in_search = true;
                in_replace = false;
            } else {
                in_search = false;
                in_replace = false;
            }
            i += 1;
            continue;
        }
        updated.push(line);
        i += 1;
    }

    // Missing final UPDATED at end-of-stream still commits (§6.3 rule 4).
    //
    // Aider rejects this case outright; the plan deliberately forgives it
    // (§0 rule 3 — the plan wins). Forgiving it safely means one extra
    // guard: the usual reason the marker is "missing" is that the model
    // omitted the newline before it, so the marker is sitting at the end of
    // the last REPLACE line. Committing that verbatim writes
    // `>>>>>>> REPLACE` into the user's source. Split it back off.
    if in_replace {
        let (updated, note) = split_trailing_marker(&updated.concat());
        if let Some(note) = note {
            out.notes.push(note);
        }
        out.blocks.push(EditBlock {
            candidates,
            original: original.concat(),
            updated,
        });
    } else if in_search {
        out.notes.push(MISSING_DIVIDER_NOTE.to_owned());
    }

    out
}

/// Strips an UPDATED marker that the model ran onto the end of the last
/// REPLACE line (`…BB>>>>>>> REPLACE`). Returns the cleaned text plus a
/// corrective note when a marker was removed.
fn split_trailing_marker(updated: &str) -> (String, Option<String>) {
    let trimmed = updated.trim_end_matches(['\n', ' ', '\t']);
    let Some(before_word) = trimmed.strip_suffix(" REPLACE") else {
        return (updated.to_owned(), None);
    };
    let run = before_word.len() - before_word.trim_end_matches('>').len();
    if !(MARKER_MIN..=MARKER_MAX).contains(&run) {
        return (updated.to_owned(), None);
    }
    // A marker alone on its line was consumed by the main loop, so anything
    // reaching here is glued to content (or is the whole tail).
    let mut kept = before_word[..before_word.len() - run].to_owned();
    if !kept.is_empty() && !kept.ends_with('\n') {
        kept.push('\n');
    }
    (kept, Some(TRAILING_MARKER_NOTE.to_owned()))
}

const TRAILING_MARKER_NOTE: &str = "A `>>>>>>> REPLACE` marker was run onto the end of the last \
                                     REPLACE line and has been stripped. Put each marker alone on \
                                     its own line.";

const MISSING_DIVIDER_NOTE: &str = "A SEARCH/REPLACE block was missing its `=======` divider \
                                     and was ignored. Each block needs `<<<<<<< SEARCH`, \
                                     `=======`, and `>>>>>>> REPLACE` markers.";

/// `<<<<<<< SEARCH` (5–9 `<`, optional stray trailing `>`), on trimmed text.
fn is_head(trimmed: &str) -> bool {
    let run = leading_run(trimmed, '<');
    (MARKER_MIN..=MARKER_MAX).contains(&run)
        && matches!(trimmed.get(run..), Some(" SEARCH") | Some(" SEARCH>"))
}

/// `=======` (5–9 `=`), on trimmed text.
fn is_divider(trimmed: &str) -> bool {
    let run = leading_run(trimmed, '=');
    (MARKER_MIN..=MARKER_MAX).contains(&run) && run == trimmed.len()
}

/// `>>>>>>> REPLACE` (5–9 `>`), on trimmed text.
fn is_updated(trimmed: &str) -> bool {
    let run = leading_run(trimmed, '>');
    (MARKER_MIN..=MARKER_MAX).contains(&run) && trimmed.get(run..) == Some(" REPLACE")
}

fn leading_run(line: &str, ch: char) -> usize {
    line.chars().take_while(|&c| c == ch).count()
}

/// A fence whose *language token* is one of [`SHELL_FENCES`]. The token is
/// matched whole: prefix matching made ` ```csharp ` a shell block (via
/// `csh`), and likewise ` ```shader ` and ` ```batchfile `.
fn is_shell_fence(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("```") else {
        return false;
    };
    let lang = rest
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .next()
        .unwrap_or_default();
    SHELL_FENCES.split(' ').any(|known| known == lang)
}

/// Aider's `next_is_editblock` guard: HEAD on the next line or the one after.
fn next_is_head(lines: &[&str], i: usize) -> bool {
    lines.get(i + 1).is_some_and(|l| is_head(l.trim()))
        || lines.get(i + 2).is_some_and(|l| is_head(l.trim()))
}

/// Collect a fenced shell block body starting after `i`; returns the joined
/// (trimmed) command text and the index of the first line after the block.
/// An unterminated fence runs to end-of-stream.
fn collect_shell_block(lines: &[&str], i: usize) -> (String, usize) {
    let mut j = i + 1;
    let mut body = Vec::new();
    while j < lines.len() && !lines[j].trim().starts_with("```") {
        body.push(lines[j]);
        j += 1;
    }
    // Skip the closing fence when present.
    if j < lines.len() {
        j += 1;
    }
    (body.concat().trim().to_owned(), j)
}

/// Candidate filenames from up to three lines above the HEAD marker at
/// `head`, **nearest first**. Fence lines are skipped (plan §6.3 rule 3);
/// the scan stops at the first non-fence line (Aider semantics — this keeps
/// prose mentions two lines up from hijacking resolution). Blank lines are
/// also skipped: a blank between filename and HEAD is a classic small-model
/// artifact worth forgiving.
fn filename_candidates(lines: &[&str], head: usize) -> Vec<String> {
    let mut out = Vec::new();
    let start = head.saturating_sub(FILENAME_SCAN_LINES);
    for j in (start..head).rev() {
        let trimmed = lines[j].trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("```") {
            // Fence line, or a DeepSeek-style fence-prefixed filename.
            if !rest.is_empty() && (rest.contains('.') || rest.contains('/')) {
                out.push(rest.to_owned());
            }
            continue; // fences never end the scan
        }
        if let Some(name) = strip_filename(trimmed) {
            out.push(name);
        }
        break; // first non-fence line ends the scan
    }
    out
}

/// Aider's `strip_filename`: clean a raw line into a filename candidate.
/// `...` lines and edit markers are never filenames; trailing `:`, leading
/// `#`, and surrounding `` ` `` / `*` markdown decoration are stripped.
fn strip_filename(trimmed: &str) -> Option<String> {
    if trimmed == "..." || is_head(trimmed) || is_divider(trimmed) || is_updated(trimmed) {
        return None;
    }
    let name = trimmed
        .trim_end_matches(':')
        .trim_start_matches('#')
        .trim()
        .trim_matches('`')
        .trim_matches('*');
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(candidates: &[&str], original: &str, updated: &str) -> EditBlock {
        EditBlock {
            candidates: candidates.iter().map(|s| (*s).to_owned()).collect(),
            original: original.to_owned(),
            updated: updated.to_owned(),
        }
    }

    #[test]
    fn parses_simple_block_with_filename_above() {
        let text = "Here is the fix.\n\nsrc/main.rs\n<<<<<<< SEARCH\nfn main() {\n    old();\n}\n=======\nfn main() {\n    new();\n}\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(
            parsed.blocks,
            vec![block(
                &["src/main.rs"],
                "fn main() {\n    old();\n}\n",
                "fn main() {\n    new();\n}\n"
            )]
        );
        assert!(parsed.commands.is_empty());
        assert!(parsed.notes.is_empty());
    }

    #[test]
    fn accepts_flexible_marker_lengths_and_stray_gt() {
        let text = "a.rs\n<<<<<<<< SEARCH>\nold\n========\nnew\n>>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(parsed.blocks, vec![block(&["a.rs"], "old\n", "new\n")]);
    }

    #[test]
    fn chained_divider_starts_next_block() {
        // Two complete edits sharing markers: the second block starts at the
        // chained DIVIDER with no HEAD of its own (§6.3 rule 1).
        let text = "src/lib.rs\n<<<<<<< SEARCH\none\n=======\ntwo\n=======\nthree\n=======\nfour\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(
            parsed.blocks,
            vec![
                block(&["src/lib.rs"], "one\n", "two\n"),
                block(&[], "three\n", "four\n"),
            ]
        );
    }

    #[test]
    fn missing_final_updated_still_commits() {
        let text = "src/a.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n";
        let parsed = parse_response(text);
        assert_eq!(parsed.blocks, vec![block(&["src/a.rs"], "old\n", "new\n")]);
        assert!(parsed.notes.is_empty());
    }

    #[test]
    fn missing_divider_degrades_to_note() {
        let text = "src/a.rs\n<<<<<<< SEARCH\nfn main() {}\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert!(parsed.blocks.is_empty());
        assert_eq!(parsed.notes.len(), 1);
        assert!(parsed.notes[0].contains("======="));
    }

    #[test]
    fn head_inside_search_restarts_with_note() {
        let text = "src/a.rs\n<<<<<<< SEARCH\na\n<<<<<<< SEARCH\n";
        let parsed = parse_response(text);
        assert!(parsed.blocks.is_empty());
        assert_eq!(parsed.notes.len(), 2);
    }

    #[test]
    fn head_inside_replace_is_content() {
        let text = "src/a.rs\n<<<<<<< SEARCH\na\n=======\nb\n<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        // Aider semantics: stray markers inside REPLACE are plain content.
        assert_eq!(
            parsed.blocks,
            vec![block(&["src/a.rs"], "a\n", "b\n<<<<<<< SEARCH\nx\n")]
        );
    }

    #[test]
    fn new_file_block_has_empty_search() {
        let text = "docs/notes.md\n<<<<<<< SEARCH\n=======\n# Notes\n\nhello\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(parsed.blocks.len(), 1);
        let b = &parsed.blocks[0];
        assert_eq!(b.candidates, vec!["docs/notes.md"]);
        assert_eq!(b.original, "");
        assert_eq!(b.updated, "# Notes\n\nhello\n");
        assert!(b.is_new_file());
    }

    #[test]
    fn deepseek_fenced_filename_is_candidate() {
        let text = "Sure!\n\n```rust\nword_count.py\n```\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(
            parsed.blocks,
            vec![block(&["word_count.py"], "old\n", "new\n")]
        );
    }

    #[test]
    fn fence_prefixed_filename_is_candidate() {
        let text = "```src/lib.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(
            parsed.blocks,
            vec![block(&["src/lib.rs"], "old\n", "new\n")]
        );
    }

    #[test]
    fn filename_cleanup_strips_markdown_decoration() {
        for (line, expected) in [
            ("src/main.rs:", "src/main.rs"),
            ("# src/main.rs", "src/main.rs"),
            ("**main.rs**", "main.rs"),
            ("`mod.rs`", "mod.rs"),
        ] {
            let text = format!("{line}\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n");
            let parsed = parse_response(&text);
            assert_eq!(
                parsed.blocks[0].candidates,
                vec![expected.to_owned()],
                "line: {line}"
            );
        }
    }

    #[test]
    fn dots_line_and_blank_lines_are_not_filenames() {
        let text = "...\n\nsrc/a.rs\n\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(parsed.blocks[0].candidates, vec!["src/a.rs"]);
    }

    #[test]
    fn crlf_is_normalized() {
        let text = "src/a.rs\r\n<<<<<<< SEARCH\r\nold\r\n=======\r\nnew\r\n>>>>>>> REPLACE\r\n";
        let parsed = parse_response(text);
        assert_eq!(parsed.blocks, vec![block(&["src/a.rs"], "old\n", "new\n")]);
    }

    #[test]
    fn shell_command_is_surfaced() {
        let text = "Run this:\n\n```bash\ncargo test\ncargo fmt --check\n```\n\nsrc/main.rs\n<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(
            parsed.commands,
            vec!["cargo test\ncargo fmt --check".to_owned()]
        );
        assert_eq!(parsed.blocks.len(), 1);
    }

    #[test]
    fn shell_fence_directly_above_head_is_not_a_command() {
        let text = "```sh\nscripts/build.sh\n<<<<<<< SEARCH\nset -e\n=======\nset -euo pipefail\n>>>>>>> REPLACE\n```\n";
        let parsed = parse_response(text);
        assert!(parsed.commands.is_empty());
        assert_eq!(
            parsed.blocks,
            vec![block(
                &["scripts/build.sh"],
                "set -e\n",
                "set -euo pipefail\n"
            )]
        );
    }

    #[test]
    fn unterminated_shell_fence_collects_to_eof() {
        let parsed = parse_response("```bash\ncargo build\n");
        assert_eq!(parsed.commands, vec!["cargo build".to_owned()]);
    }

    #[test]
    fn prose_markers_are_ignored() {
        let text = "The separator ======= appears in prose.\nSo does <<<<<<< SEARCH, oddly.\n";
        let parsed = parse_response(text);
        assert_eq!(parsed, ParsedResponse::default());
    }

    #[test]
    fn second_block_without_filename_has_no_candidates() {
        let text = "src/a.rs\n<<<<<<< SEARCH\none\n=======\ntwo\n>>>>>>> REPLACE\n\n<<<<<<< SEARCH\nthree\n=======\nfour\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(
            parsed.blocks,
            vec![
                block(&["src/a.rs"], "one\n", "two\n"),
                block(&[], "three\n", "four\n"),
            ]
        );
    }

    #[test]
    fn search_and_replace_may_contain_fence_lines() {
        let text =
            "README.md\n<<<<<<< SEARCH\n```\nold\n```\n=======\n```\nnew\n```\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(
            parsed.blocks,
            vec![block(&["README.md"], "```\nold\n```\n", "```\nnew\n```\n")]
        );
    }

    #[test]
    fn trailing_updated_marker_is_split_off_not_committed() {
        // Small models drop the newline before the closing marker; §6.3 rule
        // 4 then commits the whole line. The marker must never reach the file.
        let text = "a.rs\n<<<<<<< SEARCH\nold\n=======\nnew>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(parsed.blocks, vec![block(&["a.rs"], "old\n", "new\n")]);
        assert_eq!(parsed.notes.len(), 1);
        assert!(
            parsed.notes[0].contains(">>>>>>> REPLACE"),
            "{:?}",
            parsed.notes
        );
    }

    #[test]
    fn marker_alone_on_its_line_still_closes_the_block_cleanly() {
        // The well-formed case must be untouched by the rule-4 guard.
        let text = "a.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n";
        let parsed = parse_response(text);
        assert_eq!(parsed.blocks, vec![block(&["a.rs"], "old\n", "new\n")]);
        assert!(parsed.notes.is_empty());
    }

    #[test]
    fn replace_text_ending_in_angle_brackets_is_preserved() {
        // Not a marker: must survive verbatim.
        let text = "a.rs\n<<<<<<< SEARCH\nold\n=======\nVec<Vec<u8>>\n";
        let parsed = parse_response(text);
        assert_eq!(parsed.blocks[0].updated, "Vec<Vec<u8>>\n");
        assert!(parsed.notes.is_empty());
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert_eq!(parse_response(""), ParsedResponse::default());
    }
}
