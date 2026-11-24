//! Building a provider: which endpoint, key, headers and timeouts it gets from
//! the command line, the config and the environment, and its model catalog.

use std::time::Duration;

use lm_bridge::providers::catalog_cache::CatalogEntry;
use lm_bridge::providers::deepseek_catalog::DeepseekModelInfo;
use lm_bridge::providers::kimi_catalog::KimiModelInfo;
use lm_bridge::providers::openai_compat::HttpTimeouts;
use lm_bridge::providers::{
    DeepseekChatProvider, GlmChatProvider, KimiChatProvider, OpenAiCompatProvider,
};

use crate::cli::Cli;
use crate::{catalogs, endpoints, provider_defaults};

/// Where Kimi Files API uploads are remembered across sessions, pruned of
/// entries older than [`agent_core::tool_output::SPILL_MAX_AGE`] on the way.
fn kimi_media_cache_dir() -> Option<std::path::PathBuf> {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = agent_core::user_dirs::data_dir()?
            .join("rusta")
            .join("media-cache");
        agent_core::tool_output::prune_spill_root(&dir, agent_core::tool_output::SPILL_MAX_AGE);
        Some(dir)
    })
    .clone()
}

/// Default for `--idle-timeout`. A connection silent for five minutes is
/// stuck, not thinking.
pub(crate) const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

pub(crate) fn build_provider(
    cli: &Cli,
    timeouts: HttpTimeouts,
) -> anyhow::Result<Box<dyn lm_bridge::provider::ChatProvider>> {
    build_provider_for(cli, cli.provider.clone(), cli.model.clone(), timeouts)
}

/// The two request bounds for this invocation.
///
/// They are separate settings because one number cannot do both jobs: a total
/// cap that is generous enough for a long answer is useless for detecting a
/// stall, and one tight enough to catch a stall kills healthy work. `--timeout`
/// is therefore uncapped unless asked for, while `--idle-timeout` always has a
/// default — a silent connection is never something to wait on forever.
pub(crate) fn resolve_timeouts(cli: &Cli) -> HttpTimeouts {
    HttpTimeouts {
        total: cli.timeout.filter(|s| *s > 0).map(Duration::from_secs),
        // Zero turns a bound off, rather than setting a zero-length deadline
        // that would fail every request instantly.
        idle: match cli.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS) {
            0 => None,
            secs => Some(Duration::from_secs(secs)),
        },
    }
}

