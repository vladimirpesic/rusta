//! Bounding tool output before it enters the model's context.
//!
//! A tool result goes into the conversation verbatim and stays there until
//! compaction. One oversized result — a verbose test run, a 16 MiB `bash`
//! capture, a large MCP payload — can overrun the context window on its own, and
//! the overflow recovery that follows throws away older history to make room.
//!
//! Every result therefore passes through [`bound_tool_result`] on its way in:
//! text over the budget is cut down to a head and a tail,
//! the full output is saved to a spill file, and the excerpt ends with a pointer
//! telling the model how to read the rest. Nothing is lost, only moved out of
//! the context until the model asks for it.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use lm_bridge::message::ContentPart;

use crate::agent_loop::types::ExecutableToolResult;

/// Default per-result budget, in characters.
pub const DEFAULT_TOOL_OUTPUT_MAX_CHARS: usize = 50_000;

/// Default ceiling on the `max_chars` one `read_file` call may ask for.
pub const DEFAULT_READ_MAX_CHARS: usize = 500_000;

/// Default raw-byte budget for one image `read_media` sends, 256 KiB: small
/// enough that an image-heavy session stays well inside the providers'
/// request-body limits.
pub const DEFAULT_IMAGE_READ_BYTE_BUDGET: usize = 262_144;

/// The most of one output a spill file keeps. Larger outputs are cut, and the
/// excerpt says so.
pub const SPILL_RETENTION_BYTES: usize = 10 * 1024 * 1024;

/// How long spill files are kept before [`prune_spill_root`] removes them.
pub const SPILL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Share of the budget given to the head of a truncated output; the tail gets
/// the rest. The head carries the command's framing and the first errors, the
/// tail its conclusion — a test summary, a final error, an exit status.
const HEAD_SHARE: f64 = 0.6;

/// How far a cut may move back to land on a line boundary, as a share of the
/// piece it cuts. Cutting mid-line leaves a fragment the model has to guess at.
const LINE_SNAP_SHARE: f64 = 0.2;

/// Tools that already bound their own output and must not be cut again.
///
/// `read_file` enforces a character budget of its own and hands back an exact
/// continuation point; cutting its result here would break that contract — and
/// reading a spill file back through it would otherwise spill it again.
const SELF_BOUNDED_TOOLS: &[&str] = &["read_file"];

/// How tool results are bounded before they reach the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutputPolicy {
    /// Budget for one result's text, in characters. `0` disables the cap.
    pub max_chars: usize,
    /// Directory full outputs are saved to when they exceed the budget.
    pub spill_dir: PathBuf,
    /// Ceiling on the `max_chars` one `read_file` call may ask for. `read_file`
    /// bounds itself instead of going through [`bound_tool_result`]; `0`
    /// removes the ceiling.
    pub read_max_chars: usize,
    /// Raw-byte budget for one image `read_media` sends; larger images are
    /// re-encoded and downsampled to fit. `0` sets none.
    pub image_byte_budget: usize,
}

impl Default for ToolOutputPolicy {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_TOOL_OUTPUT_MAX_CHARS,
            spill_dir: default_spill_dir(),
            read_max_chars: DEFAULT_READ_MAX_CHARS,
            image_byte_budget: DEFAULT_IMAGE_READ_BYTE_BUDGET,
        }
    }
}

impl ToolOutputPolicy {
    /// A policy saving spill files under `spill_dir`.
    #[must_use]
    pub fn new(max_chars: usize, spill_dir: impl Into<PathBuf>) -> Self {
        Self {
            max_chars,
            spill_dir: spill_dir.into(),
            read_max_chars: DEFAULT_READ_MAX_CHARS,
            image_byte_budget: DEFAULT_IMAGE_READ_BYTE_BUDGET,
        }
    }

    /// Sets the raw-byte budget for one image (`0` sets none).
    #[must_use]
    pub fn with_image_byte_budget(mut self, image_byte_budget: usize) -> Self {
        self.image_byte_budget = image_byte_budget;
        self
    }

