//! `/usage`: what the session's requests used so far, how much of their input
//! the provider's prompt cache served, and what they cost
//! ([`agent_core::usage_ledger`]).

use agent_core::usage_ledger::UsageSummary;
use lm_bridge::usage::TokenUsage;

use crate::context_view::thousands;

/// The `/usage` report for `summary`.
#[must_use]
pub fn render(summary: &UsageSummary) -> String {
    if summary.requests == 0 {
        return "No model requests yet in this session.".to_string();
    }
    let usage = &summary.usage;
    let noun = if summary.requests == 1 {
        "request"
    } else {
        "requests"
    };
    let mut out = format!(
        "Usage in this session: {} {noun}, the sub-agents' included.\n\n",
        thousands(summary.requests)
    );
    out.push_str(&format!(
        "  Input   {} tokens{}\n",
        thousands(usage.input_tokens),
        cache_words(usage)
    ));
    out.push_str(&format!(
        "  Output  {} tokens\n",
        thousands(usage.output_tokens)
    ));
    out.push_str(&format!("  Cost    {}\n", cost_words(summary)));
    if let Some(last) = &summary.last
        && let Some(read) = last.input_cache_read
        && last.input_tokens > 0
    {
        out.push_str(&format!(
            "\nThe last request read {} of its {} input tokens from the cache.",
            percent(read, last.input_tokens),
            thousands(last.input_tokens)
        ));
    }
    out.trim_end().to_string()
}

/// What the prompt cache did for `usage`'s input, or that the provider did
/// not say.
fn cache_words(usage: &TokenUsage) -> String {
    let Some(read) = usage.input_cache_read else {
        return " (the provider reports nothing of its cache)".to_string();
    };
    let mut words = format!(
        ", {} of them ({}) read from the cache",
        thousands(read),
        percent(read, usage.input_tokens)
    );
    if let Some(written) = usage.input_cache_creation.filter(|written| *written > 0) {
        words.push_str(&format!(", {} written to it", thousands(written)));
    }
    words
}

/// What the requests cost, and how far that is known.
fn cost_words(summary: &UsageSummary) -> String {
    let priced = summary.requests - summary.unpriced;
    if priced == 0 {
        return "not known: a Coding Plan is billed by subscription, and a model's prices can \
                be set under [prices] in the config"
            .to_string();
    }
    let mut words = format!(
        "${:.4}, by the prices published or configured",
        summary.cost_usd
    );
    if summary.unpriced > 0 {
        let noun = if summary.unpriced == 1 {
            "request"
        } else {
            "requests"
        };
        words.push_str(&format!(
            "; {} {noun} had no known price",
            thousands(summary.unpriced)
        ));
    }
    words
}

/// `part` of `whole`, to a tenth of a percent.
fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".to_string();
    }
    let tenths = part.min(whole).saturating_mul(1000) / whole;
    format!("{}.{}%", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(requests: u64, unpriced: u64, cost_usd: f64, usage: TokenUsage) -> UsageSummary {
        UsageSummary {
            requests,
            usage,
            cost_usd,
            unpriced,
            last: Some(usage),
        }
    }

    #[test]
    fn a_priced_session_shows_its_tokens_cache_and_cost() {
        let usage = TokenUsage {
            input_tokens: 1_234_567,
            output_tokens: 45_678,
            input_cache_read: Some(1_100_000),
            input_cache_creation: Some(12_000),
            ..TokenUsage::empty()
        };
        let text = render(&summary(42, 3, 0.04213, usage));
        assert_eq!(
            text,
            "Usage in this session: 42 requests, the sub-agents' included.\n\n\
             \x20 Input   1,234,567 tokens, 1,100,000 of them (89.1%) read from the cache, 12,000 \
             written to it\n\
             \x20 Output  45,678 tokens\n\
             \x20 Cost    $0.0421, by the prices published or configured; 3 requests had no \
             known price\n\n\
             The last request read 89.1% of its 1,234,567 input tokens from the cache."
        );
    }

    #[test]
    fn an_unpriced_session_and_a_provider_without_cache_figures_say_so() {
        let usage = TokenUsage {
            input_tokens: 500,
            output_tokens: 20,
            ..TokenUsage::empty()
        };
        let text = render(&summary(1, 1, 0.0, usage));
        assert!(
            text.starts_with("Usage in this session: 1 request,"),
            "{text}"
        );
        assert!(
            text.contains("Input   500 tokens (the provider reports nothing of its cache)"),
            "{text}"
        );
        assert!(text.contains("Cost    not known: a Coding Plan"), "{text}");
        assert!(!text.contains("The last request"), "{text}");
        assert_eq!(
            render(&UsageSummary::default()),
            "No model requests yet in this session."
        );
    }
}
