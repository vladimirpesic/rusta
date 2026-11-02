//! Single source of truth for provider names, model resolution, and defaults.
//!
//! `main` builds the provider and `doctor` reports on it. When each kept its own
//! copy of these rules they drifted: `doctor` blessed a `mock` provider that the
//! runtime rejected, and warned "no model configured" while `KIMI_MODEL_NAME` was
//! in fact supplying the model. Both now read from here.

use crate::endpoints::Billing;

/// Providers the runtime can actually construct.
///
/// Must stay in sync with the dispatch in [`crate::build_provider_for`]; the
/// `providers_match_dispatch` test below enforces that.
pub const VALID_PROVIDERS: &[&str] = &["kimi", "glm", "zai", "openai", "deepseek"];

/// Provider used when none is given on the CLI, in config, or in the environment.
pub const DEFAULT_PROVIDER: &str = "kimi";

/// A context window of `n` tokens as vendors write it: `1M`, `256K`, `128K`.
///
/// Vendors mix binary windows (262,144 is "256K") with decimal ones (128,000
/// is "128K"), so a round decimal count reads as decimal and only then is the
/// binary unit tried.
#[must_use]
pub fn format_window(n: u64) -> String {
    const MI: u64 = 1_048_576;
    match n {
        0 => "0".to_string(),
        n if n % MI == 0 => format!("{}M", n / MI),
        n if n % 1_000_000 == 0 => format!("{}M", n / 1_000_000),
        n if n % 1_000 == 0 => format!("{}K", n / 1_000),
        n if n % 1_024 == 0 => format!("{}K", n / 1_024),
        n => format!("{}K", (n + 500) / 1_000),
    }
}

/// The environment variable consulted for `provider`'s model, if any.
#[must_use]
pub fn model_env_var(provider: &str) -> Option<&'static str> {
    match provider {
        "kimi" => Some("KIMI_MODEL_NAME"),
        "glm" | "zai" => Some("ZAI_MODEL_NAME"),
        "openai" => Some("OPENAI_MODEL_NAME"),
        "deepseek" => Some("DEEPSEEK_MODEL_NAME"),
        _ => None,
    }
}

/// The model used when nothing else specifies one.
///
/// The plan decides it: Kimi's subscription endpoint serves `kimi-for-coding`
/// and the platform `kimi-k3.1-pro`, and neither knows the other's ids, so an
/// id is not portable between the two doors. See [`crate::endpoints`].
#[must_use]
pub fn default_model(provider: &str, billing: Billing) -> Option<&'static str> {
    crate::endpoints::default_model(provider, billing)
}

/// Resolve the provider exactly as the runtime does.
#[must_use]
pub fn resolve_provider(explicit: Option<&str>) -> String {
    resolve_provider_with(explicit, crate::endpoints::env_non_empty)
}

/// [`resolve_provider`], reading `RUSTA_PROVIDER` through `env`, injected
/// so tests need not mutate the process.
pub fn resolve_provider_with<F>(explicit: Option<&str>, env: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    explicit
        .map(str::to_string)
        .or_else(|| env("RUSTA_PROVIDER"))
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string())
}

/// Where a resolved model came from. `doctor` reports this; `main` only needs
/// the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSource {
    /// `--model`, or `model` in the config file.
    Explicit(String),
    /// The provider's `*_MODEL_NAME` environment variable.
    Environment { var: &'static str, model: String },
    /// The compiled-in default for this provider.
    Default(String),
    /// The provider is not one we can build.
    UnknownProvider,
}

impl ModelSource {
    /// The model name, if one could be resolved.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        match self {
            Self::Explicit(m) | Self::Default(m) => Some(m),
            Self::Environment { model, .. } => Some(model),
            Self::UnknownProvider => None,
        }
    }
}

/// Resolve the model for `provider`, mirroring [`crate::build_provider_for`].
///
/// `explicit` is the already-merged `--model` / config value. `env` reads an
/// environment variable, injected so tests need not mutate the process.
pub fn resolve_model_with<F>(
    provider: &str,
    explicit: Option<&str>,
    billing: Billing,
    env: F,
) -> ModelSource
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(m) = explicit {
        return ModelSource::Explicit(m.to_string());
    }
    if let Some(var) = model_env_var(provider)
        && let Some(model) = env(var)
    {
        return ModelSource::Environment { var, model };
    }
    match default_model(provider, billing) {
        Some(m) => ModelSource::Default(m.to_string()),
        None => ModelSource::UnknownProvider,
    }
}

