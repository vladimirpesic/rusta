//! Budget-fitted rendering — ADR.md §6.5 steps 5–6.
//!
//! Definitions render in rank order, grouped by file, chat files excluded
//! (their content is already in context). Each shown region is the
//! definition line plus a small window of surrounding context, headed by
//! `path/to/file.rs:`; tag-less files appear as bare paths. Every rendered
//! line is truncated to 100 chars and carries a `│` gutter, and every gap
//! between shown regions is marked `⋮` so the model can never mistake
//! elided code for contiguous code (Aider's `TreeContext` convention).
//!
//! Token cost is estimated from ≤ 100 evenly-spaced lines scaled by the
//! character ratio (Aider's sampling trick). Fitting drops the
//! lowest-ranked **definitions**, not whole files: the budget therefore
//! binds smoothly, and a tight budget yields fewer definitions rather than
//! an empty map.

use crate::graph::RankedLoi;
use std::collections::{BTreeMap, BTreeSet};

/// Context lines kept *above* a definition line — enough for an attribute,
/// decorator, or one-line doc comment, which is what a SEARCH block anchors
/// on. Deliberately small: the map is an overview, not a reading view.
const PAD_BEFORE: usize = 1;
/// Context lines kept *below* a definition line (signature plus a little
/// body — the shape Aider's parent-scope rendering produces in practice).
const PAD_AFTER: usize = 2;
/// Hard cap on rendered line length in chars (§6.5 step 5).
const MAX_LINE_LEN: usize = 100;
/// Lines sampled for token-cost estimation (§6.5 step 6).
const SAMPLE_LINES: usize = 100;
/// Marks elided lines between two shown regions.
const ELISION: &str = "⋮";
/// Prefixes every shown source line, so structure and code never blur.
const GUTTER: char = '│';

/// Render the budget-fitted map for rank-ordered `lois`, reading each source
/// via `read` (repo-relative path → file text). Never exceeds
/// `budget_tokens` as estimated by [`estimate_tokens`].
pub(crate) fn fit_map(
    lois: &[RankedLoi],
    chat_files: &BTreeSet<String>,
    read: &dyn Fn(&str) -> Option<String>,
    budget_tokens: usize,
) -> String {
    let visible: Vec<&RankedLoi> = lois
        .iter()
        .filter(|l| !chat_files.contains(&l.rel))
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    // Largest rank-ordered prefix of the definition list whose render fits.
    // Monotone by construction: keeping one more definition can only add
    // lines, so binary search is sound.
    let mut best = String::new();
    let (mut lo, mut hi) = (1usize, visible.len());
    while lo <= hi {
        let keep = lo + (hi - lo) / 2;
        let text = render_subset(&visible[..keep], read);
        if estimate_tokens(&text) <= budget_tokens {
            best = text;
            lo = keep + 1;
        } else {
            hi = keep - 1;
        }
    }
    best
}

/// Render `kept` definitions, grouped by file (Aider sorts tags before
/// building the tree, so output order is by path, not by rank).
fn render_subset(kept: &[&RankedLoi], read: &dyn Fn(&str) -> Option<String>) -> String {
    let mut by_file: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
    let mut bare: BTreeSet<&str> = BTreeSet::new();
    for loi in kept {
        match loi.line {
            Some(line) => {
                by_file.entry(&loi.rel).or_default().insert(line);
            }
            None => {
                bare.insert(&loi.rel);
            }
        }
    }
    let mut out = String::new();
    for (rel, lines) in &by_file {
        if let Some(source) = read(rel) {
            out.push_str(&render_file(rel, lines, &source));
        }
    }
    for rel in bare {
        if !by_file.contains_key(rel) {
            out.push_str(&format!("\n{rel}\n"));
        }
    }
    out
}

/// One file block: header, then each shown region gutter-prefixed, with `⋮`
/// wherever lines were elided — including before the first region and after
/// the last, when those do not reach the file's edges.
fn render_file(rel: &str, lois: &BTreeSet<usize>, source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return format!("\n{rel}\n");
    }
    let last = lines.len() - 1;
    let mut show: BTreeSet<usize> = BTreeSet::new();
    for &loi in lois {
        if loi > last {
            continue; // stale cache entry; never index past the file
        }
        let from = loi.saturating_sub(PAD_BEFORE);
        let to = loi.saturating_add(PAD_AFTER).min(last);
        for line in from..=to {
            show.insert(line);
        }
    }
    if show.is_empty() {
        return format!("\n{rel}\n");
    }

    let mut out = format!("\n{rel}:\n");
    let mut previous: Option<usize> = None;
    for line in &show {
        let gap = match previous {
            None => *line > 0,              // lines elided above the first region
            Some(prev) => *line > prev + 1, // lines elided between regions
        };
        if gap {
            out.push_str(ELISION);
            out.push('\n');
        }
        out.push(GUTTER);
        out.push_str(&truncate(lines[*line], MAX_LINE_LEN));
        out.push('\n');
        previous = Some(*line);
    }
    if previous.is_some_and(|prev| prev < last) {
        out.push_str(ELISION);
        out.push('\n');
    }
    out
}

