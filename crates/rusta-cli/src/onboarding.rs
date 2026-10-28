//! First-run setup: pick a provider, say which key you have, paste it.
//!
//! The one thing a new user cannot guess is that a Coding Plan key and a
//! pay-per-token key are *different keys for different endpoints*. Kimi answers
//! one sent to the other's URL with `invalid_authentication_error`, which reads
//! like a bad key and sends people off checking their clipboard. So the wizard
//! asks which key you have, derives the endpoint and the flagship model from
//! that, and then **proves the pairing works before writing anything** — and if
//! the key is rejected, it offers the other plan rather than leaving you to
//! guess. Z.AI takes a key at either endpoint and bills it by its type, so for
//! GLM the check proves the key, and the wizard says which type the plan needs.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use lm_bridge::providers::kimi_catalog::KimiModelInfo;

use crate::cli::Cli;
use crate::config::Config;
use crate::endpoints::{self, Billing, Region};
use crate::kimi::{Probe, probe};
use crate::provider_defaults;

/// Providers the wizard offers, in the order it offers them.
const CHOICES: &[(&str, &str)] = &[
    (
        "kimi",
        "Kimi   (Moonshot)  — kimi-for-coding / K2.8 Preview, 1M context",
    ),
    ("glm", "GLM    (z.ai)      — glm-5.3, 1M context"),
    (
        "deepseek",
        "DeepSeek           — deepseek-flash, 1M context, hybrid thinking, vision",
    ),
    ("openai", "OpenAI-compatible endpoint"),
];

/// Whether to offer setup: no key anywhere, and a human is watching.
///
/// Never fires in a pipe, under `-p`, or for a subcommand — those must keep
/// failing loudly with exit 1 rather than blocking on a prompt.
///
/// Reads the *merged* `cli`, never the config directly. `apply_config` has
/// already folded the config in, and it scopes the key to the provider it was
/// written for — so a Kimi key in the config cannot convince this that a GLM
/// session is ready to go.
#[must_use]
pub fn should_offer(cli: &Cli) -> bool {
    if cli.command.is_some() || cli.prompt.is_some() {
        return false;
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return false;
    }
    resolved_key(cli).is_none()
}

/// The key the runtime would find for the provider it is about to build.
fn resolved_key(cli: &Cli) -> Option<String> {
    let provider = provider_defaults::resolve_provider(cli.provider.as_deref());
    cli.api_key
        .clone()
        .or_else(|| endpoints::api_key_env_var(&provider).and_then(endpoints::env_non_empty))
}

/// Run the wizard and save the result. Returns the path written.
pub async fn run(cli: &Cli, config: &Config) -> anyhow::Result<PathBuf> {
    println!();
    println!("  Rusta setup");
    println!();

    let provider = ask_provider(cli, config)?;
    let billing = ask_billing(cli, &provider, config)?;
    let region = ask_region(cli, &provider, billing, config)?;
    let custom_url = ask_custom_base_url(&provider, billing, region)?;

    let model = endpoints::default_model(&provider, billing)
        .ok_or_else(|| anyhow::anyhow!("unknown provider {provider}"))?;
    let base_url = match &custom_url {
        Some(url) => url.clone(),
        None => endpoints::base_url(&provider, billing, region)
            .ok_or_else(|| anyhow::anyhow!("no endpoint for {provider}"))?
            .to_string(),
    };

    println!();
    println!("  endpoint  {base_url}");
    println!("  model     {model}");
    println!();

    let verified = ask_key_until_valid(
        &provider,
        billing,
        region,
        base_url,
        custom_url.is_some(),
        &crate::custom_headers(cli, &provider)?,
    )
    .await?;
    let Verified {
        key,
        billing,
        base_url,
        models,
    } = verified;

    // The plan may have changed if the key turned out to belong to the other one.
    let model = endpoints::default_model(&provider, billing)
        .ok_or_else(|| anyhow::anyhow!("unknown provider {provider}"))?;

    // A key accepted only by the other Kimi Coding host is pinned there, so
    // every later run goes straight to the host that takes it.
    let table_url = endpoints::base_url(&provider, billing, region);
    let pinned =
        custom_url.or_else(|| (table_url != Some(base_url.as_str())).then(|| base_url.clone()));
    let path = save(&provider, billing, region, &key, pinned.as_deref())?;
    if provider == "kimi" && !models.is_empty() {
        println!();
        println!("  models on {base_url}:");
        for line in crate::kimi::describe_models(&models) {
            println!("    {line}");
        }
        if let Some(cache) = crate::catalogs::catalog_cache::<KimiModelInfo>()
            && let Err(e) = cache.save(&base_url, &models)
        {
            println!("  (could not cache the model list: {e})");
        }
    }
    println!();
    println!("  saved to {}", path.display());
    println!("  using {model} on {base_url}");
    println!();
    Ok(path)
}