    /// Sets the ceiling on `read_file`'s `max_chars` (`0` removes it).
    #[must_use]
    pub fn with_read_max_chars(mut self, read_max_chars: usize) -> Self {
        self.read_max_chars = read_max_chars;
        self
    }
}

/// A per-process spill directory under the system temp dir, for agents whose
/// host did not name one (the CLI does: see `rusta_cli`).
fn default_spill_dir() -> PathBuf {
    static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        std::env::temp_dir().join("rusta-spill").join(format!(
            "{}-{}",
            std::process::id(),
            ulid::Ulid::generate()
        ))
    });
    DIR.clone()
}

/// Bound `result` to `policy`, saving the full output when it has to be cut.
///
/// Only text parts count against the budget; media parts pass through (they are
/// sized separately). `tool_name` exempts tools that bound themselves, and
/// `tool_call_id` names the spill file.
pub async fn bound_tool_result(
    result: ExecutableToolResult,
    tool_name: &str,
    tool_call_id: &str,
    policy: &ToolOutputPolicy,
) -> ExecutableToolResult {
    if policy.max_chars == 0 || SELF_BOUNDED_TOOLS.contains(&tool_name) {
        return result;
    }
    let text: String = result
        .output
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let total_chars = text.chars().count();
    if total_chars <= policy.max_chars {
        return result;
    }

    let saved = spill(&text, tool_call_id, &policy.spill_dir).await;
    let excerpt = excerpt(&text, total_chars, policy.max_chars);
    let pointer = match saved {
        Ok(Spilled {
            path,
            capped: false,
        }) => format!(
            "[Output truncated: {total_chars} characters in total. The full output was saved \
             to {}; use read_file with offset/limit (or grep) on that file to see the rest.]",
            path.display()
        ),
        Ok(Spilled { path, capped: true }) => format!(
            "[Output truncated: {total_chars} characters in total. The first {} MB were saved \
             to {}; use read_file with offset/limit (or grep) on that file to see more. The \
             remainder was too large to keep.]",
            SPILL_RETENTION_BYTES / (1024 * 1024),
            path.display()
        ),
        Err(e) => format!(
            "[Output truncated: {total_chars} characters in total. The full output could not \
             be saved ({e}); re-run the command with narrower output to see more.]"
        ),
    };

    let mut output = Vec::with_capacity(result.output.len());
    let mut text_placed = false;
    for part in result.output {
        match part {
            ContentPart::Text { .. } => {
                if !text_placed {
                    output.push(ContentPart::text(format!("{excerpt}\n\n{pointer}")));
                    text_placed = true;
                }
            }
            other => output.push(other),
        }
    }
    ExecutableToolResult {
        output,
        is_error: result.is_error,
        stop_turn: result.stop_turn,
    }
}

/// Save `bytes` in the spill directory as `<tool_call_id>-<index>.<extension>`,
/// and return the path.
///
/// For tool output the model cannot take inline — audio, binary resources —
/// and for originals worth keeping beside the copy the model gets, such as an
/// image sent downsampled. `read_file`, `grep` and
/// `read_media` can open the file.
pub async fn save_attachment(
    bytes: &[u8],
    tool_call_id: &str,
    index: usize,
    extension: &str,
    policy: &ToolOutputPolicy,
) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(&policy.spill_dir).await?;
    let extension: String = extension
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    let extension = if extension.is_empty() {
        "bin".to_string()
    } else {
        extension
    };
    let path = policy.spill_dir.join(format!(
        "{}-{index}.{extension}",
        spill_file_stem(tool_call_id)
    ));
    tokio::fs::write(&path, bytes).await?;
    Ok(path)
}

struct Spilled {
    path: PathBuf,
    capped: bool,
}

async fn spill(text: &str, tool_call_id: &str, dir: &Path) -> std::io::Result<Spilled> {
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join(format!("{}.txt", spill_file_stem(tool_call_id)));
    let capped = text.len() > SPILL_RETENTION_BYTES;
    let kept = if capped {
        &text[..floor_char_boundary(text, SPILL_RETENTION_BYTES)]
    } else {
        text
    };
    tokio::fs::write(&path, kept).await?;
    Ok(Spilled { path, capped })
}

