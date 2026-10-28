//! Which endpoint and model a provider + billing + region selects.
//!
//! Both vendors sell the same models two ways, and the *key decides which plan
//! it is billed against*. Kimi refuses a Coding Plan key at the pay-per-token
//! endpoint with `invalid_authentication_error`, and vice versa. Z.AI takes a
//! key at either endpoint and bills it by its type; an older key made for an
//! account with a Coding Plan is billed by the endpoint, so at the
//! pay-per-token one it answers `1113` "Insufficient balance". Getting this
//! wrong is the single most common way to be locked out, so the pairing lives in
//! one table rather than being spread across `main`, `doctor`, and the docs.
//!
//! Every URL and model id below was verified against the live APIs:
//!
//! * Kimi's subscription endpoint serves `kimi-for-coding` (K2.8 Preview),
//!   `kimi-for-coding-highspeed`, `k3`, `k3-256k`, `k3.1-pro` and
//!   `k3.1-pro-highspeed` (its `/models` list, read live and cached — see
//!   `kimi.rs`), and the pay-per-token platform names its models by version
//!   (`kimi-k3.1-pro`, …), so the two defaults differ. The subscription has
//!   two hosts: `api.kimi.com` (mainland accounts) and `api.kimi.ai` (global
//!   accounts), see [`KIMI_CODING_HOSTS`].
//! * Z.AI serves `glm-5.3` on *all four* of its endpoints, so only the billing
//!   door changes there, never the model.
//!
//! Z.AI's Anthropic endpoint (`/api/anthropic`) is deliberately absent: it
//! speaks the Anthropic Messages protocol for Claude Code, and Rusta is an
//! OpenAI-protocol client. Z.AI's own docs route OpenAI-protocol tools to the
//! `coding/paas/v4` endpoint we use.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Which product the API key was issued for.
///
/// This is a property of the *key*, not a preference: Kimi refuses a key at
/// the other plan's endpoint, and Z.AI bills a key by its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[value(rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum Billing {
    /// A Coding Plan subscription key (Kimi "Coding Plan", GLM "Coding Plan").
    /// This is what a coding agent is normally billed against.
    #[default]
    Subscription,
    /// A pay-per-token key billed against API credits.
    #[value(alias = "credits", alias = "pay-per-token")]
    Api,
}

/// Which regional deployment to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[value(rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum Region {
    /// The international deployment (`api.moonshot.ai`, `api.z.ai`).
    #[default]
    Global,
    /// The mainland-China deployment (`api.moonshot.cn`, `open.bigmodel.cn`).
    #[value(alias = "cn", alias = "mainland")]
    China,
}

impl Billing {
    /// How a human refers to this billing.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Subscription => "Coding Plan subscription",
            Self::Api => "API credits (pay-per-token)",
        }
    }
}

impl Region {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::China => "Mainland China",
        }
    }
}

/// Kimi's two Coding Plan hosts: mainland (`api.kimi.com`, the default) and
/// global (`api.kimi.ai`): a `kimi.com` account belongs to the first and a
/// `kimi.ai` account to the second. A key from one account can be refused by
/// the other host with an error that reads like a bad key, so setup probes both
/// and a request refused by one is retried on the other.
pub const KIMI_CODING_HOSTS: [&str; 2] = [
    "https://api.kimi.com/coding/v1",
    "https://api.kimi.ai/coding/v1",
];

/// The other Kimi Coding host, when `base_url` is one of them.
#[must_use]
pub fn alternate_kimi_coding_host(base_url: &str) -> Option<&'static str> {
    let url = base_url.trim_end_matches('/');
    match KIMI_CODING_HOSTS.iter().position(|host| *host == url) {
        Some(0) => Some(KIMI_CODING_HOSTS[1]),
        Some(_) => Some(KIMI_CODING_HOSTS[0]),
        None => None,
    }
}

/// Where Z.AI's standalone tool endpoints (`/web_search`, `/reader`) live: the
/// pinned base URL when there is one, else the pay-per-token API root of the
/// platform that issued the key (api.z.ai, or open.bigmodel.cn for mainland
/// keys).
#[must_use]
pub fn zai_tools_base_url(pinned: Option<&str>, region: Region) -> String {
    pinned
        .or_else(|| base_url("glm", Billing::Api, region))
        .unwrap_or_default()
        .to_string()
}

