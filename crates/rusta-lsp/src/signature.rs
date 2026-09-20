//! Signature extraction from LSP hover markdown.
//!
//! A pure function, compiled in both feature graphs and tested in both. This
//! is where the context cost of the whole feature is controlled: a
//! rust-analyzer hover on a generic-heavy item runs to several hundred tokens
//! of prose, and the one line this module returns is what makes the
//! annotation context-*positive* rather than another thing crowding a small
//! model's window.

use rusta_repomap::{MAX_LINE_LEN, truncate_line};

/// Separator rust-analyzer emits between an item's signature and its doc
/// comment. Everything from here down is prose and is discarded.
const DOC_SEPARATOR: &str = "---";

/// Fence delimiter for a markdown code block.
const FENCE: &str = "```";

/// Extract a one-line type signature from LSP hover markdown.
///
/// rust-analyzer returns a hover shaped like this — the containing path
/// first, the signature second, then the separator and the doc comment
/// (shown inside a wider fence so the inner ones render):
///
/// ````text
/// ```rust
/// mycrate::module
/// ```
///
/// ```rust
/// pub fn alpha(x: u32) -> u32
/// ```
///
/// ---
///
/// Doc comment prose, which may run for paragraphs.
/// ````
///
/// The rules, in order: discard everything from the first `---` line; keep
/// only the contents of fenced code blocks; take the **last** such block (the
/// first is the containing path); collapse whitespace; truncate to the map's
/// own line width.
///
/// Returns `None` when there is no fenced block, or when what survives is
/// empty — both meaning "nothing worth annotating", never an error.
#[must_use]
pub fn extract(hover_markdown: &str) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Option<Vec<&str>> = None;

    for line in hover_markdown.lines() {
        let trimmed = line.trim();

        // The separator ends the useful part of the hover. An unterminated
        // block above it is still worth keeping — rust-analyzer has been seen
        // to omit the closing fence — so flush it rather than dropping it.
        if current.is_none() && trimmed == DOC_SEPARATOR {
            break;
        }

        if trimmed.starts_with(FENCE) {
            match current.take() {
                // Closing fence: the block is complete.
                Some(lines) => push_block(&mut blocks, &lines),
                // Opening fence; the rest of the line is the language tag.
                None => current = Some(Vec::new()),
            }
            continue;
        }

        if let Some(lines) = current.as_mut() {
            lines.push(line);
        }
    }

    if let Some(lines) = current {
        push_block(&mut blocks, &lines);
    }

    // The last block is the signature: rust-analyzer puts the containing
    // module path in the first one.
    let signature = blocks.pop()?;
    Some(clip(&signature))
}

/// Marks a signature cut short, so a truncated one cannot read as a complete
/// one.
const ELLIPSIS: char = '…';

/// Clip a signature to the map's line width, **marking the cut**.
///
/// [`truncate_line`] is deliberately unmarked: a rendered map line is a
/// source excerpt inside a structure the model already reads as a sketch. A
/// signature is not. Cut silently, `fn rank_files(a: &BTreeMap<..>, ment`
/// ends on what looks like an identifier, and the model has no way to tell it
/// is missing three parameters and the return type — it would reason about a
/// function that does not exist. The marker costs one character of the same
/// budget and removes that failure entirely.
fn clip(signature: &str) -> String {
    if signature.chars().count() <= MAX_LINE_LEN {
        return signature.to_owned();
    }
    let mut clipped = truncate_line(signature, MAX_LINE_LEN - 1);
    clipped.push(ELLIPSIS);
    clipped
}