/// A file-name-safe rendering of a tool-call id.
fn spill_file_stem(tool_call_id: &str) -> String {
    let stem: String = tool_call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        ulid::Ulid::generate().to_string()
    } else {
        stem
    }
}

/// The head and tail of `text` within `max_chars`, snapped to line boundaries,
/// with a marker saying how much was left out between them.
fn excerpt(text: &str, total_chars: usize, max_chars: usize) -> String {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let head_budget = ((max_chars as f64) * HEAD_SHARE) as usize;
    let tail_budget = max_chars.saturating_sub(head_budget);

    let head_end = byte_index_of_char(text, head_budget);
    let head = snap_back_to_line(&text[..head_end]);

    let tail_start = byte_index_of_char(text, total_chars.saturating_sub(tail_budget));
    let tail = snap_forward_to_line(&text[tail_start..]);

    let omitted = total_chars - head.chars().count() - tail.chars().count();
    format!("{head}\n\n[... {omitted} characters omitted ...]\n\n{tail}")
}

/// Cut `piece` back to its last line break, unless that would discard more than
/// [`LINE_SNAP_SHARE`] of it.
fn snap_back_to_line(piece: &str) -> &str {
    match piece.rfind('\n') {
        Some(idx) if (piece.len() - idx) as f64 <= piece.len() as f64 * LINE_SNAP_SHARE => {
            &piece[..idx]
        }
        _ => piece,
    }
}

/// Start `piece` at its first line break, unless that would discard more than
/// [`LINE_SNAP_SHARE`] of it.
fn snap_forward_to_line(piece: &str) -> &str {
    match piece.find('\n') {
        Some(idx) if (idx as f64) <= piece.len() as f64 * LINE_SNAP_SHARE => &piece[idx + 1..],
        _ => piece,
    }
}

/// The byte offset of the `n`th character, or the end of `text`.
fn byte_index_of_char(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(i, _)| i)
}

