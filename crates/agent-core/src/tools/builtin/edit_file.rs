//! `edit_file`: replace text in a file, as `read_file` shows it.
//!
//! A model quotes `old_string` from `read_file`'s view of the file, so the
//! match is made in that view: a CR before a line feed is left out, as
//! `read_file` shows CRLF as LF. The replacement's line breaks follow the
//! file's own: a CRLF file stays CRLF, and in a file that mixes the two they
//! follow the line the match starts on.
//!
//! `old_string` must occur exactly once unless `replace_all` asks for every
//! occurrence: an ambiguous edit is refused rather than landing on the first
//! match.
//!
//! `read_file` numbers its lines. When `old_string` is not found and every
//! line of it starts with such a number, counting up by one, the model copied
//! the numbers along with the text. They are dropped from `old_string` and
//! `new_string`, the text is matched on the line the numbers name (or wherever
//! it occurs once), and the result says so.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::{Value, json};

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

pub struct EditFileTool {
    tool: Tool,
}

impl EditFileTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "edit_file",
                "Replace text in a file. old_string is the exact text to replace, as \
                 read_file shows it, without its line numbers: the number \
                 and tab before each line are not part of the file. It must occur exactly once; \
                 include surrounding lines to make it unique, or set replace_all to change \
                 every occurrence. Lines end in LF, as read_file shows them, even in a CRLF \
                 file: the file keeps its own line endings.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path" },
                        "old_string": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Exact text to replace, as read_file shows it, without the line numbers"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Replacement text, in the same form"
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace every occurrence of old_string instead of exactly one, e.g. to rename a symbol throughout the file. Defaults to false."
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            ),
        }
    }
}

impl Default for EditFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for EditFileTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, input: &Value) -> ToolAccesses {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_default();
        ToolAccesses::write_file(path)
    }

    fn approval_rule(&self, _input: &Value) -> &str {
        "write_file"
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let text_arg = |key: &str| input.get(key).and_then(|v| v.as_str()).unwrap_or("");
        let (old, new) = (text_arg("old_string"), text_arg("new_string"));
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = match ctx.resolve_allowed_path(Path::new(text_arg("path"))) {
            Ok(p) => p,
            Err(e) => return ExecutableToolResult::error(e),
        };

        // An edit needs no read: `old_string` must match the file as it is, so
        // the edit itself shows the model knows the text it changes. Whether
        // the agent's picture of the whole file is current decides only what it
        // may overwrite afterwards.
        let current = crate::stale_guard::FileStamp::of(&path).await;
        let picture_current = ctx
            .stale_guard
            .lock()
            .unwrap()
            .check(&path, current)
            .is_ok();

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(e) => {
                return ExecutableToolResult::error(format!(
                    "Failed to read {}: {e}",
                    path.display()
                ));
            }
        };
        let edited = match apply(&content, old, new, replace_all) {
            Ok(edited) => edited,
            Err(e) => return ExecutableToolResult::error(e.message(&path)),
        };
        if let Err(e) = tokio::fs::write(&path, &edited.text).await {
            // A failed write may have left the file partly changed; drop the
            // record so an overwrite must read it again.
            ctx.stale_guard.lock().unwrap().forget(&path);
            return ExecutableToolResult::error(format!("Failed to write {}: {e}", path.display()));
        }
        // A file the agent read, unchanged since, it still knows: re-stamped, so
        // its own edit does not stand between it and an overwrite. Of any other
        // it knows only the text it replaced: its stamp, if any, no longer
        // matches, and an overwrite needs a read first.
        if picture_current && let Ok(metadata) = tokio::fs::metadata(&path).await {
            let stamp =
                crate::stale_guard::FileStamp::from_parts(&metadata, Some(edited.text.as_bytes()));
            ctx.stale_guard.lock().unwrap().record(path.clone(), stamp);
        }
        ExecutableToolResult::text(edited.summary(&path))
    }
}

/// A file's new content after an edit, and what the edit did.
#[derive(Debug, PartialEq, Eq)]
struct Edited {
    text: String,
    /// Occurrences replaced.
    count: usize,
    /// `old_string` carried `read_file`'s line numbers, which were dropped.
    unnumbered: bool,
}

