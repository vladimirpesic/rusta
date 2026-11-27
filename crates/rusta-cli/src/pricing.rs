//! What a model request costs, for `/usage`.
//!
//! Rusta knows the prices its vendors publish for their pay-per-token APIs,
//! and uses them only on the endpoint they are published for:
//!
//! - **DeepSeek** (`https://api.deepseek.com`): `deepseek-flash` and
//!   `deepseek-v4-pro`, at the peak rate on weekdays from 01:00 to 04:00 and
//!   from 06:00 to 10:00 UTC, and at half of it otherwise. DeepSeek charges
//!   the off-peak rate on Chinese public holidays too, which Rusta does not
//!   know, so a holiday's peak hours are counted at the peak rate.
//! - **Z.AI** outside China (`https://api.z.ai/api/paas/v4`): the GLM models
//!   of its price list.
//!
//! A Coding Plan is a subscription: its requests have no price per token. A
//! model's prices can be set in the config (`[prices."<model>"]`), for any
//! endpoint; they win over the built-in ones.

use std::collections::BTreeMap;

use agent_core::usage_ledger::Pricer;
use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};
use lm_bridge::usage::TokenUsage;
use serde::Deserialize;

/// A model's prices, in US dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    /// An input token the cache did not serve.
    pub input: f64,
    /// An input token read from the cache; the input price when unset.
    pub cached_input: Option<f64>,
    /// An input token written to the cache, where that is billed apart; the
    /// input price when unset.
    pub cache_write: Option<f64>,
    /// An output token, reasoning included.
    pub output: f64,
}

impl ModelPrice {
    const fn flat(input: f64, cached_input: f64, output: f64) -> Self {
        Self {
            input,
            cached_input: Some(cached_input),
            cache_write: None,
            output,
        }
    }

    /// Whether every price is a finite amount of no less than nothing.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        [
            Some(self.input),
            self.cached_input,
            self.cache_write,
            Some(self.output),
        ]
        .into_iter()
        .flatten()
        .all(|price| price.is_finite() && price >= 0.0)
    }

    /// What `usage` costs at these prices. The cache's reads and writes are
    /// part of the input tokens a provider reports.
    #[allow(clippy::cast_precision_loss)]
    fn cost(&self, usage: &TokenUsage) -> f64 {
        let read = usage.input_cache_read.unwrap_or(0).min(usage.input_tokens);
        let written = usage
            .input_cache_creation
            .unwrap_or(0)
            .min(usage.input_tokens - read);
        let fresh = usage.input_tokens - read - written;
        (fresh as f64 * self.input
            + read as f64 * self.cached_input.unwrap_or(self.input)
            + written as f64 * self.cache_write.unwrap_or(self.input)
            + usage.output_tokens as f64 * self.output)
            / 1_000_000.0
    }
}

/// DeepSeek's off-peak prices (https://api-docs.deepseek.com/quick_start/pricing);
/// its peak hours cost twice as much.
const DEEPSEEK: &[(&str, ModelPrice)] = &[
    ("deepseek-flash", ModelPrice::flat(0.15, 0.003, 0.6)),
    ("deepseek-v4-pro", ModelPrice::flat(0.66, 0.022, 1.98)),
];

/// Z.AI's prices outside China (https://docs.z.ai/guides/overview/pricing).
/// Storing the cache is free for now; GLM-4.7-Flash and GLM-4.5-Flash cost
/// nothing.
const ZAI: &[(&str, ModelPrice)] = &[
    ("glm-5.3", ModelPrice::flat(1.4, 0.26, 4.4)),
    ("glm-5.3-flash", ModelPrice::flat(0.15, 0.03, 0.5)),
    ("glm-5.3-flashx", ModelPrice::flat(0.37, 0.075, 1.25)),
    ("glm-5.2", ModelPrice::flat(1.4, 0.26, 4.4)),
    ("glm-5.1", ModelPrice::flat(1.4, 0.26, 4.4)),
    ("glm-5", ModelPrice::flat(1.0, 0.2, 3.2)),
    ("glm-4.7", ModelPrice::flat(0.6, 0.11, 2.2)),
    ("glm-4.7-flashx", ModelPrice::flat(0.07, 0.01, 0.4)),
    ("glm-4.7-flash", ModelPrice::flat(0.0, 0.0, 0.0)),
    ("glm-4.6", ModelPrice::flat(0.6, 0.11, 2.2)),
    ("glm-4.5", ModelPrice::flat(0.6, 0.11, 2.2)),
    ("glm-4.5-x", ModelPrice::flat(2.2, 0.45, 8.9)),
    ("glm-4.5-air", ModelPrice::flat(0.2, 0.03, 1.1)),
    ("glm-4.5-airx", ModelPrice::flat(1.1, 0.22, 4.5)),
    ("glm-4.5-flash", ModelPrice::flat(0.0, 0.0, 0.0)),
];

const DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com";
const ZAI_ENDPOINT: &str = "https://api.z.ai/api/paas/v4";

/// Whose published prices apply to the session's endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Published {
    DeepSeek,
    Zai,
    None,
}

/// The prices of a session's requests: the configured ones, then those
/// published for its endpoint.
#[derive(Debug, Clone)]
pub struct Prices {
    configured: BTreeMap<String, ModelPrice>,
    published: Published,
}

impl Prices {
    /// The prices for a session of `cli` on `provider`, at the endpoint its
    /// requests go to, with `configured` ones (`[prices]`) winning.
    #[must_use]
    pub fn for_session(
        cli: &crate::cli::Cli,
        provider: &str,
        configured: &BTreeMap<String, ModelPrice>,
    ) -> Self {
        let base_url = crate::provider_setup::resolve_base_url(
            cli,
            provider,
            cli.billing.unwrap_or_default(),
            cli.region.unwrap_or_default(),
        );
        Self::for_endpoint(&base_url, configured)
    }

    /// The prices for requests to `base_url`, with `configured` ones
    /// (`[prices]`) winning. An invalid configured price is left out.
    #[must_use]
    pub fn for_endpoint(base_url: &str, configured: &BTreeMap<String, ModelPrice>) -> Self {
        let endpoint = base_url.trim_end_matches('/');
        let published = if endpoint == DEEPSEEK_ENDPOINT
            || endpoint.strip_suffix("/v1") == Some(DEEPSEEK_ENDPOINT)
        {
            Published::DeepSeek
        } else if endpoint == ZAI_ENDPOINT {
            Published::Zai
        } else {
            Published::None
        };
        Self {
            configured: configured
                .iter()
                .filter(|(_, price)| price.is_valid())
                .map(|(model, price)| (model.to_ascii_lowercase(), *price))
                .collect(),
            published,
        }
    }
}

impl Pricer for Prices {
    fn cost(&self, model: &str, usage: &TokenUsage, at: DateTime<Utc>) -> Option<f64> {
        let model = model.to_ascii_lowercase();
        if let Some(price) = self.configured.get(&model) {
            return Some(price.cost(usage));
        }
        let listed = |list: &[(&str, ModelPrice)]| {
            list.iter()
                .find(|(name, _)| *name == model)
                .map(|(_, price)| price.cost(usage))
        };
        match self.published {
            Published::DeepSeek => {
                listed(DEEPSEEK).map(|cost| if deepseek_peak(at) { cost * 2.0 } else { cost })
            }
            Published::Zai => listed(ZAI),
            Published::None => None,
        }
    }
}

