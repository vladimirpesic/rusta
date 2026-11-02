use std::time::Duration;

use agent_core::permissions::PermissionMode;
use lm_bridge::providers::catalog_cache::CatalogCache;
use lm_bridge::providers::deepseek_catalog::DeepseekModelInfo;
use lm_bridge::providers::glm::{CodingPlanModel, GlmChatProvider};
use lm_bridge::providers::kimi_catalog::KimiModelInfo;
use mcp_client::manager::{McpConnectionManager, McpStatus};

use crate::kimi::{Probe, describe_models, probe};

use crate::provider_defaults::{
    DEFAULT_PROVIDER, ModelSource, VALID_PROVIDERS, resolve_model, resolve_provider,
    resolve_provider_with,
};
use crate::trust::{Outcome, Report};
use crate::{cli::Cli, config::Config};

/// Fallback wait for MCP servers when no server declares a startup timeout.
const DEFAULT_MCP_WAIT_MS: u64 = 2_000;

/// Upper bound on how long `doctor` waits for MCP servers, so a server with a
/// generous `startup_timeout_ms` (Devscriptor asks for 60 s) cannot stall it.
const MAX_MCP_WAIT_MS: u64 = 10_000;

/// Result of a single `doctor` check.
struct Check {
    name: &'static str,
    status: Status,
    message: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Error,
}

/// Run all diagnostic checks and print a report. `trust` is what workspace
/// trust made of the project config, when there is one.
pub async fn run(cli: &Cli, config: &Config, trust: Option<&Report>) {
    let mut checks = vec![
        check_provider(cli),
        check_endpoint(cli),
        check_api_key(cli),
        check_model(cli),
    ];
    checks.extend(check_kimi(cli).await);
    checks.extend(check_glm_plan(cli, config));
    checks.extend(check_deepseek(cli).await);
    checks.extend(check_vision_mcp(config));
    checks.extend(trust.map(check_trust));
    checks.extend([
        check_permission_mode(cli, config),
        check_records(cli),
        check_devscriptor(config),
        check_mcp_servers(config).await,
    ]);

    for check in &checks {
        check.print();
    }

    let errors = checks
        .iter()
        .filter(|c| matches!(c.status, Status::Error))
        .count();
    let warnings = checks
        .iter()
        .filter(|c| matches!(c.status, Status::Warn))
        .count();

    println!(
        "\n{} check(s) passed, {warnings} warning(s), {errors} error(s)",
        checks.len() - errors - warnings
    );

    if errors > 0 {
        std::process::exit(1);
    }
}

fn check_provider(cli: &Cli) -> Check {
    let provider = cli.provider.as_ref();
    match provider {
        Some(p) if VALID_PROVIDERS.contains(&p.as_str()) => Check {
            name: "provider",
            status: Status::Ok,
            message: format!("provider set to {p}"),
        },
        Some(p) => Check {
            name: "provider",
            status: Status::Error,
            message: format!(
                "unknown provider '{p}' (expected one of: {})",
                VALID_PROVIDERS.join(", ")
            ),
        },
        None => Check {
            name: "provider",
            status: Status::Warn,
            message: format!("no provider configured; will default to '{DEFAULT_PROVIDER}'"),
        },
    }
}

fn check_api_key(cli: &Cli) -> Check {
    check_api_key_with(cli, crate::endpoints::env_non_empty)
}

/// [`check_api_key`], reading variables through `env` (on
/// [`crate::endpoints::env_non_empty`]'s terms), injected so tests need not
/// mutate the process.
fn check_api_key_with(cli: &Cli, env: impl Fn(&str) -> Option<String>) -> Check {
    // Resolve the provider exactly as the runtime does. Reading `cli.provider`
    // raw would report "no API key configured; provider not set" whenever no
    // `--provider` is passed — the common case — even though the run resolves
    // to the default provider and its key works. `provider_defaults` exists to
    // stop precisely this kind of doctor/runtime drift, and the key variable
    // comes from the same endpoint table the runtime reads.
    let provider = resolve_provider_with(cli.provider.as_deref(), &env);

    if cli.api_key.as_ref().is_some_and(|k| !k.trim().is_empty()) {
        return Check {
            name: "api key",
            status: Status::Ok,
            message: "API key is configured".to_string(),
        };
    }

    // A configured `api_key_env` is the only variable consulted.
    if let Some(var) = crate::configured_key_env_with(cli, &provider, &env) {
        return if env(var).is_some() {
            Check {
                name: "api key",
                status: Status::Ok,
                message: format!("using {var} (api_key_env)"),
            }
        } else {
            Check {
                name: "api key",
                status: Status::Error,
                message: format!(
                    "the config declares api_key_env = \"{var}\", but that environment variable \
                     is not set or is empty"
                ),
            }
        };
    }

    let Some(var) = crate::endpoints::api_key_env_var(&provider) else {
        return Check {
            name: "api key",
            status: Status::Warn,
            message: format!("unknown provider '{provider}'; cannot tell which API key it needs"),
        };
    };

    if env(var).is_some() {
        Check {
            name: "api key",
            status: Status::Ok,
            message: format!("using {var}"),
        }
    } else {
        Check {
            name: "api key",
            status: Status::Error,
            message: format!("no API key for {provider}; run `rusta init`, or set {var}"),
        }
    }
}