impl Edited {
    fn summary(&self, path: &Path) -> String {
        let occurrences = if self.count == 1 {
            "1 occurrence".to_string()
        } else {
            format!("{} occurrences", self.count)
        };
        let mut summary = format!("Edited {}: replaced {occurrences}.", path.display());
        if self.unnumbered {
            summary.push_str(
                " old_string carried read_file's line numbers, which are not part of the file: \
                 they were left out of old_string and new_string.",
            );
        }
        summary
    }
}

/// Why an edit was refused.
#[derive(Debug, PartialEq, Eq)]
enum EditError {
    Empty,
    NoChange,
    /// `numbered`: also not found without the line numbers `old_string`
    /// carried.
    NotFound {
        numbered: bool,
    },
    NotUnique {
        count: usize,
    },
}

impl EditError {
    fn message(&self, path: &Path) -> String {
        let shown = path.display();
        match self {
            Self::Empty => "old_string is empty: give the exact text to replace. To create a \
                            file or replace all of it, use write_file."
                .to_string(),
            Self::NoChange => {
                "old_string and new_string are the same: there is nothing to change.".to_string()
            }
            Self::NotFound { numbered: false } => format!(
                "old_string was not found in {shown}. Copy it exactly from read_file's view of \
                 the file, whitespace and indentation included and the line numbers left out, \
                 or read the file again if your picture of it may be out of date."
            ),
            Self::NotFound { numbered: true } => format!(
                "old_string was not found in {shown}, with or without the line numbers each of \
                 its lines starts with. Read the file again and copy old_string from it exactly, \
                 without the numbers."
            ),
            Self::NotUnique { count } => format!(
                "old_string occurs {count} times in {shown}. Include enough surrounding lines to \
                 make it unique, or set replace_all to replace every occurrence."
            ),
        }
    }
}

/// `content` with `old` replaced by `new`: once, where `old` occurs exactly
/// once, or everywhere with `replace_all`. The strings are in `read_file`'s
/// view of the file (see the module documentation).
fn apply(content: &str, old: &str, new: &str, replace_all: bool) -> Result<Edited, EditError> {
    if old.is_empty() {
        return Err(EditError::Empty);
    }
    // The view has no CRLF: one in the strings means a line break.
    let (old, new) = (old.replace("\r\n", "\n"), new.replace("\r\n", "\n"));
    if old == new {
        return Err(EditError::NoChange);
    }
    let view = View::of(content);

    let found = view.find_all(&old);
    if !found.is_empty() {
        let offsets = chosen(found, replace_all)?;
        return Ok(Edited {
            text: view.replace(&offsets, old.len(), &new),
            count: offsets.len(),
            unnumbered: false,
        });
    }

    let Some((first_line, unnumbered_old)) = without_line_numbers(&old) else {
        return Err(EditError::NotFound { numbered: false });
    };
    let unnumbered_new = strip_line_numbers(&new);
    if unnumbered_old.is_empty() {
        return Err(EditError::NotFound { numbered: true });
    }
    if unnumbered_old == unnumbered_new {
        return Err(EditError::NoChange);
    }
    let found = view.find_all(&unnumbered_old);
    if found.is_empty() {
        return Err(EditError::NotFound { numbered: true });
    }
    // The numbers name the line the text starts on: the one occurrence that
    // starts there is the one meant, even if the text occurs elsewhere too,
    // or the model left the line's indentation out. Numbers out of date
    // after an earlier edit still leave a text that occurs once.
    let on_its_line: Vec<usize> = view.line_span(first_line).map_or_else(Vec::new, |span| {
        found
            .iter()
            .copied()
            .filter(|at| span.contains(at))
            .collect()
    });
    let offsets = if on_its_line.len() == 1 && !replace_all {
        on_its_line
    } else {
        chosen(found, replace_all)?
    };
    Ok(Edited {
        text: view.replace(&offsets, unnumbered_old.len(), &unnumbered_new),
        count: offsets.len(),
        unnumbered: true,
    })
}

