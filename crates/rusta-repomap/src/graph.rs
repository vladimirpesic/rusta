//! Reference graph and personalized PageRank ranking — §6.5 steps 3–4.
//!
//! One node per file. Each identifier defined in `D` files and referenced `n_r`
//! times by a file `r` contributes edges `r → definer` of weight
//! `mul · √n_r` — Aider's damping, so heavier use pulls rank *toward* the
//! definer (see the §6.5 step 3 erratum; dividing inverted the signal) —
//! with the ADR's boost/suppression rules; identifiers
//! defined but never referenced keep their files rankable through a 0.1
//! self-edge. Ranking is power-iteration PageRank (damping 0.85,
//! personalization `100/N` baseline plus boosts, dangling mass redistributed
//! by the personalization vector).

use crate::tags::{Tag, TagKind};
use std::collections::{BTreeMap, BTreeSet};

/// Damping factor (§6.5 step 4).
const DAMPING: f64 = 0.85;
/// Convergence threshold for the L1 rank delta (§6.5 step 4).
const TOLERANCE: f64 = 1e-6;
/// Max power iterations (§6.5 step 4).
const MAX_ITERATIONS: usize = 100;
/// Self-edge weight for defined-but-never-referenced identifiers.
const SELF_EDGE_WEIGHT: f64 = 0.1;
/// Aider's edge multiplier when the *referencing* file is in the chat set.
const CHAT_REFERENCER_BOOST: f64 = 50.0;

/// User mentions steering personalization (§6.5 step 4).
#[derive(Debug, Default, Clone)]
pub(crate) struct Mentions {
    /// Mentioned files, as repo-relative paths.
    pub(crate) files: BTreeSet<String>,
    /// Mentioned identifiers (from the conversation).
    pub(crate) idents: BTreeSet<String>,
}

/// One ranked line of interest: a definition line in a file, positioned by
/// the ranking pass. `line: None` marks a file that produced no tags at all
/// (Aider's `rel_other_fnames_without_tags`) — rendered as a bare path.
///
/// The list is flat and rank-ordered on purpose: §6.5 step 6 fitting drops
/// the lowest-ranked *definitions*, not whole files, so a tight budget
/// degrades to fewer definitions rather than to nothing (Aider fits over
/// `ranked_tags[:middle]` the same way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankedLoi {
    pub(crate) rel: String,
    pub(crate) line: Option<usize>,
}

type EdgeList<'a> = BTreeMap<&'a str, Vec<(&'a str, f64, &'a str)>>;