/// Report the endpoint the runtime will actually call, and the plan it implies.
///
/// This is the setting people get wrong: Kimi answers a Coding Plan key sent to
/// the pay-per-token endpoint `invalid_authentication_error`, which reads like a
/// bad key rather than a bad endpoint, and Z.AI bills an older Coding Plan key
/// per token there (`1113` without a balance). Naming both here turns a
/// confusing refusal into an obvious mismatch.
fn check_endpoint(cli: &Cli) -> Check {
    let provider = resolve_provider(cli.provider.as_deref());
    let billing = cli.billing.unwrap_or_default();
    let region = cli.region.unwrap_or_default();

    let pinned = cli.base_url.as_deref();
    if let Some(url) = pinned {
        return Check {
            name: "endpoint",
            status: Status::Ok,
            message: format!("{url} (pinned)"),
        };
    }

    match crate::endpoints::base_url(&provider, billing, region) {
        // The plan and the region are named where they pick the endpoint:
        // DeepSeek and a plain OpenAI-protocol server have one door.
        Some(url) if !crate::endpoints::billing_matters(&provider) => Check {
            name: "endpoint",
            status: Status::Ok,
            message: url.to_string(),
        },
        Some(url) => {
            let where_ = if crate::endpoints::region_matters(&provider, billing) {
                format!("{}, {}", billing.label(), region.label())
            } else {
                billing.label().to_string()
            };
            Check {
                name: "endpoint",
                status: Status::Ok,
                message: format!("{url}  ({where_})"),
            }
        }
        None => Check {
            name: "endpoint",
            status: Status::Error,
            message: format!("no endpoint for provider {provider}"),
        },
    }
}

/// Which Kimi host accepts the key, and the models it serves.
///
/// The Coding Plan has two hosts, one for kimi.com (mainland) accounts and one
/// for kimi.ai (overseas) accounts, and each refuses the other's keys with an
/// error that reads like a bad key. Asking both turns that into a plain
/// answer; the model list, cached for later runs, shows what the plan serves.
async fn check_kimi(cli: &Cli) -> Option<Check> {
    let provider = resolve_provider(cli.provider.as_deref());
    if provider != "kimi" {
        return None;
    }
    // A missing key is the api-key check's to report.
    let api_key = crate::resolve_api_key(cli, &provider)?;
    let billing = cli.billing.unwrap_or_default();
    let region = cli.region.unwrap_or_default();
    let base_url = crate::resolve_base_url(cli, &provider, billing, region);
    // A pinned URL is where the runtime goes, alternate or not.
    let pinned = crate::endpoints::base_url(&provider, billing, region) != Some(base_url.as_str());
    let alternate = (!pinned)
        .then(|| crate::endpoints::alternate_kimi_coding_host(&base_url))
        .flatten();
    let headers = match crate::custom_headers(cli, &provider) {
        Ok(headers) => headers,
        Err(e) => {
            return Some(Check {
                name: "kimi",
                status: Status::Error,
                message: format!("invalid custom header: {e}"),
            });
        }
    };
    let cache = crate::catalogs::catalog_cache::<KimiModelInfo>();
    Some(kimi_host_check(&base_url, alternate, &api_key, &headers, cache.as_ref()).await)
}

/// Probe `base_url`, then `alternate` if `base_url` refuses the key, and save
/// the model list of the host that accepts it to `cache`.
async fn kimi_host_check(
    base_url: &str,
    alternate: Option<&str>,
    api_key: &str,
    headers: &reqwest::header::HeaderMap,
    cache: Option<&CatalogCache<KimiModelInfo>>,
) -> Check {
    let fail = |status, message| Check {
        name: "kimi",
        status,
        message,
    };
    let (host, models, switched) = match probe(base_url, api_key, headers).await {
        Probe::Accepted(models) => (base_url.to_string(), models, false),
        Probe::Unreachable(e) => {
            return fail(Status::Warn, format!("could not reach {base_url}: {e}"));
        }
        Probe::Rejected => {
            let Some(alternate) = alternate else {
                return fail(Status::Error, format!("{base_url} refused the API key"));
            };
            match probe(alternate, api_key, headers).await {
                Probe::Accepted(models) => (alternate.to_string(), models, true),
                Probe::Rejected => {
                    return fail(
                        Status::Error,
                        format!(
                            "both Kimi Coding hosts refused the API key ({base_url}, {alternate})"
                        ),
                    );
                }
                Probe::Unreachable(e) => {
                    return fail(
                        Status::Error,
                        format!(
                            "{base_url} refused the API key, and {alternate} could not be reached: {e}"
                        ),
                    );
                }
            }
        }
    };

    let mut message = if switched {
        format!(
            "{host} accepts the key; {base_url} refused it. Runs switch over on their own; \
             pin it with `base_url = \"{host}\"`, or re-run `rusta init`"
        )
    } else {
        format!("{host} accepts the key")
    };
    if !models.is_empty() {
        if let Some(cache) = cache
            && let Err(e) = cache.save(&host, &models)
        {
            message.push_str(&format!(" (could not cache the model list: {e})"));
        }
        message.push_str(&format!("; {} models:", models.len()));
        for line in describe_models(&models) {
            message.push_str("\n         ");
            message.push_str(&line);
        }
    }
    Check {
        name: "kimi",
        status: if switched { Status::Warn } else { Status::Ok },
        message,
    }
}

