//! Token estimation — development plan §6.2.
//!
//! The HTTP backend estimates tokens with the heuristic `ceil(chars / 3)`:
//! deliberately conservative (English source code averages ~3.3–4 chars/token,
//! so `/3` overestimates slightly and budgets stay safe). This module is the
//! single swap-in point for a real tokenizer if empirical drift demands one;
//! the embedded backend counts exactly (M1.5).

/// Estimated token count for `text`.
pub fn estimate_tokens(text: &str) -> u64 {
    text.chars().count().div_ceil(3) as u64
}

#[cfg(test)]
mod tests {
    use super::estimate_tokens;

    #[test]
    fn empty_text_is_zero_tokens() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn rounds_up_to_whole_tokens() {
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens("ab"), 1);
    }

    #[test]
    fn counts_characters_not_bytes() {
        // 4 chars, 8 UTF-8 bytes → 2 tokens, not 3.
        assert_eq!(estimate_tokens("déjà"), 2);
    }
}