/// The occurrences an edit replaces: all with `replace_all`, otherwise the
/// only one.
fn chosen(found: Vec<usize>, replace_all: bool) -> Result<Vec<usize>, EditError> {
    if found.len() > 1 && !replace_all {
        return Err(EditError::NotUnique { count: found.len() });
    }
    Ok(found)
}

/// A file's text as `read_file` shows it: every CR before a line feed left
/// out.
struct View<'a> {
    raw: &'a str,
    text: Cow<'a, str>,
    /// Offsets in `text` of the line feeds whose CR was left out, ascending.
    crlf: Vec<usize>,
}

impl<'a> View<'a> {
    fn of(raw: &'a str) -> Self {
        if !raw.contains("\r\n") {
            return Self {
                raw,
                text: Cow::Borrowed(raw),
                crlf: Vec::new(),
            };
        }
        let mut text = String::with_capacity(raw.len());
        let mut crlf = Vec::new();
        let mut rest = raw;
        while let Some(i) = rest.find("\r\n") {
            text.push_str(&rest[..i]);
            crlf.push(text.len());
            text.push('\n');
            rest = &rest[i + 2..];
        }
        text.push_str(rest);
        Self {
            raw,
            text: Cow::Owned(text),
            crlf,
        }
    }

    /// The offset in the raw text where view offset `at` starts. A left-out
    /// CR belongs to the line feed after it, so a match never splits a CRLF.
    fn raw_offset(&self, at: usize) -> usize {
        at + self.crlf.partition_point(|&lf| lf < at)
    }

    /// Whether lines end in CRLF where view offset `at` is: the line feed
    /// ending its line, or past the last one, that one.
    fn crlf_at(&self, at: usize) -> bool {
        let lf = self.text[at..]
            .find('\n')
            .map(|i| at + i)
            .or_else(|| self.text[..at].rfind('\n'));
        lf.is_some_and(|lf| self.crlf.binary_search(&lf).is_ok())
    }

    /// View offsets of `needle`'s occurrences, left to right, none
    /// overlapping.
    fn find_all(&self, needle: &str) -> Vec<usize> {
        self.text.match_indices(needle).map(|(i, _)| i).collect()
    }

    /// View offsets of 1-based line `line`, from its start to its line feed
    /// (or the end of the text).
    fn line_span(&self, line: usize) -> Option<std::ops::RangeInclusive<usize>> {
        let start = match line {
            0 => return None,
            1 => 0,
            _ => self.text.match_indices('\n').nth(line - 2)?.0 + 1,
        };
        let end = self.text[start..]
            .find('\n')
            .map_or(self.text.len(), |i| start + i);
        Some(start..=end)
    }

    /// The raw text with the `len`-byte matches at view `offsets` replaced by
    /// `replacement`, its line breaks CRLF where the match's line has one.
    fn replace(&self, offsets: &[usize], len: usize, replacement: &str) -> String {
        let mut out = String::with_capacity(self.raw.len() + replacement.len() * offsets.len());
        let mut copied = 0;
        for &at in offsets {
            out.push_str(&self.raw[copied..self.raw_offset(at)]);
            if self.crlf_at(at) {
                out.push_str(&replacement.replace('\n', "\r\n"));
            } else {
                out.push_str(replacement);
            }
            copied = self.raw_offset(at + len);
        }
        out.push_str(&self.raw[copied..]);
        out
    }
}

/// `text` without the `N<TAB>` numbers `read_file` puts before each line,
/// and the first number, when every line starts with one and they count up
/// by one, as in a stretch copied from its output.
fn without_line_numbers(text: &str) -> Option<(usize, String)> {
    let mut first = None;
    let mut unnumbered = String::with_capacity(text.len());
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let (number, rest) = line_number(line)?;
        if number != *first.get_or_insert(number) + i {
            return None;
        }
        unnumbered.push_str(rest);
    }
    Some((first?, unnumbered))
}

/// `text` with the number dropped from each line that starts with one.
fn strip_line_numbers(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| line_number(line).map_or(line, |(_, rest)| rest))
        .collect()
}