/// A key the endpoint accepted, where, and the models it lists.
struct Verified {
    key: String,
    billing: Billing,
    base_url: String,
    models: Vec<KimiModelInfo>,
}

/// Ask which provider, unless this invocation already named one.
///
/// A provider in the *config* is last run's answer, not this one's: it seeds the
/// default so pressing Enter keeps it, but it must not skip the question, or
/// `rusta init` could never move you off the provider you first picked. A
/// provider on the *command line* is an instruction for right now, so it stands.
fn ask_provider(cli: &Cli, config: &Config) -> anyhow::Result<String> {
    if let Some(p) = cli.provider.as_deref() {
        return Ok(p.to_string());
    }
    let configured = config.provider.as_deref();
    let default = configured
        .and_then(|p| CHOICES.iter().position(|(id, _)| *id == p))
        .unwrap_or(0);

    println!("  Which provider?");
    for (i, (_, label)) in CHOICES.iter().enumerate() {
        println!("    {}) {label}", i + 1);
    }
    let n = ask_index("  Provider", CHOICES.len(), default + 1)?;
    Ok(CHOICES[n].0.to_string())
}

/// Ask which plan the key belongs to — but only where the plans are two doors.
///
/// The default is seeded the same way `ask_provider` seeds its own: a `--billing`
/// on the command line is an instruction for right now and stands; otherwise last
/// run's saved plan pre-fills the prompt, so a pay-per-token user rotating a key
/// keeps their plan by pressing Enter. Hard-defaulting to Subscription silently
/// re-pointed them at the Coding Plan endpoint, which rejects a pay-per-token key
/// as `invalid_authentication_error` — a valid key reading as a bad one.
fn ask_billing(cli: &Cli, provider: &str, config: &Config) -> anyhow::Result<Billing> {
    if !endpoints::billing_matters(provider) {
        return Ok(Billing::default());
    }
    if let Some(b) = cli.billing {
        return Ok(b);
    }
    let default = match config.billing.unwrap_or_default() {
        Billing::Subscription => 1,
        Billing::Api => 2,
    };
    println!();
    println!("  Which key do you have? (they are not interchangeable)");
    println!("    1) {}", Billing::Subscription.label());
    println!("    2) {}", Billing::Api.label());
    let n = ask_index("  Key type", 2, default)?;
    Ok(if n == 0 {
        Billing::Subscription
    } else {
        Billing::Api
    })
}