/// Whether the DeepSeek key works against the endpoint the runtime will call,
/// and what the account's balance allows.
///
/// A `GET /models` proves the key and the endpoint in one request that costs
/// no tokens — DeepSeek bills chat completions, not model lists — and brings
/// the model catalogue, which is reported and cached for the next run. A 402
/// here (or on any later request) means the account's balance is spent, which
/// is a top-up, not a retry; `GET /user/balance` says so before a request
/// runs into it.
async fn check_deepseek(cli: &Cli) -> Vec<Check> {
    let provider = resolve_provider(cli.provider.as_deref());
    if provider != "deepseek" {
        return Vec::new();
    }
    // A missing key is the api-key check's to report.
    let Some(api_key) = crate::resolve_api_key(cli, &provider) else {
        return Vec::new();
    };
    let billing = cli.billing.unwrap_or_default();
    let region = cli.region.unwrap_or_default();
    let base_url = crate::resolve_base_url(cli, &provider, billing, region);
    let headers = match crate::custom_headers(cli, &provider) {
        Ok(headers) => headers,
        Err(e) => {
            return vec![Check {
                name: "deepseek",
                status: Status::Error,
                message: format!("invalid custom header: {e}"),
            }];
        }
    };
    let cache = crate::catalogs::catalog_cache::<DeepseekModelInfo>();
    let host = deepseek_host_check(&base_url, &api_key, &headers, cache.as_ref()).await;
    // The balance says more only once the key is known to work.
    let balance = (host.status == Status::Ok)
        .then_some(deepseek_balance_check(&base_url, &api_key, &headers).await);
    std::iter::once(host).chain(balance).collect()
}

/// DeepSeek's `GET /user/balance`: whether the account can make requests,
/// and what is left in each currency.
#[derive(serde::Deserialize)]
struct DeepseekBalance {
    is_available: bool,
    #[serde(default)]
    balance_infos: Vec<DeepseekBalanceInfo>,
}

#[derive(serde::Deserialize)]
struct DeepseekBalanceInfo {
    currency: String,
    total_balance: String,
}

/// Ask for the account's balance, and word it for the report: what is left
/// per currency, and an error when DeepSeek says the account cannot make
/// requests, before the first one runs into a 402.
async fn deepseek_balance_check(
    base_url: &str,
    api_key: &str,
    headers: &reqwest::header::HeaderMap,
) -> Check {
    use crate::probe::{ProbeAnswer, probe};

    let check = |status, message| Check {
        name: "deepseek balance",
        status,
        message,
    };
    let unread = |why: String| check(Status::Warn, format!("could not read the balance: {why}"));
    let body = match probe(base_url, "user/balance", api_key, headers).await {
        Ok(ProbeAnswer::Accepted(body)) => body,
        Ok(ProbeAnswer::Rejected) => return unread("the API key was refused".to_string()),
        Ok(ProbeAnswer::Other(status)) => return unread(format!("HTTP {status}")),
        Err(e) => return unread(e),
    };
    let Ok(balance) = serde_json::from_str::<DeepseekBalance>(&body) else {
        return unread("the response is not the documented balance".to_string());
    };
    let amounts = balance
        .balance_infos
        .iter()
        .map(|info| format!("{} {}", info.total_balance, info.currency))
        .collect::<Vec<_>>()
        .join(", ");
    let amounts = if amounts.is_empty() {
        "no balance listed".to_string()
    } else {
        amounts
    };
    if balance.is_available {
        check(Status::Ok, amounts)
    } else {
        check(
            Status::Error,
            format!(
                "{amounts}: DeepSeek says the account cannot make requests; top it up at \
                 https://platform.deepseek.com"
            ),
        )
    }
}

/// Ask `base_url` for its models with the key, and word the answer for the
/// report: accepted (with what each model can do, saved to `cache`), refused
/// (401/403), out of balance (402), or unreachable.
async fn deepseek_host_check(
    base_url: &str,
    api_key: &str,
    headers: &reqwest::header::HeaderMap,
    cache: Option<&CatalogCache<DeepseekModelInfo>>,
) -> Check {
    use crate::probe::{ProbeAnswer, probe_models};

    let fail = |status, message| Check {
        name: "deepseek",
        status,
        message,
    };
    let answer = match probe_models(base_url, api_key, headers).await {
        Ok(answer) => answer,
        Err(e) => return fail(Status::Warn, format!("could not reach {base_url}: {e}")),
    };
    match answer {
        ProbeAnswer::Rejected => fail(Status::Error, format!("{base_url} refused the API key")),
        ProbeAnswer::Other(reqwest::StatusCode::PAYMENT_REQUIRED) => fail(
            Status::Error,
            format!(
                "{base_url} reports insufficient balance; top up the account at \
                 https://platform.deepseek.com"
            ),
        ),
        ProbeAnswer::Other(status) => fail(
            Status::Warn,
            format!("could not reach {base_url}: HTTP {status}"),
        ),
        ProbeAnswer::Accepted(body) => {
            let models =
                lm_bridge::providers::deepseek_catalog::parse_catalog(&body).unwrap_or_default();
            let mut message = format!("{base_url} accepts the key");
            if !models.is_empty() {
                if let Some(cache) = cache
                    && let Err(e) = cache.save(base_url, &models)
                {
                    message.push_str(&format!(" (could not cache the model list: {e})"));
                }
                message.push_str(&format!("; {} models:", models.len()));
                for line in describe_deepseek_models(&models) {
                    message.push_str("\n         ");
                    message.push_str(&line);
                }
            }
            Check {
                name: "deepseek",
                status: Status::Ok,
                message,
            }
        }
    }
}

/// One line per DeepSeek model: id, name, window, output ceiling, input,
/// effort ladder.
fn describe_deepseek_models(models: &[DeepseekModelInfo]) -> Vec<String> {
    use crate::provider_defaults::format_window;
    models
        .iter()
        .map(|model| {
            let mut line = model.id.clone();
            if let Some(name) = &model.display_name {
                line.push_str(&format!(" ({name})"));
            }
            line.push_str(&format!(
                ", {} context, {} max output",
                format_window(model.context_window),
                format_window(u64::from(model.max_output_tokens))
            ));
            if model.image_in {
                line.push_str(", image input");
            }
            if let Some(ladder) = &model.efforts {
                line.push_str(&format!(", effort {}", ladder.valid.join("/")));
                if let Some(default) = &ladder.default {
                    line.push_str(&format!(" (default {default})"));
                }
            }
            line
        })
        .collect()
}