/// [`resolve_model_with`] reading the real process environment.
#[must_use]
pub fn resolve_model(provider: &str, explicit: Option<&str>, billing: Billing) -> ModelSource {
    resolve_model_with(provider, explicit, billing, crate::endpoints::env_non_empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn windows_read_naturally() {
        assert_eq!(format_window(1_048_576), "1M");
        assert_eq!(format_window(262_144), "256K");
        assert_eq!(format_window(128_000), "128K");
        assert_eq!(format_window(200_000), "200K");
        assert_eq!(format_window(202_752), "198K");
        assert_eq!(format_window(2_000_000), "2M");
        assert_eq!(format_window(8_191), "8K");
    }

    #[test]
    fn explicit_model_wins_over_environment_and_default() {
        let src = resolve_model_with("kimi", Some("custom"), Billing::default(), |_| {
            Some("from-env".into())
        });
        assert_eq!(src, ModelSource::Explicit("custom".into()));
        assert_eq!(src.model(), Some("custom"));
    }

    #[test]
    fn environment_wins_over_default() {
        let src = resolve_model_with("kimi", None, Billing::default(), |v| {
            (v == "KIMI_MODEL_NAME").then(|| "kimi-for-coding".to_string())
        });
        assert_eq!(
            src,
            ModelSource::Environment {
                var: "KIMI_MODEL_NAME",
                model: "kimi-for-coding".into()
            }
        );
    }

    /// The defaults are the models the user runs: GLM 5.3 (1M context), Kimi
    /// K2.8 Preview on the Coding Plan and K3.1 Pro per token, and DeepSeek
    /// V4.1-Flash.
    #[test]
    fn falls_back_to_the_recommended_provider_default() {
        assert_eq!(
            resolve_model_with("glm", None, Billing::default(), no_env),
            ModelSource::Default("glm-5.3".into())
        );
        assert_eq!(
            resolve_model_with("kimi", None, Billing::Subscription, no_env),
            ModelSource::Default("kimi-for-coding".into())
        );
        assert_eq!(
            resolve_model_with("kimi", None, Billing::Api, no_env),
            ModelSource::Default("kimi-k3.1-pro".into())
        );
        assert_eq!(
            resolve_model_with("openai", None, Billing::default(), no_env),
            ModelSource::Default("gpt-4o-mini".into())
        );
        // DeepSeek is pay-as-you-go only; either billing value resolves to
        // the model DeepSeek recommends.
        assert_eq!(
            resolve_model_with("deepseek", None, Billing::default(), no_env),
            ModelSource::Default("deepseek-flash".into())
        );
        assert_eq!(
            resolve_model_with("deepseek", None, Billing::Api, no_env),
            ModelSource::Default("deepseek-flash".into())
        );
    }

    /// The deprecated model must not come back as a default.
    #[test]
    fn no_default_is_a_deprecated_model() {
        for billing in [Billing::Subscription, Billing::Api] {
            for p in VALID_PROVIDERS {
                assert_ne!(default_model(p, billing), Some("kimi-k2-0711-preview"));
                assert_ne!(default_model(p, billing), Some("glm-5.1"));
            }
        }
    }

    #[test]
    fn zai_is_an_alias_for_glm() {
        assert_eq!(model_env_var("zai"), model_env_var("glm"));
        assert_eq!(
            default_model("zai", Billing::default()),
            default_model("glm", Billing::default())
        );
    }

    #[test]
    fn unknown_provider_resolves_to_nothing() {
        let src = resolve_model_with("mock", None, Billing::default(), no_env);
        assert_eq!(src, ModelSource::UnknownProvider);
        assert_eq!(src.model(), None);
    }

    /// Every provider the runtime accepts must have a default model, and every
    /// provider with a default must be accepted. This is what keeps `doctor`
    /// from blessing a provider `build_provider_for` would reject.
    #[test]
    fn providers_match_dispatch() {
        for p in VALID_PROVIDERS {
            assert!(
                default_model(p, Billing::default()).is_some(),
                "`{p}` is accepted but has no default model"
            );
            assert!(
                model_env_var(p).is_some(),
                "`{p}` is accepted but has no model env var"
            );
        }
        for p in ["mock", "anthropic", ""] {
            assert!(
                !VALID_PROVIDERS.contains(&p),
                "`{p}` must not be accepted without dispatch support"
            );
            assert!(default_model(p, Billing::default()).is_none());
        }
    }

    #[test]
    fn default_provider_is_valid() {
        assert!(VALID_PROVIDERS.contains(&DEFAULT_PROVIDER));
    }
}