/// Offer a custom endpoint where the provider is a *protocol*, not a vendor.
///
/// `openai` here means any OpenAI-protocol server — a local vLLM, an OpenRouter,
/// an in-house gateway. Hardcoding `api.openai.com` would make the choice a lie.
/// Kimi, GLM and DeepSeek are vendors with known endpoints, so there is nothing
/// to ask.
fn ask_custom_base_url(
    provider: &str,
    billing: Billing,
    region: Region,
) -> anyhow::Result<Option<String>> {
    if provider != "openai" {
        return Ok(None);
    }
    let default = endpoints::base_url(provider, billing, region)
        .ok_or_else(|| anyhow::anyhow!("no endpoint for {provider}"))?;

    println!();
    println!("  Endpoint — any OpenAI-protocol server. Enter for OpenAI itself.");
    print!("  Base URL [{default}]: ");
    std::io::stdout().flush().ok();

    let line = read_line()?;
    let line = line.trim();
    Ok(if line.is_empty() || line == default {
        None
    } else {
        Some(line.to_string())
    })
}

fn ask_region(
    cli: &Cli,
    provider: &str,
    billing: Billing,
    config: &Config,
) -> anyhow::Result<Region> {
    // Only ask where the answer changes the endpoint.
    if !endpoints::region_matters(provider, billing) {
        return Ok(Region::default());
    }
    if let Some(r) = cli.region {
        return Ok(r);
    }
    // Same reasoning as `ask_billing`: a China user rotating a key keeps their
    // region by pressing Enter instead of being flipped to the Global endpoint.
    let default = match config.region.unwrap_or_default() {
        Region::Global => 1,
        Region::China => 2,
    };
    println!();
    println!("  Which region?");
    println!("    1) {}", Region::Global.label());
    println!("    2) {}", Region::China.label());
    let n = ask_index("  Region", 2, default)?;
    Ok(if n == 0 {
        Region::Global
    } else {
        Region::China
    })
}

/// What a GLM key's acceptance does not prove: Z.AI takes any valid key at
/// either endpoint and bills it by its type, so the key must be the one
/// `billing` names. `None` for a provider whose endpoints refuse the other
/// plan's keys.
fn key_type_note(provider: &str, billing: Billing) -> Option<String> {
    if !matches!(provider, "glm" | "zai") {
        return None;
    }
    let kind = match billing {
        Billing::Subscription => "Personal Coding Plan (or Team Coding Plan)",
        Billing::Api => "Standard API Key",
    };
    Some(format!(
        "Z.AI accepts any key here and bills it by its type: for a {} it must be a \
         {kind} key, as z.ai's API Keys page lists it.",
        billing.label()
    ))
}