/// Truncate to at most `max` chars, never splitting a multi-byte character.
fn truncate(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        line.to_string()
    } else {
        line.chars().take(max).collect()
    }
}

/// Estimate the token cost of `text` (§6.5 step 6): tokenize ≤ 100
/// evenly-spaced lines and scale by the total/sampled character ratio.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    if text.len() < 200 {
        return count_tokens(text);
    }
    let lines: Vec<&str> = text.lines().collect();
    let step = (lines.len() / SAMPLE_LINES).max(1);
    let mut sampled_chars = 0usize;
    let mut sampled_tokens = 0usize;
    for line in lines.iter().step_by(step) {
        sampled_chars += line.chars().count();
        sampled_tokens += count_tokens(line);
    }
    if sampled_chars == 0 {
        return 0;
    }
    let total_chars = text.chars().count();
    (sampled_tokens as f64 * total_chars as f64 / sampled_chars as f64).ceil() as usize
}

/// Deterministic BPE-ish heuristic: identifier/alphanumeric runs cost
/// `ceil(len / 5)` tokens (≈5 chars/token average), each punctuation
/// character one token. Monotone in text length, which is what the
/// budget-fitting loop relies on.
fn count_tokens(s: &str) -> usize {
    fn flush(run: usize, tokens: &mut usize) {
        if run > 0 {
            *tokens += run.div_ceil(5);
        }
    }
    let mut tokens = 0usize;
    let mut run = 0usize;
    for ch in s.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            run += 1;
        } else {
            flush(run, &mut tokens);
            run = 0;
            if !ch.is_whitespace() {
                tokens += 1;
            }
        }
    }
    flush(run, &mut tokens);
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loi(rel: &str, line: usize) -> RankedLoi {
        RankedLoi {
            rel: rel.to_owned(),
            line: Some(line),
        }
    }

    #[test]
    fn gaps_between_shown_regions_are_marked() {
        let mut body = String::from("pub fn first() {}\n");
        for i in 0..40 {
            body.push_str(&format!("// filler {i}\n"));
        }
        body.push_str("pub fn second() {}\n");
        let read = |_: &str| Some(body.clone());
        let text = render_file(
            "a.rs",
            &[0usize, 41].into_iter().collect(),
            &read("a.rs").unwrap(),
        );
        assert!(text.contains(ELISION), "{text}");
        // The elided middle must never be presented as contiguous: every
        // boundary between non-adjacent shown lines carries the marker.
        let elisions = text.matches(ELISION).count();
        assert_eq!(elisions, 1, "{text}");
        assert!(text.lines().any(|l| l.starts_with(GUTTER)), "{text}");
    }

    #[test]
    fn a_tight_budget_yields_fewer_definitions_not_nothing() {
        // The C1 regression: fitting drops definitions, so a small budget
        // still renders something useful.
        let mut body = String::new();
        for i in 0..60 {
            body.push_str(&format!("pub fn item_{i}() {{\n    work();\n}}\n\n"));
        }
        let read = move |_: &str| Some(body.clone());
        let lois: Vec<RankedLoi> = (0..60).map(|i| loi("big.rs", i * 4)).collect();
        let chat = BTreeSet::new();

        let small = fit_map(&lois, &chat, &read, 120);
        assert!(!small.is_empty(), "a tight budget must still render");
        assert!(estimate_tokens(&small) <= 120);

        let large = fit_map(&lois, &chat, &read, 4096);
        assert!(large.len() > small.len(), "a larger budget shows more");
        assert!(estimate_tokens(&large) <= 4096);
    }

    #[test]
    fn chat_files_never_render() {
        let read = |_: &str| Some("pub fn a() {}\n".to_owned());
        let lois = vec![loi("a.rs", 0), loi("b.rs", 0)];
        let chat: BTreeSet<String> = ["a.rs".to_owned()].into_iter().collect();
        let text = fit_map(&lois, &chat, &read, 4096);
        assert!(!text.contains("a.rs"), "{text}");
        assert!(text.contains("b.rs"), "{text}");
    }

    #[test]
    fn stale_line_numbers_never_panic() {
        let read = |_: &str| Some("one\ntwo\n".to_owned());
        let lois = vec![loi("a.rs", 999)];
        let chat = BTreeSet::new();
        let text = fit_map(&lois, &chat, &read, 4096);
        assert!(text.contains("a.rs"), "{text}");
    }
}
