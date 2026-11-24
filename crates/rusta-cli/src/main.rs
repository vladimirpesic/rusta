use std::io;
use std::sync::Arc;

use agent_core::tools::builtin::KimiWebSearchTool;
use agent_core::{Agent, Llm, PermissionMode};
use approval::StdinApprovalHook;
use headless::run_headless;
use provider_setup::{build_provider, refresh_catalog, resolve_timeouts};
use registries::{load_profile_registry, load_skill_registry};
use settings::{apply_config, load_config, trust_mode};
use user_interaction::StdinUserInteraction;

use clap::CommandFactory;
use clap_complete::generate;
use cli::Cli;
use lm_bridge::providers::deepseek_catalog::DeepseekModelInfo;
use lm_bridge::providers::kimi_catalog::KimiModelInfo;
use lm_bridge::tool::Tool;
use mcp_client::manager::{McpConnectionManager, McpEvent, McpStatus};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use tracing::{info, warn};

mod approval;
mod catalogs;
mod cli;
mod commands;
mod config;
mod context_view;
mod devscriptor;
mod doctor;
mod endpoints;
mod export;
mod headless;
mod hooks;
mod interactive;
mod kimi;
mod native_web;
mod onboarding;
mod probe;
mod project_context;
mod provider_defaults;
mod provider_setup;
mod registries;
mod reload;
mod session_store;
mod session_title;
mod sessions;
mod settings;
mod stream;
mod subagent_pool;
mod system_prompt;
mod terminal_sanitize;
mod trust;
#[cfg(feature = "tui")]
mod tui;
mod user_interaction;
mod vision_fallback;
mod vision_mcp;
mod zai_mcp;
mod zai_reader;
mod zai_search;

/// `rmcp` logs `unexpected content type` at ERROR *before* returning the error to
/// its caller. [`mcp_client::lenient_http`] handles the benign case — a server
/// acknowledging a notification with a bodyless `200` — so the line is pure noise
/// on a healthy connection. Genuine failures still surface: the transport quits,
/// the server lands in `McpStatus::Failed`, and `doctor` names it.
const RMCP_HTTP_NOISE: &str = "rmcp::transport::common::reqwest::streamable_http_client=off";

/// Build the tracing filter directives.
///
/// `EnvFilter::from_default_env()` defaults to ERROR-only when `RUST_LOG` is
/// unset. Preserve that, minus the noisy rmcp target. An explicit `RUST_LOG` is
/// honoured verbatim, so the noise can still be inspected when debugging.
fn tracing_filter_directives(rust_log: Option<&str>) -> String {
    match rust_log {
        Some(v) if !v.trim().is_empty() => v.to_string(),
        _ => format!("error,{RMCP_HTTP_NOISE}"),
    }
}

/// Ctrl+C, or on Unix the SIGTERM a supervisor sends — `timeout`, `kill`, a
/// container or CI job being stopped. Either cancels the turn, and a cancelled
/// turn records what it did before the process exits, so the session can be
/// resumed, the step in progress included.
///
/// Returns `true` for Ctrl+C (SIGINT) — a cancel the user asked for, which
/// the interruption reminder follows — and `false` for SIGTERM, which gets no
/// reminder.
async fn interrupt_signal() -> bool {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(mut sigint), Ok(mut sigterm)) => {
                tokio::select! {
                    _ = sigint.recv() => return true,
                    _ = sigterm.recv() => return false,
                }
            }
            _ => warn!("cannot listen for SIGINT/SIGTERM; only Ctrl+C will stop the run"),
        }
    }
    tokio::signal::ctrl_c().await.ok();
    true
}