/// The base URL `provider` serves `billing` on, in `region`.
///
/// `None` for a provider we cannot construct.
#[must_use]
pub fn base_url(provider: &str, billing: Billing, region: Region) -> Option<&'static str> {
    Some(match (provider, billing, region) {
        // Kimi's Coding Plan has two hosts, and the account that issued the
        // key picks one, not the region setting: `rusta init` pins the one
        // that accepts the key, and a run retries a refused key on the other
        // (see `KIMI_CODING_HOSTS`).
        ("kimi", Billing::Subscription, _) => "https://api.kimi.com/coding/v1",
        ("kimi", Billing::Api, Region::Global) => "https://api.moonshot.ai/v1",
        ("kimi", Billing::Api, Region::China) => "https://api.moonshot.cn/v1",

        ("glm" | "zai", Billing::Subscription, Region::Global) => {
            "https://api.z.ai/api/coding/paas/v4"
        }
        ("glm" | "zai", Billing::Subscription, Region::China) => {
            "https://open.bigmodel.cn/api/coding/paas/v4"
        }
        ("glm" | "zai", Billing::Api, Region::Global) => "https://api.z.ai/api/paas/v4",
        ("glm" | "zai", Billing::Api, Region::China) => "https://open.bigmodel.cn/api/paas/v4",

        // OpenAI has one endpoint; billing and region do not apply.
        ("openai", _, _) => "https://api.openai.com/v1",

        // DeepSeek has a single pay-as-you-go API; the `/v1` suffix is
        // optional on its side and absent from ours. There is no separate
        // subscription product, so billing and region do not apply — a
        // config's `billing` value is accepted and ignored for deepseek.
        ("deepseek", _, _) => "https://api.deepseek.com",

        _ => return None,
    })
}

/// The default model `provider` serves on `billing`: the one its vendor
/// recommends for coding.
///
/// Kimi names its models differently at each door, so the billing decides
/// the id. On the Coding Plan, `kimi-for-coding` (K2.8 Preview) runs with a
/// 1M window on every plan that includes coding, while K3.1 Pro needs Pro
/// for its 1M window and is not on the legacy Andante plan. On the platform,
/// every key reaches `kimi-k3.1-pro` once its account has topped up at least
/// $1, the K2 series is closed to new accounts, and the platform points
/// every migration at `kimi-k3.1-pro`.
#[must_use]
pub fn default_model(provider: &str, billing: Billing) -> Option<&'static str> {
    Some(match (provider, billing) {
        ("kimi", Billing::Subscription) => "kimi-for-coding",
        ("kimi", Billing::Api) => "kimi-k3.1-pro",
        ("glm" | "zai", _) => "glm-5.3",
        ("openai", _) => "gpt-4o-mini",
        ("deepseek", _) => "deepseek-flash",
        _ => return None,
    })
}

/// Whether the region changes anything for this provider+billing.
///
/// Kimi's Coding Plan and OpenAI have one endpoint each, so asking a user to
/// pick a region there is a question with no consequence.
#[must_use]
pub fn region_matters(provider: &str, billing: Billing) -> bool {
    base_url(provider, billing, Region::Global) != base_url(provider, billing, Region::China)
}

/// Whether the billing plan changes anything for this provider.
///
/// It does for Kimi and GLM, where the two plans are separate doors and a key
/// only opens one. It does not for a plain OpenAI-protocol endpoint, which has a
/// single door — so there is nothing to ask, and nothing to fall back to when a
/// key is rejected.
#[must_use]
pub fn billing_matters(provider: &str) -> bool {
    base_url(provider, Billing::Subscription, Region::Global)
        != base_url(provider, Billing::Api, Region::Global)
}

/// Read `var`, treating an exported-but-empty value as unset.
///
/// `.env.example` ships `KIMI_BASE_URL=` and friends with no value, and sourcing
/// it exports them as empty strings. `std::env::var` reports that as `Ok("")`,
/// which would otherwise win the precedence chain and override a perfectly good
/// default with nothing — an empty base URL, or an empty API key that also
/// convinces the first-run wizard there is nothing to set up. Nobody means "use
/// the empty string"; they mean "I did not set this".
#[must_use]
pub fn env_non_empty(var: &str) -> Option<String> {
    std::env::var(var).ok().as_deref().and_then(non_empty)
}

/// What [`env_non_empty`] makes of a variable's value: the value trimmed, or
/// nothing when only whitespace was there.
///
/// The *trimmed* value, not the raw one. A key exported with a trailing
/// newline (`export KIMI_API_KEY="$(cat key)"`) or a stray space otherwise
/// flows verbatim into the `Authorization: Bearer …` header, where the control
/// character makes reqwest reject the request — a valid key failing for a
/// reason no error message names. Endpoints and model ids get the same
/// treatment; none of them means to carry whitespace.
fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The environment variable holding this provider's API key.
#[must_use]
pub fn api_key_env_var(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "kimi" => "KIMI_API_KEY",
        "glm" | "zai" => "ZAI_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        _ => return None,
    })
}