/// Whether the GLM models this configuration runs are on the Coding Plan.
///
/// The plan documents GLM-5.3 and GLM-5.3-Flash. Other ids are answered by
/// one of those, served without being documented, or refused (`1311`, or
/// `1113` when Z.AI bills them per token), so a pinned model can quietly be
/// a different model, or fail every request.
fn check_glm_plan(cli: &Cli, config: &Config) -> Option<Check> {
    let provider = resolve_provider(cli.provider.as_deref());
    if !matches!(provider.as_str(), "glm" | "zai") {
        return None;
    }
    let billing = cli.billing.unwrap_or_default();
    let region = cli.region.unwrap_or_default();
    let base_url = crate::resolve_base_url(cli, &provider, billing, region);
    let coding_plan = GlmChatProvider::endpoint_is_coding_plan(&base_url)
        .unwrap_or(billing == crate::endpoints::Billing::Subscription);
    if !coding_plan {
        return None;
    }
    let models = crate::vision_mcp::session_models(cli, &provider, &config.subagent);
    Some(glm_plan_check(&models))
}

/// Judge `models` against the Coding Plan's model set.
fn glm_plan_check(models: &[String]) -> Check {
    let use_instead = GlmChatProvider::CODING_PLAN_MODELS.join(" or ");
    let mut status = Status::Ok;
    let mut notes = Vec::new();
    for model in models {
        let (level, note) = match GlmChatProvider::coding_plan_model(model) {
            CodingPlanModel::Documented => continue,
            CodingPlanModel::RoutedTo(served) => (
                Status::Warn,
                format!("{model} is answered by {served}; name {served} to say so"),
            ),
            CodingPlanModel::Undocumented => (
                Status::Warn,
                format!("{model} works but is not documented for the plan, so it may stop working"),
            ),
            CodingPlanModel::NotIncluded => (
                Status::Error,
                format!("the plan does not include {model}; its requests fail (1311 or 1113)"),
            ),
            CodingPlanModel::Unknown => (
                Status::Warn,
                format!("{model} is not a model the plan documents"),
            ),
        };
        if level == Status::Error || status == Status::Ok {
            status = level;
        }
        notes.push(note);
    }
    let message = if notes.is_empty() {
        format!("{} on the GLM Coding Plan", models.join(", "))
    } else {
        format!(
            "{}. The GLM Coding Plan serves {use_instead}",
            notes.join("; ")
        )
    };
    Check {
        name: "glm plan",
        status,
        message,
    }
}

/// Whether the Z.AI vision server can start: `@z_ai/mcp-server` exits at
/// once without `Z_AI_API_KEY`, and needs `Z_AI_MODE` to know which platform
/// issued the key.
fn check_vision_mcp(config: &Config) -> Option<Check> {
    let server = config
        .mcp_servers
        .iter()
        .find(|s| s.name == crate::vision_mcp::SERVER_NAME)?;
    let check = |status, message| {
        Some(Check {
            name: "vision mcp",
            status,
            message,
        })
    };
    if server.disabled {
        return check(Status::Ok, "disabled".to_string());
    }
    if let Some(url) = &server.url {
        return check(Status::Ok, format!("remote server at {url}"));
    }
    // A spawned server sees Rusta's environment plus its own `env`.
    let var = |name: &str| {
        server
            .env
            .get(name)
            .cloned()
            .or_else(|| crate::endpoints::env_non_empty(name))
            .filter(|v| !v.trim().is_empty())
    };
    if var("Z_AI_API_KEY").is_none() {
        return check(
            Status::Error,
            "Z_AI_API_KEY is not set for the server, so it exits at startup".to_string(),
        );
    }
    match var("Z_AI_MODE").as_deref() {
        Some("ZAI" | "ZHIPU") => check(
            Status::Ok,
            format!("{} with Z_AI_API_KEY and Z_AI_MODE set", server.command.as_deref().unwrap_or("server")),
        ),
        Some(other) => check(
            Status::Warn,
            format!("Z_AI_MODE is {other:?}; the server expects ZAI (api.z.ai keys) or ZHIPU (open.bigmodel.cn keys)"),
        ),
        None => check(
            Status::Warn,
            "Z_AI_MODE is not set; the server needs ZAI (api.z.ai keys) or ZHIPU (open.bigmodel.cn keys)".to_string(),
        ),
    }
}

/// Which of the project config's settings apply here. Left-out ones are a
/// warning: a server or rule someone meant to use is silently not there.
fn check_trust(report: &Report) -> Check {
    let file = report.path.display();
    let count = match report.gated.len() {
        1 => "1 setting".to_string(),
        n => format!("{n} settings"),
    };
    let (status, mut message) = match report.outcome {
        Outcome::NotNeeded => (Status::Ok, format!("{file} sets nothing that needs trust")),
        Outcome::Trusted { since } => (
            Status::Ok,
            format!(
                "{file} applies in full: this folder is trusted for it since {} ({count})",
                since.format("%Y-%m-%d")
            ),
        ),
        Outcome::TrustedForRun => (
            Status::Ok,
            format!(
                "{file} applies in full: RUSTA_TRUST_WORKSPACE trusts it for this run ({count})"
            ),
        ),
        Outcome::TrustedOnce => (
            Status::Ok,
            format!("{file} applies in full for this run ({count})"),
        ),
        Outcome::LeftOut { changed_since } => {
            let changed = changed_since.map_or_else(String::new, |since| {
                format!(
                    "; they changed since you trusted this folder on {}",
                    since.format("%Y-%m-%d")
                )
            });
            (
                Status::Warn,
                format!(
                    "{file}: {count} left out until you trust this folder \
                     (`rusta trust`){changed}"
                ),
            )
        }
    };
    if !report.outcome.applies() {
        for summary in &report.gated {
            message.push_str("\n         ");
            message.push_str(summary);
        }
    }
    Check {
        name: "trust",
        status,
        message,
    }
}

