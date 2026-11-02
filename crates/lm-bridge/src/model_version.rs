//! Resolving metadata for a model whose exact id is not yet catalogued.
//!
//! Vendors ship a new minor version well before we can update a table. Dropping
//! straight to a conservative default means a 1M-context model gets sized as a
//! 128k one, and the agent compacts its context eight times more often than it
//! needs to. A new minor has never *lowered* a model's context window, so an
//! uncatalogued `glm-5.3` inherits `glm-5.2`'s window rather than the default.

use std::collections::HashMap;

/// Longest minor version this module will walk down from. Two digits bounds the
/// search and covers every version scheme either vendor has used.
const MAX_MINOR_DIGITS: usize = 2;

/// A model id split around its `<major>.<minor>` version.
///
/// `glm-5.2` → prefix `glm-`, major 5, minor 2, suffix ``.
/// `kimi-k2.7-code` → prefix `kimi-k`, major 2, minor 7, suffix `-code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelVersion<'a> {
    pub prefix: &'a str,
    pub major: &'a str,
    pub minor: u32,
    pub suffix: &'a str,
}

impl ModelVersion<'_> {
    /// Rebuilds the model id with `minor` swapped in.
    fn with_minor(&self, minor: u32) -> String {
        format!("{}{}.{}{}", self.prefix, self.major, minor, self.suffix)
    }
}

/// Splits `model` around its first `<digits>.<digits>` version.
///
/// Returns `None` when the id carries no minor version — `glm-5-turbo` and
/// `moonshot-v1-8k` name no minor, so there is nothing to walk down.
pub fn parse(model: &str) -> Option<ModelVersion<'_>> {
    let bytes = model.as_bytes();
    let dot = model.char_indices().find_map(|(i, c)| {
        if c != '.' {
            return None;
        }
        let has_major = i > 0 && bytes[i - 1].is_ascii_digit();
        let has_minor = bytes.get(i + 1).is_some_and(u8::is_ascii_digit);
        (has_major && has_minor).then_some(i)
    })?;

    let major_start = bytes[..dot]
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |i| i + 1);
    let minor_end = bytes[dot + 1..]
        .iter()
        .position(|b| !b.is_ascii_digit())
        .map_or(model.len(), |i| dot + 1 + i);

    let minor_str = &model[dot + 1..minor_end];
    if minor_str.len() > MAX_MINOR_DIGITS {
        return None;
    }

    Some(ModelVersion {
        prefix: &model[..major_start],
        major: &model[major_start..dot],
        minor: minor_str.parse().ok()?,
        suffix: &model[minor_end..],
    })
}