pub(crate) fn build_provider_for(
    cli: &Cli,
    provider: Option<String>,
    model: Option<String>,
    timeouts: HttpTimeouts,
) -> anyhow::Result<Box<dyn lm_bridge::provider::ChatProvider>> {
    let provider =
        provider_defaults::resolve_provider(provider.as_deref().or(cli.provider.as_deref()));

    // `cli.model` names a model of `cli.provider` — like the key and the endpoint,
    // it is scoped to the provider it was written for. `/provider glm` in a Kimi
    // session must not carry `kimi-for-coding` across; Z.AI does not serve it.
    let explicit_model = model.or_else(|| {
        cli.model
            .clone()
            .filter(|_| is_configured_provider(cli, &provider, endpoints::env_non_empty))
    });
    let billing = cli.billing.unwrap_or_default();
    let region = cli.region.unwrap_or_default();

    // Resolve `--model` → `*_MODEL_NAME` → the flagship for this plan, in one
    // place. Kimi's two endpoints name the same generation differently, so the
    // plan decides the id. An unknown provider has no default; reject it before
    // touching the arms.
    let Some(model) =
        provider_defaults::resolve_model(&provider, explicit_model.as_deref(), billing)
            .model()
            .map(str::to_string)
    else {
        anyhow::bail!("Unknown provider: {provider}");
    };

    // The endpoint follows from the billing plan unless the user pinned one.
    // Kimi refuses a Coding Plan key at the pay-per-token endpoint and vice
    // versa, and Z.AI bills an older Coding Plan key per token there, so this
    // pairing is what makes a fresh key work first try.
    let base_url = resolve_base_url(cli, &provider, billing, region);
    let api_key = resolve_api_key(cli, &provider);

    // Say "you have no key" here, rather than letting the vendor say it.
    //
    // Sending a keyless request to Moonshot or Z.AI earns a 401 reading
    // `invalid_authentication_error` / "Invalid Authentication" — which sounds
    // like a *wrong* key and sends people off checking their clipboard, when in
    // fact they never set one. That single misreading is what this whole setup
    // path exists to prevent, so Rusta must not manufacture it itself.
    //
    // Only for the vendor endpoints we ship: someone who has pointed `--base-url`
    // at a local server may legitimately have no key at all.
    if api_key.is_none()
        && let Some(var) = configured_key_env(cli, &provider)
    {
        anyhow::bail!(
            "The config declares api_key_env = \"{var}\", but that environment variable is not \
             set or is empty."
        );
    }
    let headers = custom_headers(cli, &provider)?;
    if api_key.is_none() && requires_api_key(&provider, &base_url, billing, region) {
        let var = endpoints::api_key_env_var(&provider).unwrap_or("RUSTA_API_KEY");
        anyhow::bail!("no API key for {provider}. Run `rusta init`, or set {var}.");
    }

    match provider.as_str() {
        "kimi" => {
            // An unpinned Kimi Code host may refuse a key from the other
            // region's account; the other host is then tried.
            let pinned = endpoints::base_url(&provider, billing, region) != Some(base_url.as_str());
            let alternate = (!pinned)
                .then(|| endpoints::alternate_kimi_coding_host(&base_url))
                .flatten();
            let cached = catalogs::cached_models::<KimiModelInfo>(&base_url);
            let mut provider = KimiChatProvider::new(model, api_key, base_url, timeouts)?;
            if let Some(models) = cached {
                provider = provider.with_catalog(models);
            }
            if let Some(dir) = kimi_media_cache_dir() {
                provider = provider.with_media_cache_dir(dir);
            }
            if let Some(alternate) = alternate {
                provider = provider.with_alternate_host(alternate, timeouts)?;
            }
            Ok(Box::new(provider.with_custom_headers(&headers)))
        }
        "glm" | "zai" => {
            // A Z.AI URL shows its plan; a proxy's does not, and the billing
            // setting says which one sits behind it.
            let coding_plan = GlmChatProvider::endpoint_is_coding_plan(&base_url)
                .unwrap_or(billing == endpoints::Billing::Subscription);
            Ok(Box::new(
                GlmChatProvider::new(model, api_key, base_url, timeouts)?
                    .with_coding_plan(coding_plan)
                    .with_custom_headers(&headers),
            ))
        }
        // Only the generic OpenAI-compatible provider takes a reasoning-key pin.
        // Kimi and GLM both speak `reasoning_content` and are detected onto it
        // anyway, so exposing the knob there would only offer a way to break them.
        "openai" => Ok(Box::new(
            OpenAiCompatProvider::new("openai", model, base_url, api_key, timeouts)?
                .with_reasoning_key(resolve_reasoning_key(cli))
                .with_inline_video(cli.inline_video)
                .with_custom_headers(&headers),
        )),
        // DeepSeek speaks `reasoning_content` and is detected onto it like
        // Kimi and GLM, so it takes no reasoning-key pin either. Its
        // differences — thinking/effort fields, `max_tokens`, the 402 — live
        // in the provider, and its models' limits in its catalogue.
        "deepseek" => {
            let cached = catalogs::cached_models::<DeepseekModelInfo>(&base_url);
            let mut provider = DeepseekChatProvider::new(model, api_key, base_url, timeouts)?;
            if let Some(models) = cached {
                provider = provider.with_catalog(models);
            }
            Ok(Box::new(provider.with_custom_headers(&headers)))
        }
        other => anyhow::bail!("Unknown provider: {other}"),
    }
}

/// Refresh `provider`'s model catalogue in the background, for the next
/// provider built ([`catalogs::refresh_in_background`]).
pub(crate) fn refresh_catalog<T: CatalogEntry>(cli: &Cli, provider: &str) {
    // The provider was built already, so the key and headers are valid.
    if let Ok((base_url, key, headers)) = catalog_source(cli, provider) {
        catalogs::refresh_in_background::<T>(base_url, key, headers);
    }
}