fn check_model(cli: &Cli) -> Check {
    let provider = resolve_provider(cli.provider.as_deref());
    let explicit = cli.model.as_deref();
    let billing = cli.billing.unwrap_or_default();

    // Report the model the runtime will actually use, not merely whether one was
    // named on the CLI. A `*_MODEL_NAME` env var supplies the model just as much
    // as `--model` does, and reporting "no model configured" in that case is a lie.
    match resolve_model(&provider, explicit, billing) {
        ModelSource::Explicit(m) => Check {
            name: "model",
            status: Status::Ok,
            message: format!("model set to {m}"),
        },
        ModelSource::Environment { var, model } => Check {
            name: "model",
            status: Status::Ok,
            message: format!("model set to {model} (from {var})"),
        },
        ModelSource::Default(m) => Check {
            name: "model",
            status: Status::Warn,
            message: format!("no model configured; defaulting to {m}"),
        },
        ModelSource::UnknownProvider => Check {
            name: "model",
            status: Status::Error,
            message: format!("cannot resolve a model: unknown provider '{provider}'"),
        },
    }
}

fn check_permission_mode(cli: &Cli, _config: &Config) -> Check {
    if cli.yes {
        return Check {
            name: "permissions",
            status: Status::Warn,
            message: "--yes is set; all permission checks are bypassed".to_string(),
        };
    }

    if let Some(mode) = cli.permission_mode.as_ref() {
        if mode.parse::<PermissionMode>().is_ok() {
            Check {
                name: "permissions",
                status: Status::Ok,
                message: format!("permission mode set to {mode}"),
            }
        } else {
            Check {
                name: "permissions",
                status: Status::Error,
                message: format!(
                    "unknown permission mode '{mode}' (expected 'default', 'accept_edits', or 'bypass_permissions')"
                ),
            }
        }
    } else {
        Check {
            name: "permissions",
            status: Status::Ok,
            message: "using default permission mode".to_string(),
        }
    }
}

/// Where the session's records go, resolved as the run resolves them. Without
/// `--records` or `records` they go to the session's file under the data
/// directory — every session is recorded, which is what `--resume`, `rusta
/// sessions` and `rusta fork` read.
fn check_records(cli: &Cli) -> Check {
    match cli
        .records
        .clone()
        .or_else(|| crate::default_records_path(cli.session.as_deref()))
    {
        Some(path) => Check {
            name: "records",
            status: Status::Ok,
            message: format!("records will be written to {}", path.display()),
        },
        None => Check {
            name: "records",
            status: Status::Warn,
            message: "no data directory, so this session will not be recorded; set --records"
                .to_string(),
        },
    }
}

/// Report whether the Devscriptor code-intelligence server is wired up.
///
/// It is auto-detected rather than configured, so when the binary cannot be
/// found nothing is injected and there is simply no entry for
/// `check_mcp_servers` to report on. A session that silently lost its whole code
/// index then looks identical to a healthy one — which is how a run that made
/// zero Devscriptor calls passed `doctor` with "0 warnings, 0 errors".
fn check_devscriptor(config: &Config) -> Check {
    if let Some(name) = crate::devscriptor::configured_server(&config.mcp_servers) {
        return Check {
            name: "devscriptor",
            status: Status::Ok,
            message: format!("code intelligence available as `{name}`"),
        };
    }

    // An explicit `disabled = true` entry is a choice, not a problem.
    if config
        .mcp_servers
        .iter()
        .any(|s| s.name.to_lowercase().contains("devscriptor"))
    {
        return Check {
            name: "devscriptor",
            status: Status::Ok,
            message: "disabled by configuration".to_string(),
        };
    }

    Check {
        name: "devscriptor",
        status: Status::Warn,
        message: format!(
            "not found — code intelligence is unavailable. Install it, or point \
             {} at the binary.",
            crate::devscriptor::BIN_ENV_VAR
        ),
    }
}