/// Install the tracing subscriber.
///
/// Logs go to **stderr**. stdout carries the assistant's answer and, under
/// `--stream-json`, a strict one-JSON-object-per-line stream; a single log line
/// there makes that stream unparseable.
fn init_tracing() {
    let rust_log = std::env::var("RUST_LOG").ok();
    let filter = tracing_subscriber::EnvFilter::new(tracing_filter_directives(rust_log.as_deref()));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    // Before anything is started: no process of Rusta's reads its keys from
    // Rusta's own environment.
    agent_core::child_env::close_own_environment();

    let mut cli = Cli::parse_warning_of_key();

    if matches!(cli.command, Some(cli::Commands::Version)) {
        println!("rusta {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if let Some(cli::Commands::Completions { shell }) = &cli.command {
        let mut cmd = Cli::command();
        generate(*shell, &mut cmd, "rusta", &mut io::stdout());
        return Ok(());
    }

    cli.cwd = session_dir(&cli.cwd)?;

    // Trust is managed from the project config as written, before any of it
    // is applied.
    if let Some(cli::Commands::Trust { revoke, list, yes }) = &cli.command {
        return trust::run(cli.config.as_deref(), *revoke, *list, *yes);
    }

    let mut trust_gate = trust::Gate::new(
        trust_mode(&cli),
        cli.trust_workspace,
        trust::TrustStore::default_path(),
    );
    let mut config = load_config(&cli, &mut trust_gate);

    // Dispatched *before* the config is merged into `cli`, so the wizard can tell
    // "--provider glm, asked for right now" from "kimi, chosen on a previous run".
    // Merging first makes those indistinguishable, and `rusta init` would then
    // silently refuse to ever switch you to another provider.
    if matches!(cli.command, Some(cli::Commands::Init)) {
        onboarding::run(&cli, &config).await?;
        return Ok(());
    }

    apply_config(&mut cli, &config);
    // Minted once, before anything is cloned out of `cli`, so every derived
    // setting — the prompt cache key, the spill directory, each pooled
    // sub-agent's key — agrees on it.
    cli.assign_run_id();

    // Saved sessions are managed without a provider or a key.
    match &cli.command {
        Some(cli::Commands::Sessions {
            action:
                cli::SessionsAction::List {
                    all,
                    cwd,
                    limit,
                    json,
                },
        }) => {
            let options = sessions::ListOptions {
                all: *all,
                dir: cwd.clone().unwrap_or_else(|| cli.cwd.clone()),
                limit: *limit,
                json: *json,
            };
            sessions::list(records_dir(&cli), &options).await?;
            return Ok(());
        }
        Some(cli::Commands::Fork {
            session,
            cwd,
            id,
            yes,
        }) => {
            let options = sessions::ForkOptions {
                source: session.clone(),
                dir: cwd.clone(),
                id: id.clone(),
                yes: *yes,
            };
            sessions::fork(records_dir(&cli), options).await?;
            return Ok(());
        }
        _ => {}
    }

    // Nothing to run with: no key anywhere and a human at the keyboard. Offer to
    // set one up rather than dying with "missing API key" and leaving them to
    // work out which of the two endpoints their key belongs to.
    //
    // This has to happen before the servers below are injected: they are keyed on
    // the provider and API key, so running it later would configure this session
    // against the key the user *didn't* have.
    if onboarding::should_offer(&cli) {
        onboarding::run(&cli, &config).await?;
        config = load_config(&cli, &mut trust_gate);
        apply_config(&mut cli, &config);
    }

    // Auto-configure the Devscriptor code-intelligence server when it is installed
    // and the user has not configured (or explicitly disabled) it themselves. Runs
    // before `doctor` so the diagnostic reports it, and before the system prompt is
    // built so the Devscriptor guidance is included.
    // Resolve the global MCP timeouts before anything reads them: the injectors
    // below need them, and every user-configured server that left its own unset
    // is filled from them.
    config.apply_mcp_env_overrides();
    config.apply_mcp_timeout_defaults();

    devscriptor::normalize_server_cwds(&mut config.mcp_servers, &cli.cwd);

    if let Some(command) =
        devscriptor::inject_devscriptor_server(&mut config.mcp_servers, &cli.cwd, &config.mcp)
    {
        info!(
            "[mcp:{}] auto-detected: {command}",
            devscriptor::SERVER_NAME
        );
    }
    // Devscriptor's most-used tools sit in the tool table from the first
    // request, whether Rusta configured the server or the user did; a model
    // that must load a tool first mostly searches with `grep` instead.
    devscriptor::preload_main_tools(&mut config.mcp_servers);

    // Z.AI's web-search/reader/vision servers are injected for GLM Coding Plan
    // keys. This must happen before `doctor` runs: otherwise the diagnostic
    // reports on a server set that no real session ever uses, and silently
    // misses failures.
    let provider_for_mcp = cli
        .provider
        .clone()
        .or_else(|| endpoints::env_non_empty("RUSTA_PROVIDER"))
        .unwrap_or_else(|| "kimi".to_string());
    let api_key_for_mcp = cli
        .api_key
        .clone()
        .or_else(|| endpoints::env_non_empty("RUSTA_ZAI_API_KEY"))
        .or_else(|| endpoints::env_non_empty("ZAI_API_KEY"))
        .or_else(|| endpoints::env_non_empty("RUSTA_API_KEY"));
    zai_mcp::inject_zai_mcp_servers(
        &provider_for_mcp,
        cli.billing.unwrap_or_default(),
        api_key_for_mcp.as_deref(),
        cli.region.unwrap_or_default(),
        &mut config.mcp_servers,
        &config.mcp,
    );
    // The GLM models this session may run: the main one and the sub-agent
    // pool's. The Vision MCP server is started only if one of them needs it.
    let session_models = vision_mcp::session_models(&cli, &provider_for_mcp, &config.subagent);
    vision_mcp::inject_zai_vision_server(
        &provider_for_mcp,
        cli.billing.unwrap_or_default(),
        api_key_for_mcp.as_deref(),
        cli.region.unwrap_or_default(),
        &session_models,
        &mut config.mcp_servers,
        &config.mcp,
    );

    if matches!(cli.command, Some(cli::Commands::Doctor)) {
        doctor::run(&cli, &config, trust_gate.report()).await;
        return Ok(());
    }

    let profile_registry = Arc::new(load_profile_registry(&cli.cwd)?);
    // The template, a `--profile`'s role, the Devscriptor workflow when its
    // server is configured, and the project context.
    let (system_prompt, subagent_prompt, mcp_guidance, instruction_files) =
        match system_prompt::build(&cli, &config, &profile_registry) {
            Ok(built) => {
                for warning in &built.warnings {
                    eprintln!("warning: {warning}");
                }
                (
                    built.text,
                    built.subagent_prompt,
                    built.mcp_guidance,
                    built.instruction_files,
                )
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };

    let timeouts = resolve_timeouts(&cli);
    let provider = build_provider(&cli, timeouts)?;
    // Keep the capability catalogue current for the next provider built.
    match provider.name() {
        "kimi" => refresh_catalog::<KimiModelInfo>(&cli, "kimi"),
        "deepseek" => refresh_catalog::<DeepseekModelInfo>(&cli, "deepseek"),
        _ => {}
    }
    let builtin_tools = provider.builtin_tools();
    let llm_options = commands::LlmOptions::from_cli(&cli);
    let llm = Arc::new(llm_options.build_llm(provider));

    let mut agent = Agent::new_with_budget(cli.cwd.clone(), &system_prompt, llm.context_window());
    agent = agent.with_llm(llm.clone());
    agent = agent.with_profile_registry(profile_registry.clone());
    // Sub-agents get the session's prompt with their role, and one that gets
    // the session's MCP tools gets the guidance for them.
    agent.set_subagent_prompt(Some(subagent_prompt));
    // The session's first prompt comes with a reminder of where to start with
    // the MCP tools: the guidance alone sits deep in the system prompt.
    agent.set_tools_reminder(mcp_guidance.as_ref().map(|g| g.reminder.clone()));
    agent.set_subagent_mcp_guidance(mcp_guidance);
    // A script driving Rusta follows sub-agents' work too.
    if cli.stream_json {
        agent.set_subagent_tool_event_sender(stream::json_subagent_tool_events());
    }
    let skill_registry = Arc::new(load_skill_registry(
        &cli.cwd,
        &cli.additional_dirs,
        &config.skill_dirs,
    )?);
    agent = agent.with_skill_registry(skill_registry.clone());
    if let Some(max) = cli
        .background_max_running_tasks
        .or(config.background_max_running_tasks)
    {
        agent = agent.with_background_max_tasks(max);
    }
    // `--max-turns` / `RUSTA_MAX_TURNS` win over the config file; the Agent's
    // built-in default (100) applies when neither is set.
    // Fails fast: a pool that cannot resolve is better found before the first
    // prompt than three turns into a delegation, when nobody is watching.
    match subagent_pool::CliModelPool::new(&config.subagent, &cli, timeouts) {
        Ok(Some(pool)) => {
            use agent_core::model_pool::ModelPool;
            let aliases: Vec<&str> = pool.entries().iter().map(|e| e.alias.as_str()).collect();
            info!(
                "subagent models: {} (default: {})",
                aliases.join(", "),
                pool.default_alias().unwrap_or("primary")
            );
            agent = agent.with_model_pool(Some(Arc::new(pool)), llm.model_name());
        }
        Ok(None) => {}
        Err(e) => anyhow::bail!("invalid [subagent] config: {e}"),
    }

    if let Some(depth) = cli.max_subagent_depth.or(config.max_subagent_depth) {
        agent = agent.with_max_subagent_depth(depth);
    }
    if let Some(max_turns) = cli.max_turns.or(config.max_turns) {
        agent = agent.with_max_turns(max_turns);
    }
    if let Some(max) = cli.max_attempts_per_step.or(config.max_attempts_per_step) {
        agent = agent.with_max_attempts_per_step(max);
    }
    agent = agent.with_infinite_retry(cli.infinite_retry);
    if let Some(ms) = cli.subagent_timeout_ms.or(config.subagent_timeout_ms) {
        // Zero is the documented way to remove the deadline entirely, which is a
        // real thing to want for an overnight run — so it is not clamped away.
        agent = agent.with_subagent_timeout((ms > 0).then(|| std::time::Duration::from_millis(ms)));
    }
    if let Some(ms) = cli.swarm_timeout_ms.or(config.swarm_timeout_ms) {
        agent = agent.with_swarm_timeout((ms > 0).then(|| std::time::Duration::from_millis(ms)));
    }
    let swarm_max_concurrency = cli
        .swarm_max_concurrency
        .and_then(|n| usize::try_from(n).ok())
        .or(config.swarm_max_concurrency);
    agent = agent.with_swarm_max_concurrency(swarm_max_concurrency);
    agent = agent.with_repeat_breaker(cli.repeat_breaker.or(config.repeat_breaker).unwrap_or(true));
    agent = agent.with_dangerous_command_guard(
        cli.dangerous_command_guard
            .or(config.dangerous_command_guard)
            .unwrap_or(true),
    );
    agent = agent.with_tool_output_policy(tool_output_policy(&cli, &config));
    agent = agent.with_compaction_max_attempts(
        cli.compaction_max_attempts
            .or(config.compaction_max_attempts)
            .unwrap_or(0),
    );
    register_builtin_tools(&mut agent, &builtin_tools);
    for path in &cli.allowed_paths {
        agent = agent.with_allowed_path(path.clone());
    }
    agent = agent.with_plan_mode(cli.plan);
    if cli.swarm {
        agent.set_swarm_mode(Some(agent_core::swarm::SwarmTrigger::Manual));
    }
    let mode = if cli.yes {
        PermissionMode::BypassPermissions
    } else if let Some(mode_str) = &cli.permission_mode {
        match mode_str.parse::<PermissionMode>() {
            Ok(mode) => mode,
            Err(_) => {
                eprintln!(
                    "Unknown permission mode '{mode_str}' (expected 'default', 'accept_edits', or 'bypass_permissions')"
                );
                std::process::exit(1);
            }
        }
    } else {
        PermissionMode::Default
    };
    agent = agent.with_permission_mode(mode);
    // An interactive session asks in every mode — bypass still asks to review
    // a plan and before a guardrailed write — and a headless run unless it
    // bypasses permissions, where nobody is there to answer. `--stream-json`
    // keeps stdout to JSON events, so it asks with events.
    if cli.can_ask() {
        let hook = if cli.stream_json {
            StdinApprovalHook::json()
        } else {
            StdinApprovalHook::new()
        };
        agent = agent.with_approval_hook(std::sync::Arc::new(hook));
    }
    if !cli.is_headless() || !cli.yes {
        let interaction = if cli.stream_json {
            StdinUserInteraction::json()
        } else {
            StdinUserInteraction::new()
        };
        agent = agent.with_user_interaction(std::sync::Arc::new(interaction));
    }
    if cli.plan && !cli.can_ask() {
        eprintln!(
            "Plan mode is on, but nobody can review a plan in a headless run that bypasses \
             permissions: the plan cannot be approved, and plan mode stays on to the end."
        );
    }
    if let Some(ref permissions) = config.permissions {
        agent = agent.with_permission_rules(permissions.rules.clone());
    }
    if let Some(ref search_url) = cli.search_url {
        agent = agent.with_search_url(search_url.clone());
    }

    let records_path = cli
        .records
        .clone()
        .or_else(|| default_records_path(cli.session.as_deref()));
    let records_dir = records_dir(&cli);

    if let Some(records_path) = records_path {
        let records = agent_core::records::Records::new(config::expand_tilde(records_path));
        if cli.resume {
            agent = agent
                .with_resumed_records(records)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
        } else {
            agent = agent.with_records(records);
        }
        // One process writes the session at a time.
        let locks = agent_core::session_lock::SessionLocks::new(session_client(&cli));
        agent = agent.with_session_lock(Arc::new(locks), session_id(&cli));
    }

    let session_store = Arc::new(Mutex::new(
        session_store::SessionStore::new(records_dir).await,
    ));
    // The title the session has on record, which the hooks' payloads carry.
    let session_title = session_store
        .lock()
        .await
        .get(&session_id(&cli))
        .map(|entry| entry.display_title)
        .unwrap_or_default();
    agent.set_session_title(session_title.clone());

    // User hooks ([[hooks]] in the user config): built once for the session
    // and handed to the agent, which shares them with the tool context and
    // the background manager. A project file's hooks never got this far —
    // the trust layer refuses them at load time.
    let user_hooks = hooks::build(
        config.hooks.clone(),
        &cli.cache_key().unwrap_or_default(),
        agent.session_title(),
        &agent.tool_ctx().cwd,
    )?;
    if let Some(hooks) = &user_hooks {
        agent = agent.with_user_hooks(Some(Arc::clone(hooks)));
    }

    if let Some(cli::Commands::Export { session, output }) = &cli.command {
        let store = session_store.lock().await;
        let path = export::run_export(session, output, &store).await?;
        println!("Exported session '{}' to {}", session, path.display());
        return Ok(());
    }

    let agent = Arc::new(Mutex::new(agent));
    let ctx_config = config.clone();

    // The session events bracket the run: SessionStart now, SessionEnd once
    // the session ends (see `run_session`). The heartbeat timer exists only
    // while a rule listens for it.
    if let Some(hooks) = &user_hooks {
        let source = if cli.resume { "resume" } else { "startup" };
        hooks::session_start(hooks, source, llm.model_name(), cli.profile.as_deref()).await;
        if hooks.has_event(agent_core::user_hooks::UserHookEvent::SessionHeartbeat) {
            hooks::spawn_heartbeat(Arc::clone(hooks));
        }
    }

    // Start the MCP connection manager and wire its events into the agent registry.
    let (mcp_manager, mut mcp_events) =
        McpConnectionManager::new(config.mcp_servers.clone(), config.mcp);
    for (name, part) in mcp_manager.renamed_servers() {
        eprintln!(
            "warning: MCP server \"{name}\" has characters model APIs refuse in tool names; \
             its tools are offered as mcp__{part}__<tool>"
        );
    }
    if let Some(fallback) =
        vision_fallback::for_provider(&cli, llm.provider_name(), &config, &mcp_manager, timeouts)
    {
        agent.lock().await.set_vision_fallback(fallback);
    }
    // The provider's own web search and page reader (Z.AI's, or the Kimi
    // Coding Plan's).
    {
        let web = native_web::clients(&cli, &provider_for_mcp);
        let mut agent = agent.lock().await;
        if let Some(client) = web.search {
            agent.set_web_search_client(client);
        }
        if let Some(client) = web.reader {
            agent.set_web_reader_client(client);
        }
    }
    let agent_for_mcp = agent.clone();
    tokio::spawn(async move {
        while let Some(event) = mcp_events.recv().await {
            match event {
                McpEvent::Status {
                    server,
                    status,
                    message,
                } => {
                    let msg = message.as_deref().unwrap_or("");
                    match status {
                        McpStatus::Connected => info!("[mcp:{server}] connected"),
                        McpStatus::Failed(_) => warn!("[mcp:{server}] failed: {msg}"),
                        McpStatus::NeedsAuth => {
                            warn!("[mcp:{server}] needs authentication")
                        }
                        McpStatus::Pending => info!("[mcp:{server}] connecting"),
                        McpStatus::Disabled => info!("[mcp:{server}] disabled"),
                    }
                }
                McpEvent::ToolsAdded {
                    server,
                    tools,
                    deferred,
                    preloaded,
                } => {
                    // A large server's tools load on demand (`select_tools`),
                    // but for the ones it preloads; a small one's sit in the
                    // tool table like built-ins.
                    let mut agent = agent_for_mcp.lock().await;
                    for tool in tools {
                        let name = tool.tool_definition().name.clone();
                        let on_demand = deferred && !preloaded.contains(&name);
                        if on_demand {
                            agent.register_dynamic_tool(tool);
                        } else {
                            agent.register_tool(tool);
                        }
                        info!("[mcp:{server}] registered tool {name} (deferred: {on_demand})");
                    }
                }
                McpEvent::ToolsRemoved { server, tools } => {
                    let mut agent = agent_for_mcp.lock().await;
                    for name in tools {
                        agent.unregister_tool(&name);
                        info!("[mcp:{server}] removed tool {name}");
                    }
                }
                McpEvent::AuthRequired { server, tool } => {
                    // Always in the tool table: a rejected call tells the model
                    // to call it, so it must not first have to be loaded.
                    let mut agent = agent_for_mcp.lock().await;
                    let name = tool.tool_definition().name.clone();
                    agent.register_tool(tool);
                    info!("[mcp:{server}] registered auth tool {name}");
                }
                McpEvent::OAuthUrl { server, url } => {
                    println!(
                        "MCP server '{server}' needs authorization. Open this URL in your browser:\n{url}"
                    );
                }
            }
        }
    });
    mcp_manager.start_all().await;
    wait_for_mcp_tools(&mcp_manager, &agent, &config.mcp_servers).await;

    let mut ctx = commands::CommandContext {
        agent,
        llm,
        cli: cli.clone(),
        config: ctx_config,
        timeouts,
        mcp_manager,
        profile_registry,
        skill_registry,
        instruction_files,
        llm_options,
        session_store,
        session_title: Arc::new(session_title::SessionTitle::new(session_title)),
        model_switch: commands::ModelSwitch::default(),
    };

    let user_cancel = ctx.agent.lock().await.user_cancel_flag();
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        if interrupt_signal().await {
            // Ctrl+C is the user's own cancel: the turn it cuts short is
            // followed by the interruption reminder. SIGTERM is not, and
            // neither is a limit or a programmatic abort.
            user_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        cancel_clone.cancel();
    });

    // SessionEnd closes every session SessionStart opened, however it ends:
    // a failed, cut-short or interrupted run is where a notification hook
    // matters most.
    let outcome = run_session(&cli, &mut ctx, cancel).await;
    hooks::session_end(user_hooks.as_ref(), hooks::ctrl_c_again()).await;
    match outcome? {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

/// The session SessionStart opened: a headless prompt, the TUI, or the REPL.
/// Returns the process's exit code. Nothing in it exits the process, so the
/// caller always runs SessionEnd after it.
async fn run_session(
    cli: &Cli,
    ctx: &mut commands::CommandContext,
    cancel: CancellationToken,
) -> anyhow::Result<i32> {
    let headless_prompt = match cli.resolve_prompt() {
        Ok(prompt) => prompt,
        Err(e) => {
            eprintln!("{e}");
            return Ok(2);
        }
    };
    if let Some(prompt) = headless_prompt {
        info!("Running headless prompt");
        let code = run_headless(cli, ctx, prompt, &cancel).await;
        // A failed run is a session too, with records worth listing.
        record_session_activity(ctx, None).await;
        return Ok(code);
    }

    #[cfg(feature = "tui")]
    if matches!(cli.command, Some(cli::Commands::Tui)) {
        tui::run(ctx.clone(), cancel).await?;
        return Ok(0);
    }

    interactive::run_repl(ctx, cancel, cli.stream_json).await?;
    Ok(0)
}

/// Where session records and the session index live: beside `--records`, or
/// `<data dir>/rusta/records`.
fn records_dir(cli: &Cli) -> std::path::PathBuf {
    cli.records
        .as_ref()
        .and_then(|p| p.parent().map(std::path::PathBuf::from))
        .or_else(|| {
            default_records_path(cli.session.as_deref())
                .and_then(|p| p.parent().map(std::path::PathBuf::from))
        })
        .unwrap_or_else(|| {
            agent_core::user_dirs::data_dir()
                .map(|d| d.join("rusta").join("records"))
                .unwrap_or_else(|| cli.cwd.join(".rusta").join("records"))
        })
}

pub(crate) fn default_records_path(session: Option<&str>) -> Option<std::path::PathBuf> {
    let id = session.unwrap_or("default");
    agent_core::user_dirs::data_dir()
        .map(|d| d.join("rusta").join("records").join(format!("{id}.jsonl")))
}

/// `--cwd` as the session's directory: its tools run there, and its config,
/// project context, skills and profiles are read from there. It is made
/// absolute once, against Rusta's own directory, so that they all agree.
/// The tools once ran in Rusta's own directory whatever `--cwd` said.
fn session_dir(cwd: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let dir =
        std::fs::canonicalize(cwd).map_err(|e| anyhow::anyhow!("--cwd {}: {e}", cwd.display()))?;
    if !dir.is_dir() {
        anyhow::bail!("--cwd {}: not a directory", cwd.display());
    }
    Ok(dir)
}

/// What this process is to a session's lock: how it runs.
fn session_client(cli: &Cli) -> &'static str {
    if cli.prompt.is_some() || cli.prompt_file.is_some() {
        return "rusta headless";
    }
    #[cfg(feature = "tui")]
    if matches!(cli.command, Some(cli::Commands::Tui)) {
        return "rusta tui";
    }
    "rusta repl"
}

/// The exit code of a headless run refused a session another process is
/// writing.
const SESSION_OCCUPIED_EXIT_CODE: i32 = 4;

/// The session's id: `--session`, or `default`.
pub(crate) fn session_id(cli: &Cli) -> String {
    cli.session.clone().unwrap_or_else(|| "default".to_string())
}

/// Record the session's activity in the index; `title` is a new title (from
/// `/title`), and `None` keeps the one on record.
async fn record_session_activity(ctx: &commands::CommandContext, title: Option<String>) {
    let session_id = session_id(&ctx.cli);
    let records_path = ctx
        .cli
        .records
        .clone()
        .or_else(|| default_records_path(Some(&session_id)))
        .unwrap_or_else(|| {
            agent_core::user_dirs::data_dir()
                .map(|d| {
                    d.join("rusta")
                        .join("records")
                        .join(format!("{session_id}.jsonl"))
                })
                .unwrap_or_else(|| {
                    ctx.cli
                        .cwd
                        .join(".rusta")
                        .join("records")
                        .join(format!("{session_id}.jsonl"))
                })
        });
    let provider = ctx
        .cli
        .provider
        .clone()
        .unwrap_or_else(|| "kimi".to_string());
    let model = ctx.llm.model_name().to_string();
    let _ = ctx
        .session_store
        .lock()
        .await
        .record_activity(session_store::Activity {
            id: session_id,
            display_title: title,
            records_path,
            provider,
            model,
            cwd: std::fs::canonicalize(&ctx.cli.cwd).unwrap_or_else(|_| ctx.cli.cwd.clone()),
        })
        .await;
}

/// Every provider-supplied builtin Rusta knows how to execute.
///
/// These are *server-side* tools, and each vendor only accepts its own: Moonshot
/// takes `$web_search` as a `builtin_function`, a form Z.AI rejects outright with
/// `tools[N].type: type is illegal`. So the set is not additive — it must be
/// replaced wholesale whenever the provider changes, or a `/provider` switch
/// leaves the old vendor's tool in the list and every later request 400s.
const PROVIDER_BUILTIN_TOOLS: &[&str] = &["$web_search"];

/// Make `agent`'s builtin tools exactly the ones `builtins` names.
fn register_builtin_tools(agent: &mut Agent, builtins: &[Tool]) {
    for name in PROVIDER_BUILTIN_TOOLS {
        agent.unregister_tool(name);
    }
    for tool in builtins {
        if tool.name == "$web_search" {
            agent.register_tool(Arc::new(KimiWebSearchTool::new()));
        }
    }
}

/// How tool results are bounded for this run: the configured budget, and a
/// spill directory named after the run under the data directory. Spill
/// directories from runs older than a week are removed on the way.
fn tool_output_policy(
    cli: &Cli,
    config: &config::Config,
) -> agent_core::tool_output::ToolOutputPolicy {
    use agent_core::tool_output::{
        DEFAULT_IMAGE_READ_BYTE_BUDGET, DEFAULT_READ_MAX_CHARS, DEFAULT_TOOL_OUTPUT_MAX_CHARS,
        SPILL_MAX_AGE, ToolOutputPolicy, prune_spill_root,
    };

    let max_chars = cli
        .tool_output_max_chars
        .or(config.tool_output_max_chars)
        .unwrap_or(DEFAULT_TOOL_OUTPUT_MAX_CHARS);
    let read_max_chars = cli
        .read_max_chars
        .or(config.read_max_chars)
        .unwrap_or(DEFAULT_READ_MAX_CHARS);
    let image_byte_budget = cli
        .image_read_byte_budget
        .or(config.image_read_byte_budget)
        .unwrap_or(DEFAULT_IMAGE_READ_BYTE_BUDGET);
    let root = agent_core::user_dirs::data_dir()
        .map(|d| d.join("rusta").join("spill"))
        .unwrap_or_else(|| std::env::temp_dir().join("rusta-spill"));
    prune_spill_root(&root, SPILL_MAX_AGE);
    let run = cli.run_id.clone().unwrap_or_else(|| "default".to_string());
    let dir_name: String = run
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    ToolOutputPolicy::new(max_chars, root.join(dir_name))
        .with_read_max_chars(read_max_chars)
        .with_image_byte_budget(image_byte_budget)
}

/// Block until every enabled MCP server has settled and its tools are registered
/// with the agent.
///
/// `McpConnectionManager` sets a server to `Connected` *before* emitting
/// `ToolsAdded`, and that event is consumed on a background task. Without this
/// wait, a headless `-p` run reaches its first turn with an empty dynamic-tool
/// set: the model sees neither the `<tools_added>` announcement nor the
/// `select_tools` tool (which is only offered when dynamic tools exist), and
/// reports the MCP tools as unavailable.
///
/// Bounded by the largest configured startup timeout so a hung or failing server
/// cannot block the agent indefinitely.
async fn wait_for_mcp_tools(
    manager: &McpConnectionManager,
    agent: &Arc<Mutex<Agent>>,
    configs: &[mcp_client::McpServerConfig],
) {
    let enabled: Vec<&mcp_client::McpServerConfig> =
        configs.iter().filter(|c| !c.disabled).collect();
    if enabled.is_empty() {
        return;
    }

    let budget = enabled
        .iter()
        .map(|c| c.startup_timeout())
        .max()
        .unwrap_or_else(|| std::time::Duration::from_secs(30))
        + std::time::Duration::from_secs(5);
    let deadline = tokio::time::Instant::now() + budget;

    loop {
        let statuses = manager.statuses().await;
        let settled = statuses
            .iter()
            .all(|(_, status)| !matches!(status, McpStatus::Pending));

        if settled {
            let registered = agent.lock().await.list_tools();
            let mut all_ready = true;
            for (name, status) in &statuses {
                if !matches!(status, McpStatus::Connected) {
                    continue;
                }
                // Discovery completes before a server reports `Connected`, so an
                // empty tool list here means the server genuinely exposes none.
                let discovered = match manager.get(name) {
                    Some(handle) => handle.tool_names().await,
                    None => continue,
                };
                if discovered.is_empty() {
                    continue;
                }
                let prefix = format!("mcp__{name}__");
                if !registered.iter().any(|t| t.starts_with(&prefix)) {
                    all_ready = false;
                    break;
                }
            }
            if all_ready {
                return;
            }
        }

        if tokio::time::Instant::now() >= deadline {
            warn!(
                "MCP servers did not finish registering tools within {budget:?}; continuing without them"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tracing_filter_tests {
    use super::{RMCP_HTTP_NOISE, tracing_filter_directives};

    #[test]
    fn default_filter_silences_the_handled_rmcp_error() {
        let d = tracing_filter_directives(None);
        assert!(d.starts_with("error,"), "default must stay ERROR-only: {d}");
        assert!(d.contains(RMCP_HTTP_NOISE), "rmcp noise not silenced: {d}");
        // The directive must switch that target off, not merely mention it.
        assert!(d.contains("streamable_http_client=off"));
    }

    #[test]
    fn blank_rust_log_falls_back_to_the_default() {
        for blank in [Some(""), Some("   "), None] {
            assert_eq!(
                tracing_filter_directives(blank),
                tracing_filter_directives(None)
            );
        }
    }

    #[test]
    fn explicit_rust_log_is_honoured_verbatim() {
        // So a developer can re-enable the silenced target when debugging.
        assert_eq!(tracing_filter_directives(Some("debug")), "debug");
        assert_eq!(
            tracing_filter_directives(Some("rmcp=trace,rusta=debug")),
            "rmcp=trace,rusta=debug"
        );
    }

    #[test]
    fn default_directives_parse_as_an_env_filter() {
        // Guards against a typo in the target path silently disabling nothing.
        let filter = tracing_subscriber::EnvFilter::try_new(tracing_filter_directives(None));
        assert!(filter.is_ok(), "default directives must parse");
    }
}