/// Looks `model` up in `table`, falling back to the nearest lower catalogued
/// minor of the same family, major version, and variant suffix.
///
/// `glm-5.3` resolves to `glm-5.2`'s entry; `kimi-k2.9-code` resolves to
/// `kimi-k2.7-code`'s, never to plain `kimi-k2.6`'s — a variant suffix names a
/// different model, not a newer one.
pub fn lookup_with_minor_fallback(model: &str, table: &HashMap<&str, u64>) -> Option<u64> {
    if let Some(exact) = table.get(model).copied() {
        return Some(exact);
    }
    let version = parse(model)?;
    (0..version.minor)
        .rev()
        .find_map(|candidate| table.get(version.with_minor(candidate).as_str()).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> HashMap<&'static str, u64> {
        [
            ("glm-5.1", 128_000u64),
            ("glm-5.2", 1_048_576),
            ("glm-4.6", 200_000),
            ("glm-4.7", 200_000),
            ("glm-4.5-air", 128_000),
            ("glm-5-turbo", 128_000),
            ("kimi-k2.6", 262_144),
            ("kimi-k2.7-code", 262_144),
            ("moonshot-v1-8k", 8_192),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn parses_a_plain_minor_version() {
        let v = parse("glm-5.2").unwrap();
        assert_eq!((v.prefix, v.major, v.minor, v.suffix), ("glm-", "5", 2, ""));
    }

    #[test]
    fn parses_a_suffixed_minor_version() {
        let v = parse("kimi-k2.7-code").unwrap();
        assert_eq!(
            (v.prefix, v.major, v.minor, v.suffix),
            ("kimi-k", "2", 7, "-code")
        );
    }

    #[test]
    fn parses_a_bracketed_suffix() {
        let v = parse("glm-5.1[1m]").unwrap();
        assert_eq!(
            (v.prefix, v.major, v.minor, v.suffix),
            ("glm-", "5", 1, "[1m]")
        );
    }

    /// Ids without a `<digits>.<digits>` version have no minor to walk down.
    #[test]
    fn rejects_ids_without_a_minor_version() {
        for id in ["glm-5-turbo", "moonshot-v1-8k", "kimi-for-coding", "", "."] {
            assert_eq!(parse(id), None, "{id:?}");
        }
    }

    /// A date-like id must not be mistaken for a version and walked down 9999
    /// steps.
    #[test]
    fn rejects_over_long_minor_versions() {
        assert_eq!(parse("model-1.2026"), None);
        assert_eq!(parse("model-1.99").map(|v| v.minor), Some(99));
    }

    #[test]
    fn round_trips_through_with_minor() {
        assert_eq!(parse("glm-5.2").unwrap().with_minor(1), "glm-5.1");
        assert_eq!(
            parse("kimi-k2.7-code").unwrap().with_minor(3),
            "kimi-k2.3-code"
        );
    }

    #[test]
    fn exact_match_wins() {
        assert_eq!(
            lookup_with_minor_fallback("glm-5.2", &table()),
            Some(1_048_576)
        );
        assert_eq!(
            lookup_with_minor_fallback("glm-5.1", &table()),
            Some(128_000)
        );
    }

    /// The motivating case: a minor the table has never heard of.
    #[test]
    fn uncatalogued_minor_inherits_its_predecessor() {
        assert_eq!(
            lookup_with_minor_fallback("glm-5.3", &table()),
            Some(1_048_576),
            "glm-5.3 must inherit glm-5.2's 1M window, not the 128k default"
        );
    }

    /// The walk skips gaps: 5.9 → (no 5.8..5.3) → 5.2.
    #[test]
    fn walk_skips_uncatalogued_minors() {
        assert_eq!(
            lookup_with_minor_fallback("glm-5.9", &table()),
            Some(1_048_576)
        );
    }

    /// It walks *down*, never up: a 5.0 must not pick up 5.1's window.
    #[test]
    fn walk_never_climbs_to_a_higher_minor() {
        assert_eq!(lookup_with_minor_fallback("glm-5.0", &table()), None);
    }

    /// A variant suffix names a different model. `kimi-k2.9-code` inherits from
    /// `kimi-k2.7-code`, and plain `kimi-k2.9` from plain `kimi-k2.6`.
    #[test]
    fn suffix_must_match_for_a_fallback() {
        assert_eq!(
            lookup_with_minor_fallback("kimi-k2.9-code", &table()),
            Some(262_144)
        );
        assert_eq!(
            lookup_with_minor_fallback("kimi-k2.9", &table()),
            Some(262_144)
        );
        // ...and a suffix with no catalogued sibling finds nothing.
        assert_eq!(
            lookup_with_minor_fallback("kimi-k2.9-vision", &table()),
            None
        );
    }

    /// A different major is a different lineage.
    #[test]
    fn fallback_never_crosses_a_major_version() {
        assert_eq!(
            lookup_with_minor_fallback("glm-4.9", &table()),
            Some(200_000),
            "glm-4.9 inherits glm-4.7, not glm-5.2"
        );
        assert_eq!(lookup_with_minor_fallback("glm-6.1", &table()), None);
    }

    /// `glm-4.5-air` is catalogued; `glm-4.9-air` inherits it across the gap at
    /// 4.6/4.7 (which exist, but only without the suffix).
    #[test]
    fn suffixed_fallback_ignores_unsuffixed_siblings() {
        assert_eq!(
            lookup_with_minor_fallback("glm-4.9-air", &table()),
            Some(128_000)
        );
    }

    #[test]
    fn unknown_family_resolves_to_nothing() {
        assert_eq!(lookup_with_minor_fallback("llama-3.1", &table()), None);
        assert_eq!(lookup_with_minor_fallback("gpt-4o-mini", &table()), None);
    }
}