async fn check_mcp_servers(config: &Config) -> Check {
    if config.mcp_servers.is_empty() {
        return Check {
            name: "mcp servers",
            status: Status::Ok,
            message: "no MCP servers configured".to_string(),
        };
    }

    let (manager, _rx) = McpConnectionManager::new(config.mcp_servers.clone(), config.mcp);
    manager.start_all().await;

    // Wait as long as the slowest server is allowed to take to start, capped so
    // `doctor` stays responsive. A fixed short window would leave slow servers
    // (an `npx` cold start, a TLS handshake) stuck in `Pending`, counted as
    // neither connected nor failed, and quietly missing from the summary.
    let budget = config
        .mcp_servers
        .iter()
        .filter(|s| !s.disabled)
        // Resolve each server's effective budget rather than reading the raw
        // field: a server that defers to the global default still has a real
        // startup budget, and waiting less than it would report it as hung.
        .map(|s| s.startup_timeout().as_millis() as u64)
        .max()
        .unwrap_or(DEFAULT_MCP_WAIT_MS)
        .min(MAX_MCP_WAIT_MS);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(budget);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let statuses = manager.statuses().await;
        let all_done = statuses
            .iter()
            .all(|(_, s)| !matches!(s, McpStatus::Pending));
        if all_done || tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    let statuses = manager.statuses().await;
    manager.stop_all();

    let names = |want: fn(&McpStatus) -> bool| -> Vec<&str> {
        statuses
            .iter()
            .filter(|(_, s)| want(s))
            .map(|(n, _)| n.as_str())
            .collect()
    };

    let connected = names(|s| matches!(s, McpStatus::Connected));
    let failed = names(|s| matches!(s, McpStatus::Failed(_)));
    let needs_auth = names(|s| matches!(s, McpStatus::NeedsAuth));
    let pending = names(|s| matches!(s, McpStatus::Pending));

    let mut parts = vec![format!("{} connected", connected.len())];
    if !connected.is_empty() {
        parts.push(format!("({})", connected.join(", ")));
    }
    if !failed.is_empty() {
        parts.push(format!("| {} failed: {}", failed.len(), failed.join(", ")));
    }
    if !needs_auth.is_empty() {
        parts.push(format!(
            "| {} need auth: {}",
            needs_auth.len(),
            needs_auth.join(", ")
        ));
    }
    if !pending.is_empty() {
        parts.push(format!(
            "| {} still pending after {budget}ms: {}",
            pending.len(),
            pending.join(", ")
        ));
    }

    let status = if !failed.is_empty() || !needs_auth.is_empty() || !pending.is_empty() {
        Status::Warn
    } else {
        Status::Ok
    };

    Check {
        name: "mcp servers",
        status,
        message: parts.join(" "),
    }
}

