//! Budget-fitted rendering — DEVELOPMENT_PLAN.md §6.5 steps 5–6.
//!
//! Files render in rank order, chat files excluded (their content is already
//! in context). Each file shows its definition lines as lines-of-interest
//! padded by up to 8 surrounding context lines, headed by `path/to/file.rs:`;
//! tag-less files appear as bare paths. Every rendered line is truncated to
//! 100 chars. Token cost is estimated from ≤ 100 evenly-spaced lines scaled by
//! the character ratio (Aider's sampling trick); while over budget the
//! middle-ranked files are dropped — top and bottom of the ranking are the
//! most informative — and the largest fitting keep-set wins.

use crate::graph::RankedFile;
use std::collections::BTreeSet;

/// Context lines around each definition (§6.5 step 5).
const CONTEXT_LINES: usize = 8;
/// Hard cap on rendered line length in chars (§6.5 step 5).
const MAX_LINE_LEN: usize = 100;
/// Lines sampled for token-cost estimation (§6.5 step 6).
const SAMPLE_LINES: usize = 100;

/// Render the budget-fitted map for rank-ordered `files`, reading each source
/// via `read` (repo-relative path → file text). Never exceeds `budget_tokens`
/// as estimated by [`estimate_tokens`]; an un-fittable map renders empty.
pub(crate) fn fit_map(
    files: &[RankedFile],
    chat_files: &BTreeSet<String>,
    read: &dyn Fn(&str) -> Option<String>,
    budget_tokens: usize,
) -> String {
    let visible: Vec<&RankedFile> = files
        .iter()
        .filter(|f| !chat_files.contains(&f.rel))
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    // Binary-search the largest keep-count whose render fits. Kept files are
    // the head and tail of the ranking; the middle is dropped first.
    let mut best: Option<String> = None;
    let (mut lo, mut hi) = (1usize, visible.len());
    while lo <= hi {
        let keep = (lo + hi) / 2;
        let text = render_subset(&visible, keep, read);
        if estimate_tokens(&text) <= budget_tokens {
            best = Some(text);
            lo = keep + 1;
        } else {
            hi = keep - 1;
        }
    }
    best.unwrap_or_default()
}

/// Render the first `head` and last `keep - head` files of `visible`.
fn render_subset(
    visible: &[&RankedFile],
    keep: usize,
    read: &dyn Fn(&str) -> Option<String>,
) -> String {
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out = String::new();
    for file in visible
        .iter()
        .take(head)
        .chain(visible.iter().skip(head).rev().take(tail).rev())
    {
        if let Some(source) = read(&file.rel) {
            out.push_str(&render_file(file, &source));
        }
    }
    out
}

/// One file block: header plus context-padded definition lines, or a bare
/// path when there is nothing to show.
fn render_file(file: &RankedFile, source: &str) -> String {
    if file.lois.is_empty() {
        return format!("\n{}\n", file.rel);
    }
    let lines: Vec<&str> = source.lines().collect();
    let last = lines.len().saturating_sub(1);
    let mut show: BTreeSet<usize> = BTreeSet::new();
    for &loi in &file.lois {
        for l in loi.saturating_sub(CONTEXT_LINES)..=loi.saturating_add(CONTEXT_LINES).min(last) {
            show.insert(l);
        }
    }
    let mut out = format!("\n{}:\n", file.rel);
    for l in show {
        out.push_str(&truncate(lines.get(l).copied().unwrap_or(""), MAX_LINE_LEN));
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