/// Where `provider`'s model catalogue is fetched: the base URL its requests
/// go to, its key and its custom headers. `Err` says what is missing.
pub(crate) fn catalog_source(
    cli: &Cli,
    provider: &str,
) -> Result<(String, String, reqwest::header::HeaderMap), String> {
    let base_url = resolve_base_url(
        cli,
        provider,
        cli.billing.unwrap_or_default(),
        cli.region.unwrap_or_default(),
    );
    let key = resolve_api_key(cli, provider).ok_or_else(|| format!("no API key for {provider}"))?;
    let headers = custom_headers(cli, provider).map_err(|e| e.to_string())?;
    Ok((base_url, key, headers))
}

/// The reasoning dialect the user pinned, if any.
///
/// An unrecognised value is a no-op rather than an error: it leaves detection
/// on, which is the behaviour that works everywhere, and a hard failure here
/// would refuse to start a session over a cosmetic setting.
fn resolve_reasoning_key(cli: &Cli) -> Option<lm_bridge::providers::reasoning_key::ReasoningKey> {
    let raw = cli.reasoning_key.as_deref()?;
    match lm_bridge::providers::reasoning_key::ReasoningKey::parse(raw) {
        Some(key) => Some(key),
        None => {
            eprintln!(
                "unknown reasoning key {raw:?}; expected reasoning_content, reasoning, or \
                 reasoning_details. Detecting it from the endpoint instead."
            );
            None
        }
    }
}

/// Whether a missing API key is a hard error for the endpoint we settled on.
///
/// True for the vendor endpoints in our table — Moonshot, Z.AI and OpenAI all
/// demand a key, and going without earns a 401 that reads like a *wrong* key.
/// False once the user has pointed somewhere else: a local vLLM or llama.cpp
/// commonly wants no key at all, and refusing to start would be wrong.
pub(crate) fn requires_api_key(
    provider: &str,
    base_url: &str,
    billing: endpoints::Billing,
    region: endpoints::Region,
) -> bool {
    endpoints::base_url(provider, billing, region) == Some(base_url)
}

/// Whether `provider` is the one `cli`'s own `--api-key` / `--base-url` describe.
///
/// Those two are *not* provider-agnostic. They are either flags given in the same
/// breath as `--provider`, or the config's values sitting beside the config's
/// `provider` — so in both cases they belong to `cli.provider`. The REPL's
/// `/provider glm` rebuilds against a different one, and reusing them there would
/// send a Kimi key to Z.AI and earn an `invalid_authentication_error` that reads
/// like a bad key. Scope them to the provider they were written for.
///
/// `env` reads `RUSTA_PROVIDER` when `cli` names no provider.
fn is_configured_provider<F>(cli: &Cli, provider: &str, env: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    provider_defaults::resolve_provider_with(cli.provider.as_deref(), env) == provider
}

/// `--base-url` > `<PROVIDER>_BASE_URL` > the endpoint implied by billing+region.
pub(crate) fn resolve_base_url(
    cli: &Cli,
    provider: &str,
    billing: endpoints::Billing,
    region: endpoints::Region,
) -> String {
    resolve_base_url_with(cli, provider, billing, region, endpoints::env_non_empty)
}

/// [`resolve_base_url`], reading variables through `env` (on
/// [`endpoints::env_non_empty`]'s terms), injected so tests need not mutate
/// the process.
fn resolve_base_url_with<F>(
    cli: &Cli,
    provider: &str,
    billing: endpoints::Billing,
    region: endpoints::Region,
    env: F,
) -> String
where
    F: Fn(&str) -> Option<String>,
{
    cli.base_url
        .clone()
        .filter(|_| is_configured_provider(cli, provider, &env))
        .or_else(|| base_url_env_var(provider).and_then(&env))
        .or_else(|| endpoints::base_url(provider, billing, region).map(str::to_string))
        .unwrap_or_default()
}

/// The config's `api_key_env`, when it applies to `provider`.
pub(crate) fn configured_key_env<'a>(cli: &'a Cli, provider: &str) -> Option<&'a str> {
    configured_key_env_with(cli, provider, endpoints::env_non_empty)
}

