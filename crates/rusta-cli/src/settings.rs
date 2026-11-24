//! The settings a run starts with: the config files, read through the
//! workspace-trust gate, merged into the command line.

use std::io;

use crate::cli::Cli;
use crate::{config, endpoints, provider_defaults, trust};

/// Load the config layers, the project's through `gate`, which decides what
/// of it this folder is trusted for.
pub(crate) fn load_config(cli: &Cli, gate: &mut trust::Gate) -> config::Config {
    // Sources are layered lowest-precedence first, so an explicit `--config`
    // is appended last: naming a file on the command line is the strongest
    // statement about which settings this invocation should use.
    let mut sources = config::Config::default_sources();
    if let Some(path) = &cli.config {
        sources.push((config::Source::Explicit, path.clone()));
    }
    // A config that cannot be read is an error to fix, not one to run
    // without: falling back to defaults would drop the provider, key and
    // permission rules without a word.
    let loaded = config::Config::load_sources(&sources, |lower, path, layer| {
        Ok(gate.settle(trust::ProjectLayer::split(path, layer, lower)?))
    });
    match loaded {
        Ok(config) => {
            withhold_credentials(&config);
            config
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(2);
        }
    }
}

/// Keep Rusta's keys out of every process it starts: no command, hook or MCP
/// server sees a variable Rusta reads a key from, `config`'s `api_key_env`
/// included. Registered as each config is loaded, before anything is started.
fn withhold_credentials(config: &config::Config) {
    agent_core::child_env::withhold(endpoints::credential_env_vars());
    agent_core::child_env::withhold(config.api_key_env.clone());
}

/// How this run settles project settings that need trust: an interactive
/// session asks, a headless run (or one without a terminal to ask at) warns,
/// and a command that runs no session says nothing — `doctor` reports it.
pub(crate) fn trust_mode(cli: &Cli) -> trust::Mode {
    use std::io::IsTerminal;
    let session = match &cli.command {
        None => true,
        #[cfg(feature = "tui")]
        Some(crate::cli::Commands::Tui) => true,
        Some(_) => false,
    };
    if !session {
        trust::Mode::Quiet
    } else if cli.prompt.is_none()
        && cli.prompt_file.is_none()
        && io::stdin().is_terminal()
        && io::stderr().is_terminal()
    {
        trust::Mode::Ask
    } else {
        trust::Mode::Warn
    }
}

/// Resolves Preserved Thinking: CLI flag > environment > config file.
///
/// `None` means "nobody asked", and is what lets the provider apply the model's
/// own default — Kimi models keep thinking content, GLM models do not. So a
/// `false` anywhere in the chain must survive as `Some(false)` rather than
/// collapsing back into "unset".
fn resolve_thinking_keep(
    cli: Option<bool>,
    env: Option<bool>,
    config: Option<bool>,
) -> Option<bool> {
    cli.or(env).or(config)
}