/// A line's leading `N<TAB>` number, spaces before it allowed, and the rest
/// of the line.
fn line_number(line: &str) -> Option<(usize, &str)> {
    let (number, rest) = line.split_once('\t')?;
    let digits = number.trim_start_matches(' ');
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::builtin::ReadFileTool;

    fn edited(content: &str, old: &str, new: &str) -> String {
        apply(content, old, new, false).unwrap().text
    }

    #[test]
    fn a_unique_match_is_replaced() {
        let result = apply("fn a() {}\nfn b() {}\n", "fn b", "fn bee", false).unwrap();
        assert_eq!(
            result,
            Edited {
                text: "fn a() {}\nfn bee() {}\n".to_string(),
                count: 1,
                unnumbered: false,
            }
        );
    }

    /// Several matches are refused unless every one should change.
    #[test]
    fn an_ambiguous_match_is_refused_unless_replace_all() {
        assert_eq!(
            apply("x = 1; x = 2;", "x", "y", false),
            Err(EditError::NotUnique { count: 2 })
        );
        let all = apply("x = 1; x = 2;", "x", "y", true).unwrap();
        assert_eq!(all.text, "y = 1; y = 2;");
        assert_eq!(all.count, 2);
    }

    #[test]
    fn empty_missing_and_unchanged_edits_are_refused() {
        assert_eq!(apply("abc", "", "x", false), Err(EditError::Empty));
        assert_eq!(apply("abc", "b", "b", false), Err(EditError::NoChange));
        assert_eq!(
            apply("abc", "z", "y", false),
            Err(EditError::NotFound { numbered: false })
        );
    }

    /// A CRLF file is matched as `read_file` shows it, with LF line breaks,
    /// and stays CRLF.
    #[test]
    fn a_crlf_file_is_matched_as_read_file_shows_it_and_stays_crlf() {
        assert_eq!(
            edited("a\r\nb\r\nc\r\n", "a\nb", "a\nx\ny"),
            "a\r\nx\r\ny\r\nc\r\n"
        );
        // A CRLF in the strings is a line break too.
        assert_eq!(edited("a\r\nb\r\n", "a\r\nb", "z"), "z\r\n");
        assert_eq!(edited("a\nb\n", "a\r\nb", "x\r\ny"), "x\ny\n");
    }

    /// A match that stops at a line's end keeps the line's CRLF, one that
    /// spans it takes it along.
    #[test]
    fn an_edit_never_splits_a_crlf() {
        assert_eq!(edited("ab\r\ncd", "ab", "AB"), "AB\r\ncd");
        assert_eq!(edited("ab\r\ncd", "b\nc", "B C"), "aB Cd");
        assert_eq!(edited("ab\r\ncd", "\ncd", "!"), "ab!");
    }

    /// In a file that mixes CRLF and LF, new line breaks follow the line the
    /// match is on.
    #[test]
    fn in_a_mixed_file_the_replacement_follows_its_line() {
        assert_eq!(edited("a\r\nb\nc\r\n", "b", "b1\nb2"), "a\r\nb1\nb2\nc\r\n");
        assert_eq!(
            edited("a\r\nb\nc\r\n", "a", "a1\na2"),
            "a1\r\na2\r\nb\nc\r\n"
        );
        // Past the last line break, the last one decides.
        assert_eq!(edited("a\nb\r\nc", "c", "c1\nc2"), "a\nb\r\nc1\r\nc2");
    }

    /// Line numbers copied from `read_file` are dropped from both strings.
    #[test]
    fn copied_line_numbers_are_dropped() {
        let content = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let result = apply(content, "2\tfn b() {}", "2\tfn bee() {}", false).unwrap();
        assert_eq!(result.text, "fn a() {}\nfn bee() {}\nfn c() {}\n");
        assert!(result.unnumbered);

        // Several lines, a trailing line break, padding before the numbers,
        // and a new line without one.
        assert_eq!(
            edited(
                content,
                "  2\tfn b() {}\n  3\tfn c() {}\n",
                "2\tfn b() {}\nfn d() {}\n"
            ),
            "fn a() {}\nfn b() {}\nfn d() {}\n"
        );
    }

    /// The numbers name the line meant, even where the text occurs twice,
    /// and with the line's indentation left out.
    #[test]
    fn copied_line_numbers_pick_the_line_they_name() {
        assert_eq!(edited("x\ny\nx\n", "3\tx", "3\tz"), "x\ny\nz\n");
        assert_eq!(edited("x\ny\nx\n", "1\tx", "1\tz"), "z\ny\nx\n");
        assert_eq!(
            edited("    x = 0\n    y\n    x = 0\n", "3\tx = 0", "3\tx = 9"),
            "    x = 0\n    y\n    x = 9\n"
        );
        // An empty line named by its number.
        assert_eq!(edited("a\n\nb\n\n", "2\t\n3\tb", "2\t\n3\tc"), "a\n\nc\n\n");
    }

    /// Numbers that no longer match their line, say after an earlier edit,
    /// still find a text that occurs once; an ambiguous one is refused.
    #[test]
    fn stale_line_numbers_fall_back_to_a_unique_match() {
        assert_eq!(edited("a\nb\nc\n", "1\tc", "1\tC"), "a\nb\nC\n");
        assert_eq!(
            apply("a\nx\nx\n", "1\tx", "1\ty", false),
            Err(EditError::NotUnique { count: 2 })
        );
        assert_eq!(
            apply("a\nb\n", "1\tz", "1\ty", false),
            Err(EditError::NotFound { numbered: true })
        );
    }

    /// Numbers that are part of the file match as text first, and numbers
    /// that do not count up are no copied view.
    #[test]
    fn numbers_in_the_file_or_out_of_sequence_are_text() {
        let tsv = apply("1\tx\n2\ty\n", "2\ty", "2\tz", false).unwrap();
        assert_eq!(tsv.text, "1\tx\n2\tz\n");
        assert!(!tsv.unnumbered);
        assert_eq!(
            apply("foo\nbar\n", "5\tfoo\n9\tbar", "x", false),
            Err(EditError::NotFound { numbered: false })
        );
    }

    #[test]
    fn messages_name_the_file_and_the_remedy() {
        let path = Path::new("src/lib.rs");
        let message = EditError::NotUnique { count: 3 }.message(path);
        assert!(
            message.contains("occurs 3 times in src/lib.rs"),
            "{message}"
        );
        assert!(message.contains("replace_all"), "{message}");
        let message = EditError::NotFound { numbered: true }.message(path);
        assert!(message.contains("without the numbers"), "{message}");
    }

    fn ctx(dir: &Path) -> ExecutableToolContext {
        ExecutableToolContext {
            cwd: dir.to_path_buf(),
            allowed_paths: std::iter::once(dir.to_path_buf()).collect(),
            ..Default::default()
        }
    }

    /// A stretch of `read_file`'s output, copied with its numbers into both
    /// strings, edits the file: the end-to-end path a model takes.
    #[tokio::test]
    async fn an_edit_copied_from_read_file_output_applies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {\r\n    run();\r\n}\r\n").unwrap();
        let ctx = ctx(dir.path());
        let read = ReadFileTool::new()
            .execute(json!({"path": "a.rs"}), &ctx, "r")
            .await
            .text_content();
        assert_eq!(read, "1\tfn main() {\n2\t    run();\n3\t}");

        let copied = read.lines().take(2).collect::<Vec<_>>().join("\n");
        let result = EditFileTool::new()
            .execute(
                json!({
                    "path": "a.rs",
                    "old_string": copied,
                    "new_string": copied.replace("run()", "run(1)"),
                }),
                &ctx,
                "e",
            )
            .await;

        assert!(!result.is_error, "{}", result.text_content());
        assert!(
            result.text_content().contains("line numbers"),
            "{}",
            result.text_content()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\r\n    run(1);\r\n}\r\n"
        );
    }

    /// A refused edit leaves the file as it was, and says why.
    #[tokio::test]
    async fn a_refused_edit_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "same same").unwrap();
        let ctx = ctx(dir.path());
        ReadFileTool::new()
            .execute(json!({"path": "a.txt"}), &ctx, "r")
            .await;

        let result = EditFileTool::new()
            .execute(
                json!({"path": "a.txt", "old_string": "same", "new_string": "other"}),
                &ctx,
                "e",
            )
            .await;

        assert!(result.is_error);
        assert!(
            result.text_content().contains("occurs 2 times"),
            "{}",
            result.text_content()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "same same");
    }
}