/// [`configured_key_env`], reading `RUSTA_PROVIDER` through `env`.
pub(crate) fn configured_key_env_with<'a, F>(
    cli: &'a Cli,
    provider: &str,
    env: F,
) -> Option<&'a str>
where
    F: Fn(&str) -> Option<String>,
{
    cli.api_key_env
        .as_deref()
        .filter(|_| is_configured_provider(cli, provider, env))
}

/// Custom headers for `provider`'s requests: `RUSTA_CUSTOM_HEADERS`
/// (`Name: value` lines), then the config's `custom_headers` for the
/// configured provider, which win. An invalid header is an error naming it.
pub(crate) fn custom_headers(
    cli: &Cli,
    provider: &str,
) -> anyhow::Result<reqwest::header::HeaderMap> {
    custom_headers_with(cli, provider, endpoints::env_non_empty)
}

/// [`custom_headers`], reading variables through `env` (on
/// [`endpoints::env_non_empty`]'s terms), injected so tests need not mutate
/// the process.
pub(crate) fn custom_headers_with<F>(
    cli: &Cli,
    provider: &str,
    env: F,
) -> anyhow::Result<reqwest::header::HeaderMap>
where
    F: Fn(&str) -> Option<String>,
{
    let mut pairs = env("RUSTA_CUSTOM_HEADERS")
        .map(|raw| lm_bridge::custom_headers::parse_header_lines(&raw))
        .unwrap_or_default();
    if is_configured_provider(cli, provider, &env) {
        pairs.extend(
            cli.custom_headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
    }
    Ok(lm_bridge::custom_headers::header_map(pairs)?)
}

/// `--api-key` > the config's `api_key` or `api_key_env` > `<PROVIDER>_API_KEY`.
///
/// A configured `api_key_env` replaces the provider's variable rather than
/// falling back to it: a key from a variable the config did not name would
/// be a surprise.
pub(crate) fn resolve_api_key(cli: &Cli, provider: &str) -> Option<String> {
    resolve_api_key_with(cli, provider, endpoints::env_non_empty)
}

/// [`resolve_api_key`], reading variables through `env` (on
/// [`endpoints::env_non_empty`]'s terms), injected so tests need not mutate
/// the process.
pub(crate) fn resolve_api_key_with<F>(cli: &Cli, provider: &str, env: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    // Trim whatever source wins. `env_non_empty` already trims the provider env
    // var, but `--api-key`, `RUSTA_API_KEY` (clap `env`), and a hand-edited
    // config value arrive raw — and a trailing newline or space reaches the
    // `Authorization: Bearer …` header, where the control character makes reqwest
    // reject the request: a valid key failing for a reason no message names. A
    // key that is only whitespace falls through to the env var rather than
    // resolving to nothing usable.
    let explicit = cli
        .api_key
        .as_deref()
        .filter(|_| is_configured_provider(cli, provider, &env))
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);
    explicit.or_else(|| match configured_key_env_with(cli, provider, &env) {
        Some(var) => env(var),
        None => endpoints::api_key_env_var(provider).and_then(&env),
    })
}

/// The provider's own base-URL variable (`KIMI_BASE_URL`, …), read when
/// `--base-url` does not apply to the provider.
fn base_url_env_var(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "kimi" => "KIMI_BASE_URL",
        "glm" | "zai" => "ZAI_BASE_URL",
        "openai" => "OPENAI_BASE_URL",
        "deepseek" => "DEEPSEEK_BASE_URL",
        _ => return None,
    })
}

#[cfg(test)]
mod provider_resolution_tests {
    use super::{resolve_api_key_with, resolve_base_url_with};
    use crate::endpoints::{Billing, Region};
    use clap::Parser;