/// Whether `at` falls in DeepSeek's peak hours: weekdays, 01:00 to 04:00 and
/// 06:00 to 10:00 UTC.
fn deepseek_peak(at: DateTime<Utc>) -> bool {
    !matches!(at.weekday(), Weekday::Sat | Weekday::Sun) && matches!(at.hour(), 1..=3 | 6..=9)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Friday 27 November 2026, at `hour` UTC; Saturday is the 28th.
    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, hour, 30, 0).unwrap()
    }

    #[test]
    fn deepseek_costs_twice_as_much_in_its_peak_hours() {
        let prices = Prices::for_endpoint("https://api.deepseek.com/", &BTreeMap::new());
        // A million tokens in, 900,000 of them from the cache, 100,000 out.
        let used = usage(1_000_000, 900_000, 100_000);
        let off_peak = 100_000.0 * 0.15 / 1e6 + 900_000.0 * 0.003 / 1e6 + 100_000.0 * 0.6 / 1e6;
        let cost = |when| prices.cost("deepseek-flash", &used, when).unwrap();
        assert!(close(cost(at(27, 0)), off_peak));
        assert!(close(cost(at(27, 2)), off_peak * 2.0));
        assert!(close(cost(at(27, 5)), off_peak));
        assert!(close(cost(at(27, 9)), off_peak * 2.0));
        assert!(close(cost(at(27, 10)), off_peak));
        assert!(close(cost(at(28, 2)), off_peak), "a Saturday is off-peak");
        let pro = prices.cost("deepseek-v4-pro", &used, at(27, 0)).unwrap();
        assert!(close(
            pro,
            100_000.0 * 0.66 / 1e6 + 900_000.0 * 0.022 / 1e6 + 100_000.0 * 1.98 / 1e6
        ));
        assert_eq!(prices.cost("deepseek-chat", &used, at(27, 0)), None);
        let v1 = Prices::for_endpoint("https://api.deepseek.com/v1", &BTreeMap::new());
        assert!(v1.cost("deepseek-flash", &used, at(27, 0)).is_some());
    }

    #[test]
    fn zai_prices_apply_on_its_pay_per_token_endpoint_alone() {
        let used = usage(1_000_000, 500_000, 0);
        let api = Prices::for_endpoint("https://api.z.ai/api/paas/v4", &BTreeMap::new());
        assert!(close(
            api.cost("GLM-5.3", &used, at(27, 2)).unwrap(),
            500_000.0 * 1.4 / 1e6 + 500_000.0 * 0.26 / 1e6
        ));
        assert_eq!(
            api.cost("glm-5.4", &used, at(27, 2)),
            None,
            "not on the list"
        );
        for subscription_or_other in [
            "https://api.z.ai/api/coding/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
            "https://api.kimi.com/coding/v1",
            "https://api.moonshot.ai/v1",
            "http://localhost:8080/v1",
        ] {
            let prices = Prices::for_endpoint(subscription_or_other, &BTreeMap::new());
            assert_eq!(prices.cost("glm-5.3", &used, at(27, 2)), None);
            assert_eq!(prices.cost("deepseek-flash", &used, at(27, 2)), None);
        }
    }

    /// A configured price wins, on any endpoint, and an invalid one is left
    /// out. Cache writes are billed apart where a price says so.
    #[test]
    fn configured_prices_win_and_invalid_ones_are_left_out() {
        let mut configured = BTreeMap::new();
        configured.insert(
            "Kimi-K3.1-Pro".to_string(),
            ModelPrice {
                input: 2.0,
                cached_input: Some(0.5),
                cache_write: Some(3.0),
                output: 8.0,
            },
        );
        configured.insert(
            "deepseek-flash".to_string(),
            ModelPrice::flat(1.0, 1.0, 1.0),
        );
        configured.insert("broken".to_string(), ModelPrice::flat(-1.0, 0.0, 0.0));
        let kimi = Prices::for_endpoint("https://api.moonshot.ai/v1", &configured);
        let used = TokenUsage {
            input_tokens: 1_000_000,
            input_cache_read: Some(600_000),
            input_cache_creation: Some(300_000),
            output_tokens: 100_000,
            ..TokenUsage::empty()
        };
        assert!(close(
            kimi.cost("kimi-k3.1-pro", &used, at(27, 2)).unwrap(),
            (100_000.0 * 2.0 + 600_000.0 * 0.5 + 300_000.0 * 3.0 + 100_000.0 * 8.0) / 1e6
        ));
        assert_eq!(kimi.cost("broken", &used, at(27, 2)), None);
        let deepseek = Prices::for_endpoint(DEEPSEEK_ENDPOINT, &configured);
        assert!(close(
            deepseek.cost("deepseek-flash", &used, at(27, 2)).unwrap(),
            1.1,
        ));
    }
}
