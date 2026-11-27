//! What a session's model requests used and cost, for `/usage`.
//!
//! Every request a session makes — its own agent's steps, its sub-agents',
//! the handoffs and the summaries that compaction writes — is recorded here
//! as the provider reported it: the prompt's tokens, the part read from the
//! provider's prompt cache and the part written to it, and the reply's. One
//! ledger serves the whole tree of agents, so its totals are the session's.
//!
//! A request's cost is the [`Pricer`]'s to say, when one is set: prices
//! depend on the provider, the plan and, at DeepSeek, the hour, which the
//! front-end knows and this crate does not. A request of a model the pricer
//! has no price for is counted apart, so a total never passes for complete
//! when it is not.

use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Utc};
use lm_bridge::usage::TokenUsage;

use crate::sync::LockExt;

/// What a request's tokens cost.
pub trait Pricer: Send + Sync {
    /// What `usage` of `model`, requested at `at`, cost in US dollars; `None`
    /// when the price of `model` is not known.
    fn cost(&self, model: &str, usage: &TokenUsage, at: DateTime<Utc>) -> Option<f64>;
}

/// A session's requests so far.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UsageSummary {
    /// The requests made.
    pub requests: u64,
    /// Their tokens, summed.
    pub usage: TokenUsage,
    /// What the requests with a known price cost, in US dollars.
    pub cost_usd: f64,
    /// The requests whose price is not known.
    pub unpriced: u64,
    /// The most recent request's tokens.
    pub last: Option<TokenUsage>,
}

/// The ledger of a session's requests, shared by its agents.
#[derive(Default)]
pub struct UsageLedger {
    summary: Mutex<UsageSummary>,
    pricer: RwLock<Option<Arc<dyn Pricer>>>,
}

impl std::fmt::Debug for UsageLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageLedger")
            .field("summary", &*self.summary.lock_or_recover())
            .finish_non_exhaustive()
    }
}

impl UsageLedger {
    /// Price the requests recorded from now on with `pricer`; `None` prices
    /// none.
    pub fn set_pricer(&self, pricer: Option<Arc<dyn Pricer>>) {
        *self
            .pricer
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = pricer;
    }

    /// Record a request of `model` that used `usage`, made now. A request
    /// that reported no tokens at all is not counted: nothing says it reached
    /// the model.
    pub fn record(&self, model: &str, usage: &TokenUsage) {
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            return;
        }
        let cost = self
            .pricer
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|pricer| pricer.cost(model, usage, Utc::now()));
        let mut summary = self.summary.lock_or_recover();
        summary.requests += 1;
        summary.usage.add(usage);
        match cost {
            Some(cost) => summary.cost_usd += cost,
            None => summary.unpriced += 1,
        }
        summary.last = Some(*usage);
    }

    /// The requests recorded so far.
    #[must_use]
    pub fn summary(&self) -> UsageSummary {
        self.summary.lock_or_recover().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dollar a million tokens of `priced`, read or not; nothing else known.
    struct Flat;

    impl Pricer for Flat {
        fn cost(&self, model: &str, usage: &TokenUsage, _at: DateTime<Utc>) -> Option<f64> {
            #[allow(clippy::cast_precision_loss)]
            (model == "priced")
                .then(|| (usage.input_tokens + usage.output_tokens) as f64 / 1_000_000.0)
        }
    }

    fn usage(input: u64, read: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            input_cache_read: Some(read),
            cached_tokens: Some(read),
            ..TokenUsage::empty()
        }
    }

    #[test]
    fn requests_are_summed_and_priced_where_a_price_is_known() {
        let ledger = UsageLedger::default();
        ledger.record("priced", &usage(900_000, 800_000, 100_000));
        assert_eq!(ledger.summary().unpriced, 1, "no pricer yet");
        ledger.set_pricer(Some(Arc::new(Flat)));
        ledger.record("priced", &usage(900_000, 850_000, 100_000));
        ledger.record("other", &usage(10, 0, 5));
        ledger.record("priced", &TokenUsage::empty());

        let summary = ledger.summary();
        assert_eq!(summary.requests, 3);
        assert_eq!(summary.usage.input_tokens, 1_800_010);
        assert_eq!(summary.usage.input_cache_read, Some(1_650_000));
        assert_eq!(summary.usage.output_tokens, 200_005);
        assert!(
            (summary.cost_usd - 1.0).abs() < 1e-9,
            "{}",
            summary.cost_usd
        );
        assert_eq!(summary.unpriced, 2);
        assert_eq!(summary.last, Some(usage(10, 0, 5)));
    }
}