/// Rank `file_tags` (repo-relative path → tags) and return files in rank
/// order. Chat files keep their graph role (they pull rank towards what the
/// session is working on) — only the renderer excludes them.
pub(crate) fn rank_files(
    file_tags: &BTreeMap<String, Vec<Tag>>,
    chat_files: &BTreeSet<String>,
    mentions: &Mentions,
) -> Vec<RankedLoi> {
    if file_tags.is_empty() {
        return Vec::new();
    }

    // Identifier tables (Aider's defines/references/definitions).
    let mut defines: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut references: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut def_lines: BTreeMap<(&str, &str), BTreeSet<usize>> = BTreeMap::new();
    for (rel, tags) in file_tags {
        for tag in tags {
            match tag.kind {
                TagKind::Def => {
                    defines.entry(tag.name.as_str()).or_default().insert(rel);
                    def_lines
                        .entry((rel.as_str(), tag.name.as_str()))
                        .or_default()
                        .insert(tag.line);
                }
                TagKind::Ref => {
                    references.entry(tag.name.as_str()).or_default().push(rel);
                }
            }
        }
    }

    // Personalization: 100/N baseline, +100/N for chat/mentioned files,
    // +100/N when a path component matches a mentioned identifier.
    let baseline = 100.0 / file_tags.len() as f64;
    let mut personalization: BTreeMap<&str, f64> = BTreeMap::new();
    for rel in file_tags.keys() {
        let mut pers = baseline;
        if chat_files.contains(rel) || mentions.files.contains(rel) {
            pers += baseline;
        }
        let stem = rel.rsplit('/').next().unwrap_or(rel);
        let no_ext = stem.split_once('.').map_or(stem, |(s, _)| s);
        if rel.split('/').any(|c| mentions.idents.contains(c))
            || mentions.idents.contains(stem)
            || mentions.idents.contains(no_ext)
        {
            pers += baseline;
        }
        personalization.insert(rel, pers);
    }

    // Edge list per source: (destination, weight, identifier).
    let mut edges: EdgeList = BTreeMap::new();

    // Defined but never referenced: 0.1 self-edges keep singletons rankable.
    for (ident, definers) in &defines {
        if references.contains_key(ident) {
            continue;
        }
        for definer in definers {
            edges
                .entry(definer)
                .or_default()
                .push((definer, SELF_EDGE_WEIGHT, ident));
        }
    }

    // Referenced identifiers: referencer → every definer.
    //
    // Weight is Aider's `use_mul * sqrt(n_r)` (repomap.py): more references
    // pull rank *toward* the definer, damped by the square root so that
    // high-frequency (low-value) identifiers cannot dominate. The ADR's
    // original `mul / (|D| · n_r)` inverted that signal — see the §6.5
    // step 3 erratum note. Chat files get Aider's ×50 referencer boost: what
    // the session is already working on is the strongest steering signal
    // there is.
    for (ident, referencers) in &references {
        let Some(definers) = defines.get(ident) else {
            continue;
        };
        let mut mul = boost_multiplier(ident, mentions);
        if definers.len() > 5 {
            mul *= 0.1;
        }
        let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
        for r in referencers {
            *per_file.entry(r).or_insert(0) += 1;
        }
        for (referencer, n_r) in per_file {
            let use_mul = if chat_files.contains(referencer) {
                mul * CHAT_REFERENCER_BOOST
            } else {
                mul
            };
            let weight = use_mul * (n_r as f64).sqrt();
            for definer in definers {
                edges
                    .entry(referencer)
                    .or_default()
                    .push((definer, weight, ident));
            }
        }
    }

    let nodes: Vec<&str> = file_tags.keys().map(String::as_str).collect();
    let ranked = pagerank(&nodes, &edges, &personalization);
    distribute_rank(&ranked, &edges, &def_lines)
}

/// Boosts (§6.5 step 3): ×10 user-mentioned identifier; ×10 snake/kebab/
/// camelCase with length ≥ 8 characters (counted in `char`s, not bytes —
/// identifiers may be non-ASCII); ×0.1 leading underscore. (×0.1 for
/// `|D| > 5` lives in [`rank_files`].)
fn boost_multiplier(ident: &str, mentions: &Mentions) -> f64 {
    let mut mul = 1.0;
    if mentions.idents.contains(ident) {
        mul *= 10.0;
    }
    let alphabetic = ident.chars().any(char::is_alphabetic);
    let is_snake = ident.contains('_') && alphabetic;
    let is_kebab = ident.contains('-') && alphabetic;
    let is_camel = ident.chars().any(char::is_uppercase) && ident.chars().any(char::is_lowercase);
    if (is_snake || is_kebab || is_camel) && ident.chars().count() >= 8 {
        mul *= 10.0;
    }
    if ident.starts_with('_') {
        mul *= 0.1;
    }
    mul
}