/// Prompt for the key, verify it against the endpoint, and — when the endpoint
/// rejects it — offer the other plan instead of just failing.
///
/// The other-plan offer only makes sense when there *is* another door: not for a
/// single-endpoint provider, and not for a custom URL, which is outside the
/// table. There, a rejection just means the key is wrong.
async fn ask_key_until_valid(
    provider: &str,
    billing: Billing,
    region: Region,
    mut base_url: String,
    custom_url: bool,
    headers: &reqwest::header::HeaderMap,
) -> anyhow::Result<Verified> {
    if let Some(url) = endpoints::signup_url(provider, billing) {
        println!("  Create a key: {url}");
    }
    let switchable = endpoints::billing_matters(provider) && !custom_url;

    for attempt in 0..3 {
        let key = prompt_secret("  Paste your API key")?;
        if key.is_empty() {
            anyhow::bail!("no API key given");
        }

        print!("  checking… ");
        std::io::stdout().flush().ok();
        let mut verdict = probe(&base_url, &key, headers).await;
        // Kimi has two Coding hosts, for mainland and global accounts; a key
        // refused by one may belong to the other.
        if matches!(verdict, Probe::Rejected)
            && !custom_url
            && let Some(other_host) = endpoints::alternate_kimi_coding_host(&base_url)
        {
            println!("{base_url} refused that key");
            print!("  trying {other_host}… ");
            std::io::stdout().flush().ok();
            let there = probe(other_host, &key, headers).await;
            if matches!(there, Probe::Accepted(_)) {
                base_url = other_host.to_string();
            }
            verdict = there;
        }
        match verdict {
            Probe::Accepted(models) => {
                println!("key accepted");
                if !custom_url && let Some(note) = key_type_note(provider, billing) {
                    println!("  {note}");
                }
                return Ok(Verified {
                    key,
                    billing,
                    base_url,
                    models,
                });
            }
            Probe::Rejected if !switchable => {
                println!("{base_url} rejected that key");
                if attempt == 2 {
                    anyhow::bail!("key rejected by {base_url}");
                }
                println!("  Check the key and try again.");
                println!();
            }
            Probe::Rejected => {
                let other = match billing {
                    Billing::Subscription => Billing::Api,
                    Billing::Api => Billing::Subscription,
                };
                let other_url = endpoints::base_url(provider, other, region)
                    .ok_or_else(|| anyhow::anyhow!("no endpoint for {provider}"))?;

                println!("the {} endpoint rejected that key", billing.label());
                println!();
                println!("  That usually means it belongs to the other plan.");
                print!("  Try it as a {}? [Y/n] ", other.label());
                std::io::stdout().flush().ok();

                if !read_yes()? {
                    anyhow::bail!("key rejected by {base_url}");
                }
                print!("  checking… ");
                std::io::stdout().flush().ok();
                if let Probe::Accepted(models) = probe(other_url, &key, headers).await {
                    println!("key accepted as a {}", other.label());
                    return Ok(Verified {
                        key,
                        billing: other,
                        base_url: other_url.to_string(),
                        models,
                    });
                }
                println!("that endpoint rejected it too");
                if attempt == 2 {
                    anyhow::bail!("key rejected by both endpoints");
                }
                println!("  Check the key and try again.");
                println!();
            }
            Probe::Unreachable(e) => {
                println!("could not reach {base_url}: {e}");
                anyhow::bail!("endpoint unreachable");
            }
        }
    }
    anyhow::bail!("could not verify the API key")
}

/// The header written into a config file this wizard creates from scratch.
const NEW_CONFIG_HEADER: &str = "\
# Written by `rusta init`.
#
# The model and the endpoint follow from provider + billing (+ region), so a
# Rusta that ships a newer flagship picks it up without editing anything.
# Set `model` or `base_url` to pin a specific one.
";

/// Write the answers to the user config, with the key readable only by the user.
///
/// This **edits** an existing config rather than replacing it. A user config is
/// where permission rules, MCP servers, and sampling settings live; rewriting the
/// whole file would silently delete all of it the first time someone re-ran
/// `init` to rotate a key. `toml_edit` keeps their comments and layout intact and
/// touches only the keys the wizard owns.
///
/// The invariant it maintains: **the endpoint written here is the one the key was
/// just verified against.** A `base_url` left pinned from an earlier run would
/// override the derived one, sending the freshly-verified key somewhere it was
/// never checked — so it is always re-derived, never inherited.
fn save(
    provider: &str,
    billing: Billing,
    region: Region,
    key: &str,
    custom_url: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let dir = agent_core::user_dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("no config directory"))?
        .join("rusta");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");

    let existing = load_existing(&path)?;
    let edited = edit_config(&existing, provider, billing, region, key, custom_url)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;

    if edited.dropped_model {
        println!("  dropped the pinned model — it was for the old plan");
    }

    std::fs::write(&path, edited.text)?;
    restrict_permissions(&path);
    Ok(path)
}

/// Read the config that is about to be edited.
///
/// Only "there is no config yet" may be read as an empty document. Any other
/// read failure — a permission problem, a transient I/O error — stops the save:
/// treating it as empty produces a *fresh* document, and writing that erases the
/// permission rules, MCP servers and sampling settings [`save`] exists to
/// preserve. Failing to rotate a key is recoverable; silently deleting a config
/// is not.
fn load_existing(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => anyhow::bail!(
            "{} could not be read ({e}); refusing to overwrite it",
            path.display()
        ),
    }
}

/// The result of editing a config document.
struct Edited {
    text: String,
    dropped_model: bool,
}