pub(crate) fn apply_config(cli: &mut Cli, config: &config::Config) {
    // `model`, `base_url`, and `api_key` in the config describe the config's
    // *provider*. When this invocation names a different one — `rusta
    // --provider glm` on a Kimi-configured machine — none of the three apply to
    // it, and inheriting them would send `kimi-for-coding` and a Kimi key to
    // Z.AI. Merge them only when the two providers agree.
    //
    // Naming *no* provider is not a disagreement: the config's provider is then
    // the one that gets used (right below), so everything sitting beside it in the
    // file describes it. Resolving `None` to the default and comparing that would
    // make a plain `rusta` on a GLM-configured machine disown its own key.
    let same_provider = match cli.provider.as_deref() {
        Some(named) => {
            provider_defaults::resolve_provider(Some(named))
                == provider_defaults::resolve_provider(config.provider.as_deref())
        }
        None => true,
    };

    if cli.provider.is_none() {
        cli.provider.clone_from(&config.provider);
    }
    if same_provider {
        if cli.model.is_none() {
            cli.model.clone_from(&config.model);
        }
        if cli.base_url.is_none() {
            cli.base_url.clone_from(&config.base_url);
        }
        if cli.api_key.is_none() {
            cli.api_key.clone_from(&config.api_key);
        }
        cli.api_key_env.clone_from(&config.api_key_env);
        cli.custom_headers.clone_from(&config.custom_headers);
    }
    if cli.search_url.is_none() {
        cli.search_url.clone_from(&config.search_url);
    }
    if cli.records.is_none() {
        cli.records = config.records.as_ref().map(config::expand_tilde);
    }
    if let Some(records) = cli.records.take() {
        cli.records = Some(config::expand_tilde(records));
    }
    if cli.reasoning_key.is_none() {
        cli.reasoning_key.clone_from(&config.reasoning_key);
    }
    if cli.reasoning_effort.is_none() {
        cli.reasoning_effort.clone_from(&config.reasoning_effort);
    }
    if cli.prompt_cache_ttl.is_none() {
        cli.prompt_cache_ttl = config.prompt_cache_ttl;
    }
    if cli.max_tokens.is_none() {
        cli.max_tokens = config.max_tokens;
    }
    if cli.project_context_files.is_empty() {
        cli.project_context_files = config
            .project_context_files
            .iter()
            .map(config::expand_tilde)
            .collect();
    }
    if cli.additional_dirs.is_empty() {
        cli.additional_dirs = config
            .additional_dirs
            .iter()
            .map(config::expand_tilde)
            .collect();
    }
    if cli.project_context_budget_tokens.is_none() {
        cli.project_context_budget_tokens = config.project_context_budget_tokens;
    }
    if cli.permission_mode.is_none() {
        cli.permission_mode.clone_from(&config.permission_mode);
    }
    if !cli.plan
        && let Some(true) = config.plan
    {
        // Plan mode waits for the user to review the plan, and nobody can in
        // a headless run that bypasses permissions.
        if cli.can_ask() {
            cli.plan = true;
        } else {
            eprintln!(
                "plan = true is not applied to this run: nobody can review a plan in a headless \
                 run that bypasses permissions."
            );
        }
    }
    if !cli.infinite_retry
        && let Some(true) = config.infinite_retry
    {
        cli.infinite_retry = true;
    }
    if !cli.inline_video
        && let Some(true) = config.inline_video
    {
        cli.inline_video = true;
    }
    if cli.allowed_paths.is_empty() {
        cli.allowed_paths = config
            .allowed_paths
            .iter()
            .map(config::expand_tilde)
            .collect();
    }
    if cli.billing.is_none() {
        cli.billing = config.billing;
    }
    if cli.region.is_none() {
        cli.region = config.region;
    }
    if cli.timeout.is_none() {
        cli.timeout = config.timeout;
    }
    if cli.idle_timeout.is_none() {
        cli.idle_timeout = config.idle_timeout;
    }
    cli.thinking_keep = resolve_thinking_keep(
        cli.thinking_keep,
        agent_core::agent_loop::llm::env_thinking_keep(),
        config.thinking_keep,
    );
    if cli.temperature.is_none() {
        cli.temperature = config.temperature;
    }
    if cli.top_p.is_none() {
        cli.top_p = config.top_p;
    }
    if !cli.thinking
        && let Some(true) = config.thinking
    {
        cli.thinking = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, apply_config, resolve_thinking_keep};
    use crate::PROVIDER_BUILTIN_TOOLS;
    use crate::endpoints::{Billing, Region};
    use crate::provider_defaults;
    use crate::provider_setup::{
        build_provider_for, configured_key_env, custom_headers_with, requires_api_key,
        resolve_api_key, resolve_api_key_with, resolve_base_url, resolve_timeouts,
    };
    use clap::Parser;
    use lm_bridge::providers::openai_compat::HttpTimeouts;
    use lm_bridge::providers::{GlmChatProvider, KimiChatProvider};

    /// `register_builtin_tools` clears exactly the names in `PROVIDER_BUILTIN_TOOLS`
    /// before registering the new provider's. A builtin missing from that list
    /// would survive a `/provider` switch and be sent to a vendor that does not
    /// accept it — Moonshot's `$web_search` goes out as a `builtin_function`, and
    /// Z.AI answers `tools[N].type: type is illegal` to *every* later request.
    #[test]
    fn every_provider_builtin_is_listed_for_removal() {
        let timeout = HttpTimeouts::idle(std::time::Duration::from_secs(1));
        let providers: Vec<Box<dyn lm_bridge::provider::ChatProvider>> = vec![
            Box::new(
                KimiChatProvider::new(
                    "kimi-for-coding".to_string(),
                    Some("k".to_string()),
                    "https://api.kimi.com/coding/v1".to_string(),
                    timeout,
                )
                .unwrap(),
            ),
            Box::new(
                GlmChatProvider::new(
                    // The default for `--provider glm`, so the test covers what
                    // a user actually gets.
                    "glm-5.4".to_string(),
                    Some("k".to_string()),
                    "https://api.z.ai/api/coding/paas/v4".to_string(),
                    timeout,
                )
                .unwrap(),
            ),
        ];

        for provider in providers {
            for tool in provider.builtin_tools() {
                assert!(
                    PROVIDER_BUILTIN_TOOLS.contains(&tool.name.as_str()),
                    "{} declares the builtin `{}`, which is not cleared on a provider \
                     switch — it would leak into the next vendor's request",
                    provider.name(),
                    tool.name
                );
            }
        }
    }

    /// A keyless run against a vendor endpoint must be refused *here*, with a
    /// message naming the key — not by the vendor, whose 401 says "Invalid
    /// Authentication" and sends people off hunting a typo in a key they never
    /// set. A local OpenAI-protocol server, though, often needs no key at all.
    #[test]
    fn a_missing_key_is_an_error_only_against_the_vendor_endpoints() {
        let (billing, region) = (Billing::default(), Region::default());

        for (provider, url) in [
            ("kimi", "https://api.kimi.com/coding/v1"),
            ("glm", "https://api.z.ai/api/coding/paas/v4"),
            ("openai", "https://api.openai.com/v1"),
            ("deepseek", "https://api.deepseek.com"),
        ] {
            assert!(
                requires_api_key(provider, url, billing, region),
                "{provider} must demand a key for its own endpoint"
            );
        }

        assert!(
            !requires_api_key("openai", "http://localhost:8000/v1", billing, region),
            "a local server must be allowed to run without a key"
        );
    }

    /// CLI beats environment beats config, and an explicit `false` at any level
    /// survives rather than collapsing back into "unset" — otherwise the Kimi
    /// Preserved Thinking default could never be switched off.
    #[test]
    fn precedence_is_cli_then_env_then_config() {
        // Nobody asked: the provider applies the model's own default.
        assert_eq!(resolve_thinking_keep(None, None, None), None);

        assert_eq!(resolve_thinking_keep(None, None, Some(true)), Some(true));
        assert_eq!(resolve_thinking_keep(None, None, Some(false)), Some(false));

        // Env beats config, in both directions.
        assert_eq!(
            resolve_thinking_keep(None, Some(false), Some(true)),
            Some(false)
        );
        assert_eq!(
            resolve_thinking_keep(None, Some(true), Some(false)),
            Some(true)
        );

        // CLI beats everything, in both directions.
        assert_eq!(
            resolve_thinking_keep(Some(false), Some(true), Some(true)),
            Some(false),
            "--thinking-keep=false must switch off the Kimi default"
        );
        assert_eq!(
            resolve_thinking_keep(Some(true), Some(false), Some(false)),
            Some(true)
        );
    }

    /// An unrecognized env value arrives as `None`, so it must fall through to
    /// the config file rather than read as `false`.
    #[test]
    fn unrecognized_env_falls_through_to_config() {
        assert_eq!(resolve_thinking_keep(None, None, Some(true)), Some(true));
    }

    /// `/provider glm` in a Kimi session must not reach for the Kimi key.
    ///
    /// `rusta init` writes one `api_key` beside one `provider`, so after setup
    /// `cli.api_key` is always populated — and reusing it across a switch would
    /// send that key to the other vendor, whose 401 reads like a bad key. The key
    /// belongs to the provider it was configured for, and nowhere else.
    #[test]
    fn switching_provider_does_not_reuse_the_other_providers_key() {
        let mut cli = Cli::parse_from(["rusta"]);
        cli.provider = Some("kimi".into());
        cli.api_key = Some("sk-kimi-secret".into());
        cli.base_url = Some("https://api.kimi.com/coding/v1".into());

        // Staying on the configured provider: the key and endpoint are its own.
        assert_eq!(
            resolve_api_key(&cli, "kimi").as_deref(),
            Some("sk-kimi-secret")
        );
        assert_eq!(
            resolve_base_url(&cli, "kimi", Billing::default(), Region::default()),
            "https://api.kimi.com/coding/v1"
        );

        // Switching away: neither may follow. With no ZAI key in the environment
        // this is `None` — an honest "you have no GLM key" instead of a 401.
        assert_ne!(
            resolve_api_key(&cli, "glm").as_deref(),
            Some("sk-kimi-secret"),
            "the Kimi key must never be sent to Z.AI"
        );
        assert_eq!(
            resolve_base_url(&cli, "glm", Billing::default(), Region::default()),
            "https://api.z.ai/api/coding/paas/v4",
            "the endpoint must be re-derived for the provider being built"
        );
    }

    /// With no provider named anywhere, the default provider owns the key — the
    /// plain `KIMI_API_KEY`-in-the-environment case must keep working.
    #[test]
    fn the_default_provider_still_owns_an_explicit_key() {
        let mut cli = Cli::parse_from(["rusta"]);
        cli.api_key = Some("sk-explicit".into());
        assert_eq!(
            resolve_api_key(&cli, provider_defaults::DEFAULT_PROVIDER).as_deref(),
            Some("sk-explicit")
        );
    }

    /// `rusta --provider glm` on a Kimi-configured machine must not inherit
    /// Kimi's model, endpoint, or key from the config. All three describe the
    /// config's provider, and handing them to Z.AI is a guaranteed failure.
    /// The config's prompt-cache lifetime applies unless the command line
    /// names one.
    #[test]
    fn the_configs_prompt_cache_lifetime_applies_below_the_flag() {
        use lm_bridge::provider::PromptCacheTtl;
        let config = crate::config::Config {
            prompt_cache_ttl: Some(PromptCacheTtl::FiveMinutes),
            ..Default::default()
        };
        let mut cli = Cli::parse_from(["rusta"]);
        apply_config(&mut cli, &config);
        assert_eq!(cli.cache_ttl(), Some(PromptCacheTtl::FiveMinutes));

        let mut cli = Cli::parse_from(["rusta", "--prompt-cache-ttl", "1h"]);
        apply_config(&mut cli, &config);
        assert_eq!(cli.cache_ttl(), Some(PromptCacheTtl::OneHour));
    }

    #[test]
    fn a_cli_provider_does_not_inherit_the_configs_other_providers_settings() {
        let config = crate::config::Config {
            provider: Some("kimi".into()),
            model: Some("kimi-for-coding".into()),
            base_url: Some("https://api.kimi.com/coding/v1".into()),
            api_key: Some("sk-kimi-secret".into()),
            ..Default::default()
        };

        let mut cli = Cli::parse_from(["rusta", "--provider", "glm"]);
        apply_config(&mut cli, &config);

        assert_eq!(cli.provider.as_deref(), Some("glm"));
        assert_eq!(cli.model, None, "Kimi's model must not reach GLM");
        assert_eq!(cli.base_url, None, "Kimi's endpoint must not reach GLM");
        assert_eq!(cli.api_key, None, "Kimi's key must not reach GLM");

        // ...and the endpoint GLM is actually built with is its own.
        assert_eq!(
            resolve_base_url(&cli, "glm", Billing::default(), Region::default()),
            "https://api.z.ai/api/coding/paas/v4"
        );

        // Staying on the configured provider still inherits everything.
        let mut cli = Cli::parse_from(["rusta", "--provider", "kimi"]);
        apply_config(&mut cli, &config);
        assert_eq!(cli.api_key.as_deref(), Some("sk-kimi-secret"));
        assert_eq!(cli.model.as_deref(), Some("kimi-for-coding"));
    }

    /// Naming no provider is not a mismatch.
    ///
    /// A bare `rusta` on a GLM-configured machine uses the config's provider,
    /// so the key, model, and endpoint beside it are *its own*. Resolving the
    /// absent `--provider` to the default (`kimi`) and comparing that against the
    /// config would make GLM disown its own key and report "no API key for glm"
    /// straight after `rusta init` had just verified and saved it.
    #[test]
    fn resolve_api_key_trims_surrounding_whitespace() {
        let mut cli = Cli::parse_from(["rusta"]);
        cli.provider = Some("kimi".into());
        // A key pasted or exported with a trailing newline must not reach the
        // Authorization header verbatim.
        cli.api_key = Some("  sk-kimi-key\n".into());
        assert_eq!(
            resolve_api_key(&cli, "kimi").as_deref(),
            Some("sk-kimi-key")
        );

        // A whitespace-only explicit key resolves to nothing, not an empty key.
        cli.api_key = Some("   ".into());
        assert_eq!(resolve_api_key(&cli, "kimi"), None);
    }

    /// A configured `api_key_env` is where the key comes from — and the only
    /// place: a set `KIMI_API_KEY` does not stand in for a missing variable.
    #[test]
    fn api_key_env_names_the_keys_variable() {
        const NAMED: &str = "RUSTA_TEST_NAMED_KEY";
        let mut cli = Cli::parse_from(["rusta"]);
        cli.provider = Some("kimi".into());
        cli.api_key_env = Some(NAMED.into());

        let named = |var: &str| (var == NAMED).then(|| "sk-named".to_string());
        assert_eq!(
            resolve_api_key_with(&cli, "kimi", named).as_deref(),
            Some("sk-named")
        );
        let kimi_key = |var: &str| (var == "KIMI_API_KEY").then(|| "sk-kimi".to_string());
        assert_eq!(resolve_api_key_with(&cli, "kimi", kimi_key), None);

        // Nothing sets the named variable in this process.
        let timeouts = resolve_timeouts(&cli);
        let err = build_provider_for(&cli, None, None, timeouts)
            .err()
            .expect("a missing named variable is an error");
        assert!(err.to_string().contains(NAMED), "{err}");

        // It applies to the configured provider only.
        assert_eq!(configured_key_env(&cli, "glm"), None);
    }

    /// `RUSTA_CUSTOM_HEADERS` lines are merged under the config's table.
    #[test]
    fn custom_headers_merge_env_under_config() {
        let mut cli = Cli::parse_from(["rusta"]);
        cli.provider = Some("glm".into());
        cli.custom_headers
            .insert("X-Route".into(), "from-config".into());
        let env = |var: &str| {
            (var == "RUSTA_CUSTOM_HEADERS").then(|| "X-Route: from-env\nX-Trace: on".to_string())
        };
        let headers = custom_headers_with(&cli, "glm", env).unwrap();
        assert_eq!(headers["x-route"], "from-config");
        assert_eq!(headers["x-trace"], "on");
        let other = custom_headers_with(&cli, "kimi", |_| None).unwrap();
        assert!(
            other.get("x-route").is_none(),
            "config headers are the provider's"
        );

        cli.custom_headers.insert("Bad Name".into(), "x".into());
        assert!(custom_headers_with(&cli, "glm", |_| None).is_err());
    }

    #[test]
    fn a_bare_invocation_inherits_the_configured_providers_settings() {
        let config = crate::config::Config {
            provider: Some("glm".into()),
            api_key: Some("zai-secret".into()),
            billing: Some(Billing::Subscription),
            ..Default::default()
        };

        let mut cli = Cli::parse_from(["rusta"]);
        apply_config(&mut cli, &config);

        assert_eq!(cli.provider.as_deref(), Some("glm"));
        assert_eq!(
            cli.api_key.as_deref(),
            Some("zai-secret"),
            "a bare `rusta` must use the key `rusta init` just saved"
        );
        assert_eq!(
            resolve_api_key(&cli, "glm").as_deref(),
            Some("zai-secret"),
            "and it must survive all the way to the provider being built"
        );
        assert_eq!(
            resolve_base_url(&cli, "glm", Billing::Subscription, Region::default()),
            "https://api.z.ai/api/coding/paas/v4"
        );
    }
}