/// Personalized PageRank by power iteration over `nodes`, edges, and the
/// personalization vector. Returns `(node, rank)` for every node.
fn pagerank<'a>(
    nodes: &[&'a str],
    edges: &EdgeList<'a>,
    personalization: &BTreeMap<&'a str, f64>,
) -> Vec<(&'a str, f64)> {
    let n = nodes.len() as f64;
    let pers_total: f64 = personalization.values().sum();
    // Normalize once; a zero personalization falls back to uniform.
    let p = |node: &str| {
        if pers_total > 0.0 {
            personalization.get(node).copied().unwrap_or(0.0) / pers_total
        } else {
            1.0 / n
        }
    };
    let out_weight = |node: &str| {
        edges
            .get(node)
            .map(|outs| outs.iter().map(|(_, w, _)| w).sum::<f64>())
            .unwrap_or(0.0)
    };
    let mut rank: Vec<f64> = vec![1.0 / n; nodes.len()];
    let out_weights: Vec<f64> = nodes.iter().map(|&node| out_weight(node)).collect();
    let index: std::collections::HashMap<&str, usize> =
        nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    for _ in 0..MAX_ITERATIONS {
        let mut next = vec![0.0; nodes.len()];
        // Dangling mass (sources with no out-edges) is redistributed by the
        // personalization vector, as in Aider's dangling=personalization.
        let dangling: f64 = nodes
            .iter()
            .zip(&rank)
            .zip(&out_weights)
            .filter(|&(_, &w)| w <= 0.0)
            .map(|((&_, &r), _)| r)
            .sum();
        for (i, &node) in nodes.iter().enumerate() {
            next[i] = (1.0 - DAMPING) * p(node) + DAMPING * dangling * p(node);
        }
        for (i, &src) in nodes.iter().enumerate() {
            if out_weights[i] <= 0.0 {
                continue;
            }
            for (dst, w, _) in &edges[src] {
                if let Some(&j) = index.get(dst) {
                    next[j] += DAMPING * rank[i] * w / out_weights[i];
                }
            }
        }
        let delta: f64 = next.iter().zip(&rank).map(|(a, b)| (a - b).abs()).sum();
        rank = next;
        if delta < TOLERANCE {
            break;
        }
    }
    nodes.iter().copied().zip(rank).collect()
}

/// Aider's `ranked_definitions`: distribute each source's rank across its
/// out-edges by weight share, accumulate per (file, identifier), and sort by
/// accumulated rank. The result is a flat, rank-ordered list of definition
/// lines — the unit §6.5 step 6 fitting works in.
fn distribute_rank<'a>(
    ranked: &[(&'a str, f64)],
    edges: &EdgeList<'a>,
    def_lines: &BTreeMap<(&'a str, &'a str), BTreeSet<usize>>,
) -> Vec<RankedLoi> {
    let mut scores: BTreeMap<(&str, &str), f64> = BTreeMap::new();
    for &(src, src_rank) in ranked {
        let Some(outs) = edges.get(src) else {
            continue;
        };
        let total: f64 = outs.iter().map(|(_, w, _)| w).sum();
        if total <= 0.0 {
            continue;
        }
        for &(dst, w, ident) in outs {
            *scores.entry((dst, ident)).or_insert(0.0) += src_rank * w / total;
        }
    }
    let mut order: Vec<((&str, &str), f64)> = scores.into_iter().collect();
    order.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut out: Vec<RankedLoi> = Vec::new();
    let mut seen: BTreeSet<(&str, usize)> = BTreeSet::new();
    for ((file, ident), _) in order {
        let Some(lines) = def_lines.get(&(file, ident)) else {
            continue;
        };
        for &line in lines {
            if seen.insert((file, line)) {
                out.push(RankedLoi {
                    rel: file.to_string(),
                    line: Some(line),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boost_multiplier_counts_characters_not_bytes() {
        let mentions = Mentions::default();
        // No case mix, no separator: neutral, whatever the length.
        assert_eq!(boost_multiplier("plain", &mentions), 1.0);
        assert_eq!(boost_multiplier("abcdefghij", &mentions), 1.0);
        // Multi-case/snake with ≥ 8 characters: ×10.
        assert_eq!(boost_multiplier("Abcdefgh", &mentions), 10.0);
        assert_eq!(boost_multiplier("some_snake_name", &mentions), 10.0);
        // Multi-case but fewer than 8 characters: no boost — even when the
        // byte length crosses 8 (non-ASCII identifiers must not trip a
        // byte-count check). "AbcdΩfg" is 7 chars / 8 bytes.
        assert_eq!(boost_multiplier("Abc", &mentions), 1.0);
        assert_eq!(boost_multiplier("AbcdΩfg", &mentions), 1.0);
        // Exactly 8 characters still boosts ("Ωbcdefgh" is 8 chars / 9 bytes).
        assert_eq!(boost_multiplier("Ωbcdefgh", &mentions), 10.0);
        // Leading underscore: ×0.1 (and too short for the shape boost).
        assert_eq!(boost_multiplier("_x", &mentions), 0.1);
        // A user mention stacks with the shape boost.
        let mut mentioned = Mentions::default();
        mentioned.idents.insert("some_snake_name".to_owned());
        assert_eq!(boost_multiplier("some_snake_name", &mentioned), 100.0);
    }
}