/// Apply the wizard's answers to a config document.
///
/// Pure, so the property that actually matters — *everything the user already had
/// is still there afterwards* — is testable without touching a real config.
fn edit_config(
    existing: &str,
    provider: &str,
    billing: Billing,
    region: Region,
    key: &str,
    custom_url: Option<&str>,
) -> anyhow::Result<Edited> {
    let mut doc = if existing.trim().is_empty() {
        NEW_CONFIG_HEADER.parse::<toml_edit::DocumentMut>()?
    } else {
        existing
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| anyhow::anyhow!("not valid TOML: {e}"))?
    };

    // A pinned `model` names a model of whatever provider/plan was configured
    // last time. Re-pointing to another one leaves it naming a model the new
    // endpoint does not serve — `kimi-for-coding` sent to Z.AI. A pin made under
    // the *same* pairing is a deliberate choice, so it survives.
    let repointed = doc.get("provider").and_then(|v| v.as_str()) != Some(provider)
        || doc.get("billing").and_then(|v| v.as_str()) != Some(&toml_value(billing)?);
    let dropped_model = repointed && doc.remove("model").is_some();

    doc["provider"] = toml_edit::value(provider);
    // A billing line only means something where the plan picks the endpoint;
    // elsewhere (DeepSeek, a plain OpenAI-protocol server) it would be a
    // setting that silently does nothing, so an old one is dropped too.
    if endpoints::billing_matters(provider) {
        doc["billing"] = toml_edit::value(toml_value(billing)?);
    } else {
        doc.remove("billing");
    }
    doc["api_key"] = toml_edit::value(key);
    // The key now lives in the file; a variable named for it would conflict.
    doc.remove("api_key_env");

    // Writing a region where it cannot apply (Kimi's Coding Plan has one
    // endpoint) would be a setting that silently does nothing.
    if endpoints::region_matters(provider, billing) {
        doc["region"] = toml_edit::value(toml_value(region)?);
    } else {
        doc.remove("region");
    }

    // A custom endpoint is not derivable from the table, so it is the one thing
    // here that *must* be pinned. Otherwise the derived one wins — see above.
    match custom_url {
        Some(url) => doc["base_url"] = toml_edit::value(url),
        None => {
            doc.remove("base_url");
        }
    }

    Ok(Edited {
        text: doc.to_string(),
        dropped_model,
    })
}

/// The key is a secret; keep it out of other users' reach.
#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

/// Serialize a `ValueEnum` the way the config file spells it.
fn toml_value<T: serde::Serialize>(v: T) -> anyhow::Result<String> {
    let s = toml::Value::try_from(v)?;
    Ok(s.as_str()
        .ok_or_else(|| anyhow::anyhow!("not a string"))?
        .to_string())
}

// ---------------------------------------------------------------- prompting --

/// Read a 1-based menu choice, returning a 0-based index. Empty input takes
/// `default` (also 1-based).
fn ask_index(label: &str, max: usize, default: usize) -> anyhow::Result<usize> {
    loop {
        print!("{label} [{default}]: ");
        std::io::stdout().flush().ok();
        let line = read_line()?;
        let line = line.trim();
        if line.is_empty() {
            return Ok(default - 1);
        }
        match line.parse::<usize>() {
            Ok(n) if (1..=max).contains(&n) => return Ok(n - 1),
            _ => println!("  enter a number from 1 to {max}"),
        }
    }
}

fn read_line() -> anyhow::Result<String> {
    let mut s = String::new();
    if std::io::stdin().read_line(&mut s)? == 0 {
        anyhow::bail!("input closed");
    }
    Ok(s)
}

/// Default-yes confirmation.
fn read_yes() -> anyhow::Result<bool> {
    let s = read_line()?;
    let s = s.trim().to_lowercase();
    Ok(s.is_empty() || s == "y" || s == "yes")
}