/// Where the user creates a key for this provider and billing.
#[must_use]
pub fn signup_url(provider: &str, billing: Billing) -> Option<&'static str> {
    Some(match (provider, billing) {
        ("kimi", Billing::Subscription) => "https://www.kimi.com/code/console",
        ("kimi", Billing::Api) => "https://platform.kimi.ai/console/api-keys",
        // Coding Plan keys are made on the same page as pay-per-token ones.
        ("glm" | "zai", _) => "https://z.ai/manage-apikey/apikey-list",
        ("openai", _) => "https://platform.openai.com/api-keys",
        ("deepseek", _) => "https://platform.deepseek.com/api_keys",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_defaults::VALID_PROVIDERS;

    /// Every provider the runtime can build must have an endpoint, a default
    /// model, and a key variable for every billing/region combination. A gap here
    /// is a provider that resolves to no URL and fails at request time.
    #[test]
    fn every_valid_provider_is_fully_routed() {
        for p in VALID_PROVIDERS {
            assert!(api_key_env_var(p).is_some(), "{p}: no API key env var");
            for billing in [Billing::Subscription, Billing::Api] {
                assert!(
                    default_model(p, billing).is_some(),
                    "{p}/{billing:?}: no model"
                );
                assert!(
                    signup_url(p, billing).is_some(),
                    "{p}/{billing:?}: no signup url"
                );
                for region in [Region::Global, Region::China] {
                    let url = base_url(p, billing, region);
                    assert!(url.is_some(), "{p}/{billing:?}/{region:?}: no base URL");
                    assert!(
                        url.unwrap().starts_with("https://"),
                        "{p}: endpoint must be https"
                    );
                }
            }
        }
    }

    /// The defaults are the models each vendor recommends, and they are the
    /// ones each endpoint actually serves. Verified against the live
    /// `/models` catalogues.
    #[test]
    fn defaults_are_the_recommended_models() {
        // Kimi names its models differently at each door: K2.8 Preview on
        // the Coding Plan, K3.1 Pro by version on the platform.
        assert_eq!(
            default_model("kimi", Billing::Subscription),
            Some("kimi-for-coding")
        );
        assert_eq!(default_model("kimi", Billing::Api), Some("kimi-k3.1-pro"));

        // Z.AI serves glm-5.3 on every endpoint, so the billing cannot change it.
        for billing in [Billing::Subscription, Billing::Api] {
            assert_eq!(default_model("glm", billing), Some("glm-5.3"));
            assert_eq!(default_model("zai", billing), Some("glm-5.3"));
        }

        // DeepSeek is pay-as-you-go only; V4.1-Flash is the default on either
        // billing value a config may carry. DeepSeek ranks it ahead of V4-Pro
        // and defaults to it in its own integrations.
        for billing in [Billing::Subscription, Billing::Api] {
            assert_eq!(default_model("deepseek", billing), Some("deepseek-flash"));
        }
    }

    /// The subscription and pay-per-token endpoints are different doors, and a
    /// key only opens one. Collapsing them would lock every user out of one billing.
    #[test]
    fn subscription_and_api_endpoints_are_distinct() {
        for p in ["kimi", "glm", "zai"] {
            for region in [Region::Global, Region::China] {
                assert_ne!(
                    base_url(p, Billing::Subscription, region),
                    base_url(p, Billing::Api, region),
                    "{p}/{region:?}: subscription and pay-per-token must not share an endpoint"
                );
            }
        }
    }

    /// The verified endpoint table. Each of these answered a live request.
    #[test]
    fn endpoints_match_the_verified_table() {
        use Billing::{Api, Subscription};
        use Region::{China, Global};

        assert_eq!(
            base_url("kimi", Subscription, Global),
            Some("https://api.kimi.com/coding/v1")
        );
        // Kimi's Coding Plan has no separate mainland endpoint.
        assert_eq!(
            base_url("kimi", Subscription, China),
            base_url("kimi", Subscription, Global)
        );
        assert_eq!(
            base_url("kimi", Api, Global),
            Some("https://api.moonshot.ai/v1")
        );
        assert_eq!(
            base_url("kimi", Api, China),
            Some("https://api.moonshot.cn/v1")
        );

        assert_eq!(
            base_url("glm", Subscription, Global),
            Some("https://api.z.ai/api/coding/paas/v4")
        );
        assert_eq!(
            base_url("glm", Subscription, China),
            Some("https://open.bigmodel.cn/api/coding/paas/v4")
        );
        assert_eq!(
            base_url("glm", Api, Global),
            Some("https://api.z.ai/api/paas/v4")
        );
        assert_eq!(
            base_url("glm", Api, China),
            Some("https://open.bigmodel.cn/api/paas/v4")
        );

        // DeepSeek serves every billing and region from one endpoint.
        for billing in [Subscription, Api] {
            for region in [Global, China] {
                assert_eq!(
                    base_url("deepseek", billing, region),
                    Some("https://api.deepseek.com")
                );
            }
        }
    }

    /// Asking a user to choose a region only makes sense where it changes the
    /// endpoint.
    /// Z.AI's tool endpoints follow the key's platform unless a URL is pinned.
    #[test]
    fn zai_tools_follow_the_region() {
        assert_eq!(
            zai_tools_base_url(None, Region::Global),
            "https://api.z.ai/api/paas/v4"
        );
        assert_eq!(
            zai_tools_base_url(None, Region::China),
            "https://open.bigmodel.cn/api/paas/v4"
        );
        assert_eq!(
            zai_tools_base_url(Some("http://proxy/v4"), Region::China),
            "http://proxy/v4"
        );
    }

    #[test]
    fn the_kimi_coding_hosts_alternate() {
        assert_eq!(
            alternate_kimi_coding_host("https://api.kimi.com/coding/v1/"),
            Some("https://api.kimi.ai/coding/v1")
        );
        assert_eq!(
            alternate_kimi_coding_host("https://api.kimi.ai/coding/v1"),
            Some("https://api.kimi.com/coding/v1")
        );
        assert_eq!(
            alternate_kimi_coding_host("https://api.moonshot.ai/v1"),
            None
        );
        assert_eq!(
            base_url("kimi", Billing::Subscription, Region::Global),
            Some(KIMI_CODING_HOSTS[0]),
            "the default host is unchanged for existing users"
        );
    }

    #[test]
    fn region_only_matters_where_it_changes_the_endpoint() {
        assert!(!region_matters("kimi", Billing::Subscription));
        assert!(region_matters("kimi", Billing::Api));
        assert!(region_matters("glm", Billing::Subscription));
        assert!(region_matters("glm", Billing::Api));
        assert!(!region_matters("openai", Billing::Subscription));
    }

    /// `.env.example` ships these keys with no value. Sourcing it exports them
    /// as empty strings, and treating that as "set" would override the flagship
    /// endpoint with `""` — and hide the setup wizard behind an empty API key.
    /// An unset variable and an empty one must mean the same thing.
    #[test]
    fn an_empty_env_var_counts_as_unset() {
        assert_eq!(non_empty(""), None, "empty must read as unset");
        assert_eq!(non_empty("   "), None, "blank must read as unset");
        assert_eq!(non_empty("value").as_deref(), Some("value"));

        // Surrounding whitespace is stripped: a key pasted or piped with a
        // trailing newline must not reach the Authorization header verbatim.
        assert_eq!(non_empty("  sk-abc123\n").as_deref(), Some("sk-abc123"));

        assert_eq!(env_non_empty("RUSTA_TEST_NEVER_SET_ENV_VAR"), None);
    }

    #[test]
    fn zai_is_an_alias_for_glm_everywhere() {
        for billing in [Billing::Subscription, Billing::Api] {
            for region in [Region::Global, Region::China] {
                assert_eq!(
                    base_url("zai", billing, region),
                    base_url("glm", billing, region)
                );
            }
            assert_eq!(default_model("zai", billing), default_model("glm", billing));
        }
        assert_eq!(api_key_env_var("zai"), api_key_env_var("glm"));
    }

    #[test]
    fn unknown_provider_routes_nowhere() {
        assert_eq!(
            base_url("mock", Billing::Subscription, Region::Global),
            None
        );
        assert_eq!(default_model("mock", Billing::Subscription), None);
        assert_eq!(api_key_env_var("mock"), None);
    }

    /// The defaults must be the flagships the user asked for, at full context.
    #[test]
    fn default_plan_and_region_select_the_flagships() {
        let (billing, region) = (Billing::default(), Region::default());
        assert_eq!(billing, Billing::Subscription);
        assert_eq!(region, Region::Global);

        assert_eq!(default_model("kimi", billing), Some("kimi-for-coding"));
        assert_eq!(
            base_url("kimi", billing, region),
            Some("https://api.kimi.com/coding/v1")
        );

        assert_eq!(default_model("glm", billing), Some("glm-5.3"));
        assert_eq!(
            base_url("glm", billing, region),
            Some("https://api.z.ai/api/coding/paas/v4")
        );
    }
}