    /// DeepSeek resolves its key from `DEEPSEEK_API_KEY` and its endpoint from
    /// the table or `DEEPSEEK_BASE_URL` — the same chain the other vendors
    /// use, including the rule that a key pinned for one provider never
    /// follows a switch.
    #[test]
    fn deepseek_resolves_its_own_key_and_endpoint() {
        const KEY: (&str, &str) = ("DEEPSEEK_API_KEY", "sk-deepseek-env");
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |var: &str| {
                vars.iter()
                    .find(|(name, _)| *name == var)
                    .map(|(_, value)| value.to_string())
            }
        };
        let both = env(&[KEY, ("DEEPSEEK_BASE_URL", "https://proxy.example/v1")]);
        let key_only = env(&[KEY]);
        let base_url = |cli: &super::Cli, env| {
            resolve_base_url_with(cli, "deepseek", Billing::default(), Region::default(), env)
        };

        // DeepSeek is the configured provider: its own variables apply.
        let mut cli = super::Cli::parse_from(["rusta", "--provider", "deepseek"]);
        assert_eq!(
            resolve_api_key_with(&cli, "deepseek", both).as_deref(),
            Some("sk-deepseek-env"),
            "the provider's own variable supplies the key"
        );
        assert_eq!(
            base_url(&cli, both),
            "https://proxy.example/v1",
            "the provider's own variable pins the endpoint"
        );
        assert_eq!(
            base_url(&cli, key_only),
            "https://api.deepseek.com",
            "the table supplies the endpoint when nothing is pinned"
        );

        // A Kimi-configured run asked to build DeepSeek (`/provider deepseek`
        // in a Kimi session): the Kimi settings must not follow, and the
        // DeepSeek key comes from its own variable.
        cli.provider = Some("kimi".into());
        cli.api_key = Some("sk-kimi-secret".into());
        cli.base_url = Some("https://api.kimi.com/coding/v1".into());
        assert_eq!(
            resolve_api_key_with(&cli, "deepseek", key_only).as_deref(),
            Some("sk-deepseek-env"),
            "a Kimi key pinned beside `--provider kimi` must not reach DeepSeek"
        );
        assert_eq!(base_url(&cli, key_only), "https://api.deepseek.com");

        assert_eq!(resolve_api_key_with(&cli, "deepseek", |_| None), None);
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::{DEFAULT_IDLE_TIMEOUT_SECS, resolve_timeouts};
    use crate::cli::Cli;
    use clap::Parser;
    use std::time::Duration;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("rusta").chain(args.iter().copied()))
    }

    /// By default a request is bounded by silence, not by length. A total cap
    /// generous enough for a long answer cannot also detect a stall, so the two
    /// are separate settings and only the stall bound is on by default.
    #[test]
    fn by_default_only_the_idle_bound_applies() {
        let t = resolve_timeouts(&cli(&[]));
        assert_eq!(t.total, None, "a long answer must not be cut off");
        assert_eq!(t.idle, Some(Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)));
    }

    /// `--timeout` keeps the meaning it has always had: a cap on the whole
    /// request. Setting it must not disturb the stall bound.
    #[test]
    fn a_total_cap_is_applied_when_asked_for() {
        let t = resolve_timeouts(&cli(&["--timeout", "60"]));
        assert_eq!(t.total, Some(Duration::from_secs(60)));
        assert_eq!(
            t.idle,
            Some(Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)),
            "the stall bound is independent of the total cap"
        );
    }

    #[test]
    fn the_idle_bound_is_configurable_on_its_own() {
        let t = resolve_timeouts(&cli(&["--idle-timeout", "30"]));
        assert_eq!(t.total, None);
        assert_eq!(t.idle, Some(Duration::from_secs(30)));
    }

    #[test]
    fn both_bounds_can_apply_at_once() {
        let t = resolve_timeouts(&cli(&["--timeout", "600", "--idle-timeout", "45"]));
        assert_eq!(t.total, Some(Duration::from_secs(600)));
        assert_eq!(t.idle, Some(Duration::from_secs(45)));
    }

    /// Zero is how each bound is turned off, rather than a zero-length deadline
    /// that would fail every request instantly.
    #[test]
    fn zero_disables_a_bound_rather_than_expiring_immediately() {
        let no_idle = resolve_timeouts(&cli(&["--idle-timeout", "0"]));
        assert_eq!(no_idle.idle, None);
        let no_total = resolve_timeouts(&cli(&["--timeout", "0"]));
        assert_eq!(no_total.total, None);
    }
}