/// Collapse a block's lines into one whitespace-normalised string and record
/// it, discarding blocks that hold nothing but whitespace.
fn push_block(blocks: &mut Vec<String>, lines: &[&str]) {
    let joined = lines.join(" ");
    let collapsed = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if !collapsed.is_empty() {
        blocks.push(collapsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A verbatim rust-analyzer hover: module path block, signature block,
    /// separator, prose.
    const RUST_ANALYZER_HOVER: &str = "```rust\nrusta_repomap::drill\n```\n\n```rust\npub fn drill(root: &Path, request: DrillRequest<'_>) -> Result<String, DrillError>\n```\n\n---\n\nDrill `request` against the repo at `root`. Output format:\n`path:from-to` header followed by the requested source lines.\n";

    #[test]
    fn takes_the_signature_block_and_drops_the_prose() {
        let got = extract(RUST_ANALYZER_HOVER).expect("signature extracted");
        assert_eq!(
            got,
            "pub fn drill(root: &Path, request: DrillRequest<'_>) -> Result<String, DrillError>"
        );
        assert!(
            !got.contains("Drill `request`"),
            "doc prose must not survive"
        );
        assert!(
            !got.contains("rusta_repomap::drill"),
            "the containing path is the first block, not the signature"
        );
    }

    #[test]
    fn collapses_a_multi_line_signature_onto_one_line() {
        // rust-analyzer wraps long signatures across lines; the annotation is
        // a single appended line, so they must join.
        let hover = "```rust\nmod::thing\n```\n\n```rust\npub fn wide(\n    a: u32,\n    b: u32,\n) -> u32\n```\n";
        let got = extract(hover).expect("signature");
        assert_eq!(got, "pub fn wide( a: u32, b: u32, ) -> u32");
        assert!(!got.contains('\n'));
    }

    #[test]
    fn handles_a_hover_with_a_single_block() {
        let got = extract("```rust\nstruct Alpha\n```\n").expect("signature");
        assert_eq!(got, "struct Alpha");
    }

    #[test]
    fn handles_an_unterminated_fence() {
        // Defensive: a truncated hover must still yield its signature rather
        // than silently dropping the whole annotation.
        let got = extract("```rust\npub fn alpha() -> u32\n").expect("signature");
        assert_eq!(got, "pub fn alpha() -> u32");
    }

    #[test]
    fn returns_none_when_there_is_nothing_to_annotate() {
        // T-L3: malformed, empty and prose-only hovers degrade to `None`
        // rather than panicking or emitting noise.
        assert!(extract("").is_none(), "empty hover");
        assert!(
            extract("No hover information available").is_none(),
            "mcpls's own no-information placeholder is unfenced prose"
        );
        assert!(extract("```\n\n```\n").is_none(), "whitespace-only block");
        assert!(
            extract("---\n```rust\nfn after() {}\n```\n").is_none(),
            "nothing above the separator"
        );
    }

    #[test]
    fn truncates_at_the_map_line_width() {
        // T-L4: the annotation obeys the same cap as a rendered map line.
        let long = "a".repeat(MAX_LINE_LEN * 2);
        let got = extract(&format!("```rust\n{long}\n```\n")).expect("signature");
        assert_eq!(got.chars().count(), MAX_LINE_LEN);
    }

    #[test]
    fn a_truncated_signature_says_so() {
        // Unmarked, a cut signature reads as a complete one and the model
        // reasons about a function with the wrong arity and no return type.
        let long = "a".repeat(MAX_LINE_LEN * 2);
        let got = extract(&format!("```rust\n{long}\n```\n")).expect("signature");
        assert!(got.ends_with(ELLIPSIS), "a cut must be visible: {got}");

        // One that fits is left exactly alone — no marker, no padding.
        let exact = "b".repeat(MAX_LINE_LEN);
        let got = extract(&format!("```rust\n{exact}\n```\n")).expect("signature");
        assert_eq!(got, exact);
        assert!(!got.ends_with(ELLIPSIS));
    }

    #[test]
    fn truncates_without_splitting_a_multi_byte_character() {
        // A cap counted in bytes would slice a `→` in half and produce
        // invalid UTF-8 or a replacement character in the model's context.
        let wide = "→".repeat(MAX_LINE_LEN + 10);
        let got = extract(&format!("```rust\n{wide}\n```\n")).expect("signature");
        assert_eq!(got.chars().count(), MAX_LINE_LEN);
        assert!(
            got.chars().take(MAX_LINE_LEN - 1).all(|c| c == '→'),
            "the kept prefix must be intact"
        );
        assert!(got.ends_with(ELLIPSIS));
    }
}