impl Check {
    fn print(&self) {
        // A word, not a symbol. This is the one place the marker carried
        // information rather than decoration, so dropping it outright would
        // lose the per-check status; padded to a fixed width so the check
        // names line up.
        let status = match self.status {
            Status::Ok => "ok   ",
            Status::Warn => "warn ",
            Status::Error => "error",
        };
        println!("{status}  {}: {}", self.name, self.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_cli() -> Cli {
        Cli {
            cwd: std::path::PathBuf::from("."),
            ..Cli::default()
        }
    }

    #[test]
    fn test_check_provider_rejects_unknown() {
        let mut cli = empty_cli();
        cli.provider = Some("unknown".to_string());
        let check = check_provider(&cli);
        assert!(matches!(check.status, Status::Error));
    }

    #[test]
    fn test_check_api_key_prefers_explicit_key() {
        let mut cli = empty_cli();
        cli.provider = Some("kimi".to_string());
        cli.api_key = Some("sk-xxx".to_string());
        let check = check_api_key(&cli);
        assert!(matches!(check.status, Status::Ok));
    }

    /// Settings left out for want of trust are a warning naming each one;
    /// applied, they are fine and not listed.
    #[test]
    fn trust_outcomes_are_reported() {
        let report = |outcome| Report {
            path: std::path::PathBuf::from(".rusta.toml"),
            gated: vec!["MCP server \"helper\" runs `/bin/sh`".to_string()],
            outcome,
        };

        let left_out = check_trust(&report(Outcome::LeftOut {
            changed_since: None,
        }));
        assert_eq!(left_out.status, Status::Warn);
        assert_eq!(
            left_out.message,
            ".rusta.toml: 1 setting left out until you trust this folder (`rusta trust`)\
             \n         MCP server \"helper\" runs `/bin/sh`"
        );

        let trusted = check_trust(&report(Outcome::Trusted {
            since: chrono::Utc::now(),
        }));
        assert_eq!(trusted.status, Status::Ok);
        assert!(
            trusted.message.starts_with(".rusta.toml applies in full"),
            "{}",
            trusted.message
        );
        assert!(!trusted.message.contains("MCP server"));
    }

    #[test]
    fn test_check_provider_accepts_zai() {
        let mut cli = empty_cli();
        cli.provider = Some("zai".to_string());
        let check = check_provider(&cli);
        assert!(matches!(check.status, Status::Ok));
    }

    /// With no `--provider`, the run still resolves to the default provider, so
    /// `doctor` must judge that provider's key rather than reporting "provider
    /// not set" for a session that would work.
    #[test]
    fn api_key_check_resolves_the_default_provider() {
        const VAR: &str = "KIMI_API_KEY";
        let with_key = |var: &str| (var == VAR).then(|| "sk-from-env".to_string());

        let check = check_api_key_with(&empty_cli(), with_key);
        assert!(
            matches!(check.status, Status::Ok),
            "an unset --provider must still resolve to the default: {}",
            check.message
        );
        assert!(check.message.contains(VAR), "{}", check.message);

        let check = check_api_key_with(&empty_cli(), |_| None);
        assert!(
            matches!(check.status, Status::Error),
            "a genuinely missing key is an error, not a vague warning"
        );
        assert!(check.message.contains("rusta init"), "{}", check.message);
    }

    /// A session that silently lost its code index must not look identical to a
    /// healthy one. Devscriptor is auto-detected, so when the binary is missing
    /// there is no `[[mcp_servers]]` entry for the connection check to report —
    /// this is the only place the loss becomes visible.
    #[test]
    fn a_missing_devscriptor_is_a_warning_not_silence() {
        let config = Config::default();
        let check = check_devscriptor(&config);
        assert_eq!(check.status, Status::Warn);
        assert!(
            check.message.contains(crate::devscriptor::BIN_ENV_VAR),
            "the warning should say how to fix it: {}",
            check.message
        );
    }

    #[test]
    fn a_wired_devscriptor_reports_ok() {
        let config = Config {
            mcp_servers: vec![mcp_client::config::McpServerConfig {
                name: "devscriptor".to_string(),
                command: Some("/usr/local/bin/devscriptor".to_string()),
                ..Default::default()
            }],
            ..Config::default()
        };
        assert_eq!(check_devscriptor(&config).status, Status::Ok);
    }

    /// One Kimi Coding host refuses the key and the other takes it: the check
    /// names the host that works, lists and caches its models, and warns so the
    /// user can pin it.
    #[tokio::test]
    async fn the_kimi_check_finds_the_host_that_accepts_the_key() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let refusing = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401).set_body_string(
                r#"{"error":{"type":"invalid_authentication_error","message":"Invalid Authentication"}}"#,
            ))
            .mount(&refusing)
            .await;
        let accepting = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":[{"id":"k3","display_name":"K3","context_length":1048576,"supports_image_in":true,"supports_thinking_type":"only","think_efforts":{"support":true,"valid_efforts":["low","high","max"],"default_effort":"high"}}]}"#,
            ))
            .mount(&accepting)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = CatalogCache::new(dir.path());

        let check = kimi_host_check(
            &refusing.uri(),
            Some(&accepting.uri()),
            "sk-test",
            &reqwest::header::HeaderMap::new(),
            Some(&cache),
        )
        .await;
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check
                .message
                .starts_with(&format!("{} accepts the key", accepting.uri())),
            "{}",
            check.message
        );
        assert!(check.message.contains("base_url = "), "{}", check.message);
        assert!(
            // The entry leaves video out, so k3's built-in video holds.
            check
                .message
                .contains("k3 (K3), 1M context, image and video input"),
            "{}",
            check.message
        );
        assert_eq!(cache.load(&accepting.uri()).unwrap().models[0].id, "k3");

        let none = reqwest::header::HeaderMap::new();
        let check = kimi_host_check(&accepting.uri(), None, "sk-test", &none, None).await;
        assert_eq!(check.status, Status::Ok, "{}", check.message);

        let check = kimi_host_check(&refusing.uri(), None, "sk-test", &none, None).await;
        assert_eq!(check.status, Status::Error, "{}", check.message);

        let check = kimi_host_check(
            &refusing.uri(),
            Some(&refusing.uri()),
            "sk-test",
            &none,
            None,
        )
        .await;
        assert_eq!(check.status, Status::Error);
        assert!(check.message.contains("both"), "{}", check.message);
    }

    /// DeepSeek's `/models` as its API reference documents it.
    const DEEPSEEK_CATALOG: &str = r#"{"object":"list","data":[
        {"id":"deepseek-flash","object":"model","owned_by":"deepseek","name":"DeepSeek-V4.1-Flash","context_window":1048576,"max_output_tokens":393216,"input_modalities":["text","image"],"output_modalities":["text"],"effort":{"supported_levels":["low","high","max"],"default_level":"high"}},
        {"id":"deepseek-v4-pro","object":"model","owned_by":"deepseek","name":"DeepSeek-V4-Pro","context_window":1048576,"max_output_tokens":393216,"input_modalities":["text"],"output_modalities":["text"],"effort":{"supported_levels":["low","high","max"],"default_level":"high"}}
    ]}"#;

    /// The DeepSeek check only runs for the deepseek provider, and its probe
    /// tells an accepted key (with what each model can do, cached for the
    /// next run) from a refused one, a spent balance, and an unreachable
    /// endpoint — without spending a token.
    #[tokio::test]
    async fn the_deepseek_check_probes_models_without_burning_tokens() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let none = reqwest::header::HeaderMap::new();

        let accepting = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DEEPSEEK_CATALOG))
            .mount(&accepting)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = CatalogCache::<DeepseekModelInfo>::new(dir.path());
        let check = deepseek_host_check(&accepting.uri(), "sk-test", &none, Some(&cache)).await;
        assert_eq!(check.status, Status::Ok, "{}", check.message);
        assert!(
            check.message.contains("accepts the key; 2 models:"),
            "{}",
            check.message
        );
        assert!(
            check.message.contains(
                "deepseek-flash (DeepSeek-V4.1-Flash), 1M context, 384K max output, image \
                 input, effort low/high/max (default high)"
            ),
            "{}",
            check.message
        );
        assert!(
            check.message.contains(
                "deepseek-v4-pro (DeepSeek-V4-Pro), 1M context, 384K max output, effort \
                 low/high/max (default high)"
            ),
            "{}",
            check.message
        );
        assert_eq!(
            cache.load(&accepting.uri()).expect("cached").models.len(),
            2,
            "the catalogue is cached for the next run"
        );

        let refused = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401).set_body_string("{}"))
            .mount(&refused)
            .await;
        let check = deepseek_host_check(&refused.uri(), "sk-test", &none, None).await;
        assert_eq!(check.status, Status::Error, "{}", check.message);
        assert!(
            check.message.contains("refused the API key"),
            "{}",
            check.message
        );

        let broke = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(402).set_body_string("{}"))
            .mount(&broke)
            .await;
        let check = deepseek_host_check(&broke.uri(), "sk-test", &none, None).await;
        assert_eq!(check.status, Status::Error, "{}", check.message);
        assert!(
            check.message.contains("insufficient balance"),
            "{}",
            check.message
        );

        let down = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(503).set_body_string("{}"))
            .mount(&down)
            .await;
        let check = deepseek_host_check(&down.uri(), "sk-test", &none, None).await;
        assert_eq!(check.status, Status::Warn, "{}", check.message);
    }

    /// The endpoint line names the plan only where the plan picks the
    /// endpoint: DeepSeek is pay-as-you-go with one endpoint.
    #[test]
    fn the_endpoint_names_a_plan_only_where_the_plan_picks_it() {
        let mut cli = empty_cli();
        cli.provider = Some("deepseek".to_string());
        assert_eq!(check_endpoint(&cli).message, "https://api.deepseek.com");
        cli.provider = Some("kimi".to_string());
        assert_eq!(
            check_endpoint(&cli).message,
            "https://api.kimi.com/coding/v1  (Coding Plan subscription)"
        );
    }

    /// The deepseek check stays out of runs for other providers.
    #[test]
    fn the_deepseek_check_only_runs_for_deepseek() {
        let mut cli = empty_cli();
        cli.provider = Some("kimi".to_string());
        assert!(check_deepseek_blocking(&cli).is_empty());
    }

    /// The balance check reports what is left per currency, fails when
    /// DeepSeek says the account cannot make requests, and warns when the
    /// balance cannot be read.
    #[tokio::test]
    async fn the_deepseek_balance_is_reported() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let none = reqwest::header::HeaderMap::new();
        let balance = |body: &'static str| async move {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/user/balance"))
                .and(header("authorization", "Bearer sk-test"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            server
        };

        let funded = balance(
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"12.34","granted_balance":"0.00","topped_up_balance":"12.34"},{"currency":"CNY","total_balance":"5.00","granted_balance":"5.00","topped_up_balance":"0.00"}]}"#,
        )
        .await;
        let check = deepseek_balance_check(&funded.uri(), "sk-test", &none).await;
        assert_eq!(check.status, Status::Ok, "{}", check.message);
        assert_eq!(check.message, "12.34 USD, 5.00 CNY");

        let spent = balance(
            r#"{"is_available":false,"balance_infos":[{"currency":"USD","total_balance":"0.00","granted_balance":"0.00","topped_up_balance":"0.00"}]}"#,
        )
        .await;
        let check = deepseek_balance_check(&spent.uri(), "sk-test", &none).await;
        assert_eq!(check.status, Status::Error, "{}", check.message);
        assert!(
            check
                .message
                .starts_with("0.00 USD: DeepSeek says the account cannot make requests"),
            "{}",
            check.message
        );

        let odd = balance(r#"{"balance":"unknown"}"#).await;
        let check = deepseek_balance_check(&odd.uri(), "sk-test", &none).await;
        assert_eq!(check.status, Status::Warn, "{}", check.message);

        let down = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/balance"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&down)
            .await;
        let check = deepseek_balance_check(&down.uri(), "sk-test", &none).await;
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("503"), "{}", check.message);
    }

    /// `check_deepseek` is async only because of the probe; the gating logic
    /// is what these assertions exercise.
    fn check_deepseek_blocking(cli: &Cli) -> Vec<Check> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(check_deepseek(cli))
    }

    /// Models the Coding Plan does not document are named, with what happens
    /// to them; one it refuses is an error.
    #[test]
    fn glm_plan_models_are_judged_against_the_plan() {
        let models = |ids: &[&str]| ids.iter().map(ToString::to_string).collect::<Vec<_>>();

        let check = glm_plan_check(&models(&["glm-5.3", "glm-5.3-flash"]));
        assert_eq!(check.status, Status::Ok, "{}", check.message);

        let check = glm_plan_check(&models(&["glm-5.3", "glm-4.7"]));
        assert_eq!(check.status, Status::Warn);
        assert!(
            check
                .message
                .contains("glm-4.7 is answered by glm-5.3-flash"),
            "{}",
            check.message
        );
        assert!(
            check.message.contains("serves glm-5.3 or glm-5.3-flash"),
            "{}",
            check.message
        );

        let check = glm_plan_check(&models(&["glm-4.7", "glm-5v-turbo"]));
        assert_eq!(
            check.status,
            Status::Error,
            "a refused model outranks a routed one"
        );
        assert!(
            check.message.contains("does not include glm-5v-turbo"),
            "{}",
            check.message
        );

        let check = glm_plan_check(&models(&["glm-5v-turbo", "glm-4.6v"]));
        assert_eq!(
            check.status,
            Status::Error,
            "a later warning does not lower an error"
        );
    }

    fn vision_server(env: &[(&str, &str)]) -> Config {
        Config {
            mcp_servers: vec![mcp_client::config::McpServerConfig {
                name: crate::vision_mcp::SERVER_NAME.to_string(),
                command: Some("npx".to_string()),
                env: env
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
                ..Default::default()
            }],
            ..Config::default()
        }
    }

    /// The vision server needs its key and its platform; the check says which
    /// is missing. The variables are set in the server's own `env` so the
    /// ambient environment cannot decide the outcome.
    #[test]
    fn the_vision_server_environment_is_checked() {
        assert!(check_vision_mcp(&Config::default()).is_none());

        let complete = vision_server(&[("Z_AI_API_KEY", "key"), ("Z_AI_MODE", "ZAI")]);
        assert_eq!(check_vision_mcp(&complete).unwrap().status, Status::Ok);

        let wrong_mode = vision_server(&[("Z_AI_API_KEY", "key"), ("Z_AI_MODE", "zai.ai")]);
        assert_eq!(check_vision_mcp(&wrong_mode).unwrap().status, Status::Warn);

        let mut disabled = vision_server(&[]);
        disabled.mcp_servers[0].disabled = true;
        assert_eq!(check_vision_mcp(&disabled).unwrap().status, Status::Ok);
    }

    /// Opting out deliberately is not a problem to report.
    #[test]
    fn a_disabled_devscriptor_is_not_a_warning() {
        let config = Config {
            mcp_servers: vec![mcp_client::config::McpServerConfig {
                name: "devscriptor".to_string(),
                disabled: true,
                ..Default::default()
            }],
            ..Config::default()
        };
        let check = check_devscriptor(&config);
        assert_eq!(check.status, Status::Ok);
        assert!(check.message.contains("disabled"), "{}", check.message);
    }
}