/// The largest char boundary at or below `index`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut i = index;
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Remove spill directories under `root` that have not been touched for
/// `max_age`. Spill files are scratch copies, so a failure to remove one is
/// ignored rather than surfaced.
pub fn prune_spill_root(root: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(max_age) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified <= cutoff);
        if !stale {
            continue;
        }
        let _ = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(dir: &Path, max_chars: usize) -> ToolOutputPolicy {
        ToolOutputPolicy::new(max_chars, dir)
    }

    #[tokio::test]
    async fn small_results_pass_through_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let result = ExecutableToolResult::text("short");
        let bounded =
            bound_tool_result(result.clone(), "bash", "c1", &policy(dir.path(), 100)).await;
        assert_eq!(bounded.text_content(), "short");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    /// An oversized result keeps its head and tail within budget, and the full
    /// text is saved where the pointer says.
    #[tokio::test]
    async fn an_oversized_result_is_cut_and_spilled() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (0..20_000).map(|i| format!("line {i:05}")).collect();
        let full = lines.join("\n");
        let result = ExecutableToolResult::text(full.clone());
        let bounded =
            bound_tool_result(result, "bash", "call_42", &policy(dir.path(), 5_000)).await;
        let text = bounded.text_content();
        assert!(text.starts_with("line 00000"), "{}", &text[..40]);
        assert!(text.contains("line 19999"), "the tail is kept");
        assert!(text.contains("characters omitted"));
        assert!(text.contains(&format!("{} characters in total", full.chars().count())));
        // Head + tail stay within budget; the markers add a small constant.
        assert!(
            text.chars().count() < 5_000 + 600,
            "{}",
            text.chars().count()
        );
        let spill = dir.path().join("call_42.txt");
        assert_eq!(std::fs::read_to_string(&spill).unwrap(), full);
        assert!(text.contains(&spill.display().to_string()));
    }

    /// Cuts land on line boundaries, so no fragment of a line is shown.
    #[tokio::test]
    async fn cuts_snap_to_line_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let full = (0..2_000)
            .map(|i| format!("entry-{i:04}-xxxxxxxxxx"))
            .collect::<Vec<_>>()
            .join("\n");
        let bounded = bound_tool_result(
            ExecutableToolResult::text(full),
            "bash",
            "c",
            &policy(dir.path(), 1_000),
        )
        .await;
        let text = bounded.text_content();
        let (head, rest) = text.split_once("\n\n[... ").unwrap();
        assert!(
            head.lines()
                .all(|l| l.len() == "entry-0000-xxxxxxxxxx".len())
        );
        let tail = rest.split_once(" ...]\n\n").unwrap().1;
        let tail = tail.split("\n\n[Output truncated").next().unwrap();
        assert!(
            tail.lines()
                .all(|l| l.len() == "entry-0000-xxxxxxxxxx".len())
        );
    }

    /// Multi-byte text is cut on character boundaries, never mid-codepoint.
    #[tokio::test]
    async fn multibyte_text_is_cut_safely() {
        let dir = tempfile::tempdir().unwrap();
        let full = "日本語のテキスト".repeat(2_000);
        let bounded = bound_tool_result(
            ExecutableToolResult::text(full),
            "bash",
            "c",
            &policy(dir.path(), 500),
        )
        .await;
        assert!(bounded.text_content().contains("characters omitted"));
    }

    /// Errors stay errors, and media parts survive the cut.
    #[tokio::test]
    async fn error_status_and_media_survive() {
        let dir = tempfile::tempdir().unwrap();
        let mut result = ExecutableToolResult::error("x".repeat(10_000));
        result.output.push(ContentPart::ImageUrl {
            image_url: lm_bridge::message::ImageUrl {
                url: "data:image/png;base64,AAAA".into(),
                id: None,
            },
        });
        let bounded = bound_tool_result(result, "mcp__x__y", "c", &policy(dir.path(), 1_000)).await;
        assert!(bounded.is_error);
        assert!(
            bounded
                .output
                .iter()
                .any(|p| matches!(p, ContentPart::ImageUrl { .. }))
        );
    }

    /// Tools that bound themselves are not cut again.
    #[tokio::test]
    async fn self_bounded_tools_are_exempt() {
        let dir = tempfile::tempdir().unwrap();
        let big = "y".repeat(10_000);
        let bounded = bound_tool_result(
            ExecutableToolResult::text(big.clone()),
            "read_file",
            "c",
            &policy(dir.path(), 1_000),
        )
        .await;
        assert_eq!(bounded.text_content(), big);
    }

    /// A zero budget disables the cap.
    #[tokio::test]
    async fn a_zero_budget_disables_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let big = "z".repeat(10_000);
        let bounded = bound_tool_result(
            ExecutableToolResult::text(big.clone()),
            "bash",
            "c",
            &policy(dir.path(), 0),
        )
        .await;
        assert_eq!(bounded.text_content(), big);
    }

    /// An output past the retention cap keeps its first part and says so.
    #[tokio::test]
    async fn outputs_past_the_retention_cap_are_saved_in_part() {
        let dir = tempfile::tempdir().unwrap();
        let big = "a".repeat(SPILL_RETENTION_BYTES + 1_000);
        let bounded = bound_tool_result(
            ExecutableToolResult::text(big),
            "bash",
            "c",
            &policy(dir.path(), 1_000),
        )
        .await;
        assert!(bounded.text_content().contains("were saved"));
        let saved = std::fs::metadata(dir.path().join("c.txt")).unwrap().len();
        assert_eq!(saved, SPILL_RETENTION_BYTES as u64);
    }

    #[tokio::test]
    async fn attachments_are_saved_under_the_spill_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = save_attachment(b"RIFF", "call/7", 2, "w.a/v", &policy(dir.path(), 10))
            .await
            .unwrap();
        assert_eq!(path, dir.path().join("call_7-2.wav"));
        assert_eq!(std::fs::read(&path).unwrap(), b"RIFF");
    }

    #[test]
    fn spill_file_names_are_sanitized() {
        assert_eq!(spill_file_stem("call:1/2"), "call_1_2");
        assert!(!spill_file_stem("").is_empty());
    }

    #[test]
    fn stale_spill_directories_are_pruned() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old-session");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("c.txt"), "x").unwrap();
        prune_spill_root(root.path(), Duration::ZERO);
        assert!(!old.exists());
    }
}