/// Read a secret without echoing it.
///
/// Suppressing the echo needs a terminal to suppress it *on*: `rpassword` opens
/// `/dev/tty` and fails outright when there isn't one. A piped key has no echo
/// to hide, so read it plainly there — that is what keeps `rusta init`
/// scriptable (`echo "$KEY" | rusta init`) instead of dying with `ENXIO`.
fn prompt_secret(label: &str) -> anyhow::Result<String> {
    print!("{label}: ");
    std::io::stdout().flush().ok();
    let key = if std::io::stdin().is_terminal() {
        rpassword::read_password()?
    } else {
        read_line()?
    };
    Ok(key.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// The wizard must never block a pipe, a headless run, or a subcommand —
    /// those have to keep exiting 1 rather than waiting on a prompt no one can
    /// answer.
    #[test]
    fn a_glm_key_is_told_which_type_its_plan_needs() {
        let plan = key_type_note("glm", Billing::Subscription).unwrap();
        assert!(plan.contains("Personal Coding Plan"), "{plan}");
        let api = key_type_note("zai", Billing::Api).unwrap();
        assert!(api.contains("Standard API Key"), "{api}");
        assert_eq!(key_type_note("kimi", Billing::Subscription), None);
        assert_eq!(key_type_note("deepseek", Billing::Api), None);
    }

    #[test]
    fn never_offers_setup_for_non_interactive_use() {
        let mut cli = Cli::parse_from(["rusta"]);

        cli.prompt = Some("do a thing".into());
        assert!(!should_offer(&cli), "must not prompt under -p");

        cli.prompt = None;
        cli.command = Some(crate::cli::Commands::Doctor);
        assert!(!should_offer(&cli), "must not prompt for a subcommand");
    }

    /// Setup is pointless once a key is reachable.
    #[test]
    fn does_not_offer_setup_when_a_key_is_already_configured() {
        let mut cli = Cli::parse_from(["rusta"]);
        cli.api_key = Some("sk-something".into());
        assert!(!should_offer(&cli));
        assert!(resolved_key(&cli).is_some());
    }

    #[test]
    fn a_missing_config_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_existing(&dir.path().join("nope.toml")).unwrap(), "");
    }

    /// **A config that exists but cannot be read must not be treated as empty.**
    /// Doing so builds a fresh document, and writing it erases the permission
    /// rules, MCP servers and sampling settings `save` exists to preserve —
    /// silently, on a re-run whose only purpose was rotating a key.
    ///
    /// A directory stands in for "exists but unreadable": `read_to_string` fails
    /// on one with an error that is not `NotFound`, on every platform, and
    /// without depending on permissions (which do not stop root).
    #[test]
    fn an_unreadable_config_is_refused_rather_than_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();
        let err = load_existing(&path).expect_err("an unreadable config must not load as empty");
        assert!(
            err.to_string().contains("refusing to overwrite"),
            "the refusal must say the config was left alone: {err}"
        );
    }

    fn write(existing: &str, provider: &str, billing: Billing, region: Region) -> String {
        edit_config(existing, provider, billing, region, "sk-test", None)
            .expect("edit must succeed")
            .text
    }

    /// Saving a key replaces an `api_key_env`: the two would conflict.
    #[test]
    fn saving_a_key_drops_api_key_env() {
        let text = write(
            "provider = \"kimi\"\napi_key_env = \"WORK_KIMI_KEY\"\n",
            "kimi",
            Billing::Subscription,
            Region::Global,
        );
        assert!(!text.contains("api_key_env"), "{text}");
        assert!(text.contains("api_key = \"sk-test\""), "{text}");
    }

    /// The config the wizard writes must round-trip through the real loader —
    /// otherwise setup "succeeds" and the next run cannot read it back.
    #[test]
    fn written_config_round_trips() {
        for (provider, billing, region) in [
            ("kimi", Billing::Subscription, Region::Global),
            ("kimi", Billing::Api, Region::China),
            ("glm", Billing::Subscription, Region::China),
            ("glm", Billing::Api, Region::Global),
            ("openai", Billing::Subscription, Region::Global),
            ("deepseek", Billing::Subscription, Region::Global),
        ] {
            let text = write("", provider, billing, region);
            let parsed: Config = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("{provider}/{billing:?} does not parse: {e}\n{text}"));

            assert_eq!(parsed.provider.as_deref(), Some(provider));
            assert_eq!(parsed.api_key.as_deref(), Some("sk-test"));
            if endpoints::region_matters(provider, billing) {
                assert_eq!(parsed.region, Some(region), "{provider}/{billing:?}");
            } else {
                // A region that cannot apply must not be written at all — it
                // would read as a setting that silently does nothing.
                assert_eq!(parsed.region, None, "{provider}/{billing:?}");
            }
            if endpoints::billing_matters(provider) {
                assert_eq!(parsed.billing, Some(billing), "{provider}/{billing:?}");
            } else {
                // Same for the plan: no billing line where it cannot apply.
                assert_eq!(parsed.billing, None, "{provider}/{billing:?}");
            }
        }
    }

    /// The DeepSeek path of the wizard: it is on the menu, the plan and region
    /// questions cannot change anything so they are not asked, and the saved
    /// config carries no `billing` or `region` line — both would be settings
    /// that silently do nothing.
    #[test]
    fn deepseek_setup_skips_the_plan_and_region_questions() {
        let (id, _) = CHOICES
            .iter()
            .find(|(id, _)| *id == "deepseek")
            .expect("deepseek is on the wizard's menu");
        assert!(
            !endpoints::billing_matters(id),
            "the plan picks no endpoint"
        );
        assert!(
            !endpoints::region_matters(id, Billing::Subscription),
            "the region picks no endpoint"
        );

        let text = write("", "deepseek", Billing::Subscription, Region::Global);
        assert!(text.contains("provider = \"deepseek\""), "{text}");
        assert!(!text.contains("billing ="), "{text}");
        assert!(!text.contains("region ="), "{text}");
        let parsed: Config = toml::from_str(&text).expect("must parse");
        assert_eq!(parsed.billing, None);
        assert_eq!(parsed.region, None);

        // A stale billing line from an earlier Kimi or GLM setup is dropped.
        let existing = "provider = \"kimi\"\nbilling = \"api\"\nregion = \"global\"\n";
        let text = write(existing, "deepseek", Billing::Subscription, Region::Global);
        let parsed: Config = toml::from_str(&text).expect("must parse");
        assert_eq!(parsed.billing, None, "{text}");
        assert_eq!(parsed.region, None, "{text}");
    }

    /// **A user config is not the wizard's to own.** Permission rules, MCP
    /// servers, and sampling settings live there too, and re-running `init` to
    /// rotate a key must not take them with it.
    #[test]
    fn init_preserves_everything_else_in_an_existing_config() {
        let existing = r#"
# My carefully tuned setup.
permission_mode = "accept_edits"
temperature = 0.6
allowed_paths = ["/srv/data"]

[[permissions.rules]]
name = "allow-reads"
tool = "read_file"
action = "allow"

[[mcp_servers]]
name = "my-server"
command = "/opt/my-server"
"#;
        let text = write(existing, "kimi", Billing::Subscription, Region::Global);
        let parsed: Config = toml::from_str(&text).expect("must still parse");

        assert_eq!(parsed.permission_mode.as_deref(), Some("accept_edits"));
        assert_eq!(parsed.temperature, Some(0.6));
        assert_eq!(parsed.allowed_paths.len(), 1);
        assert_eq!(parsed.mcp_servers.len(), 1, "the MCP server must survive");
        assert_eq!(
            parsed.permissions.expect("rules must survive").rules.len(),
            1
        );

        // ...and the comment the user wrote is still theirs.
        assert!(text.contains("# My carefully tuned setup."));

        // ...while the wizard's own keys did land.
        assert_eq!(parsed.provider.as_deref(), Some("kimi"));
        assert_eq!(parsed.api_key.as_deref(), Some("sk-test"));
    }

    /// A `model` pinned for one provider names nothing the next one serves, so
    /// re-pointing must clear it rather than send `kimi-for-coding` to Z.AI.
    #[test]
    fn repointing_drops_a_model_pinned_for_the_old_plan() {
        let existing =
            "provider = \"kimi\"\nbilling = \"subscription\"\nmodel = \"kimi-for-coding\"\n";

        let moved = edit_config(
            existing,
            "glm",
            Billing::Subscription,
            Region::Global,
            "k",
            None,
        )
        .unwrap();
        assert!(moved.dropped_model, "must report what it dropped");
        let parsed: Config = toml::from_str(&moved.text).unwrap();
        assert_eq!(parsed.model, None, "a stale model must not survive");

        // Staying put is not re-pointing: a deliberate pin is the user's.
        let stayed = edit_config(
            existing,
            "kimi",
            Billing::Subscription,
            Region::Global,
            "k",
            None,
        )
        .unwrap();
        assert!(!stayed.dropped_model);
        let parsed: Config = toml::from_str(&stayed.text).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("kimi-for-coding"));
    }

    /// The endpoint in the config must be the one the key was just verified
    /// against. A `base_url` left over from an earlier plan would override the
    /// derived one and send the new key to the door that rejects it — the exact
    /// failure this wizard exists to prevent.
    #[test]
    fn a_stale_base_url_never_survives() {
        let existing =
            "provider = \"kimi\"\nbilling = \"api\"\nbase_url = \"https://api.moonshot.ai/v1\"\n";
        let text = write(existing, "kimi", Billing::Subscription, Region::Global);
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.base_url, None, "the derived endpoint must win");
    }

    /// A custom OpenAI-protocol endpoint is not in the table, so it is the one
    /// thing that has to be written down.
    #[test]
    fn a_custom_endpoint_is_pinned() {
        let edited = edit_config(
            "",
            "openai",
            Billing::Subscription,
            Region::Global,
            "k",
            Some("http://localhost:8000/v1"),
        )
        .unwrap();
        let parsed: Config = toml::from_str(&edited.text).unwrap();
        assert_eq!(parsed.base_url.as_deref(), Some("http://localhost:8000/v1"));
    }

    /// A key is pasted, not typed, and a stray quote or backslash in it must not
    /// produce a config file the next run refuses to parse.
    #[test]
    fn an_awkward_key_still_writes_a_parseable_config() {
        let key = r#"sk-"odd\key"#;
        let edited =
            edit_config("", "kimi", Billing::Subscription, Region::Global, key, None).unwrap();
        let parsed: Config = toml::from_str(&edited.text).expect("config must still parse");
        assert_eq!(parsed.api_key.as_deref(), Some(key));
    }

    /// A config we cannot parse must abort the save, not be silently replaced —
    /// clobbering it would destroy whatever the user was in the middle of fixing.
    #[test]
    fn a_malformed_config_aborts_rather_than_being_overwritten() {
        let broken = "provider = \"kimi\"\nthis is not toml";
        assert!(
            edit_config(
                broken,
                "kimi",
                Billing::Subscription,
                Region::Global,
                "k",
                None
            )
            .is_err()
        );
    }

    /// The wizard's spelling of the enums must match what the config parser
    /// accepts — a mismatch here writes a file the next run rejects.
    #[test]
    fn enum_values_match_the_config_vocabulary() {
        assert_eq!(toml_value(Billing::Subscription).unwrap(), "subscription");
        assert_eq!(toml_value(Billing::Api).unwrap(), "api");
        assert_eq!(toml_value(Region::Global).unwrap(), "global");
        assert_eq!(toml_value(Region::China).unwrap(), "china");
    }
}
