use std::io;
use std::sync::Arc;

use agent_core::tools::builtin::KimiWebSearchTool;
use agent_core::{Agent, Llm, PermissionMode, ProfileRegistry, SkillRegistry};
use approval::StdinApprovalHook;
use user_interaction::StdinUserInteraction;

use clap::{CommandFactory, Parser};
use clap_complete::generate;
use cli::Cli;
use lm_bridge::providers::catalog_cache::CatalogEntry;
use lm_bridge::providers::deepseek_catalog::DeepseekModelInfo;
use lm_bridge::providers::kimi_catalog::KimiModelInfo;
use lm_bridge::providers::{
    DeepseekChatProvider, GlmChatProvider, KimiChatProvider, OpenAiCompatProvider,
};
use lm_bridge::tool::Tool;
use mcp_client::manager::{McpConnectionManager, McpEvent, McpStatus};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use lm_bridge::providers::openai_compat::HttpTimeouts;
use tracing::{info, warn};

mod approval;
mod catalogs;
mod cli;
mod commands;
mod config;
mod devscriptor;
mod doctor;
mod endpoints;
mod export;
mod hooks;
mod interactive;
mod kimi;
mod native_web;
mod onboarding;
mod probe;
mod project_context;
mod provider_defaults;
mod reload;
mod session_store;
mod session_title;
mod sessions;
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

    let mut cli = Cli::parse();

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
    agent.set_subagent_mcp_guidance(mcp_guidance);
    // A script driving Rusta follows sub-agents' work too.
    if cli.stream_json {
        agent.set_subagent_tool_event_sender(stream::json_subagent_tool_events());
    }
    let skill_registry = Arc::new(load_skill_registry(&cli.cwd, &config.skill_dirs)?);
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
                } => {
                    // A large server's tools load on demand (`select_tools`); a
                    // small one's sit in the tool table like built-ins.
                    let mut agent = agent_for_mcp.lock().await;
                    for tool in tools {
                        let name = tool.tool_definition().name.clone();
                        if deferred {
                            agent.register_dynamic_tool(tool);
                        } else {
                            agent.register_tool(tool);
                        }
                        info!("[mcp:{server}] registered tool {name} (deferred: {deferred})");
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

/// A headless prompt, then the background work it left, as the headless
/// background mode says. Returns the exit code: 1 when a turn failed or the
/// prompt ran out of steps, 0 otherwise.
async fn run_headless(
    cli: &Cli,
    ctx: &commands::CommandContext,
    prompt: String,
    cancel: &CancellationToken,
) -> i32 {
    let code = headless_turns(cli, ctx, prompt, cancel).await;
    // Let go of the printer, which ends its last line as it goes; an exit
    // would leave it unfinished.
    ctx.agent
        .lock()
        .await
        .set_tool_event_sender(stream::silent_tool_events());
    code
}

/// The turns of [`run_headless`], which returns their exit code.
async fn headless_turns(
    cli: &Cli,
    ctx: &commands::CommandContext,
    prompt: String,
    cancel: &CancellationToken,
) -> i32 {
    let mode = cli
        .headless_background_mode
        .or(ctx.config.headless_background_mode)
        .unwrap_or_default();
    let ceiling = cli
        .headless_wait_ceiling_s
        .or(ctx.config.headless_wait_ceiling_s)
        .map(std::time::Duration::from_secs);
    let max_follow_ups = cli
        .headless_max_turns
        .or(ctx.config.headless_max_turns)
        .unwrap_or(DEFAULT_HEADLESS_MAX_TURNS);
    let (callbacks, tool_event_sender) = if cli.stream_json {
        stream::json_callbacks()
    } else {
        stream::headless_callbacks()
    };
    let llm = ctx.llm.clone();
    {
        let mut agent = ctx.agent.lock().await;
        agent.set_tool_event_sender(tool_event_sender);
        // Headless has no next prompt in which to collect a sub-agent's
        // results, so a steering run holds the turn for them rather than
        // exit and kill them along with the process.
        agent.set_drain_agent_tasks_on_stop(mode == cli::HeadlessBackgroundMode::Steer);
        match agent
            .prompt_with_callbacks(llm.as_ref(), prompt, callbacks, cancel)
            .await
        {
            Ok(result) => {
                if cli.stream_json {
                    stream::emit_response_id(result.response_id.as_deref());
                    stream::emit_usage(&result.usage);
                }
                if let Some(reason) = result.stop_reason {
                    stream::report_stop(reason, cli.stream_json);
                    // A run that ran out of steps did not finish its task,
                    // so it fails after the handoff.
                    if reason == agent_core::StopReason::MaxTurns {
                        if !cli.stream_json {
                            eprintln!(
                                "Error: Max turns exceeded. Raise --max-turns (or max_turns) \
                                 for longer tasks."
                            );
                        }
                        return 1;
                    }
                }
            }
            Err(e) => {
                stream::report_turn_error(&e, cli.stream_json);
                if matches!(e, agent_core::errors::AgentError::SessionOccupied(_)) {
                    return SESSION_OCCUPIED_EXIT_CODE;
                }
                return 1;
            }
        }
    }

    // Keep the headless run alive while a goal is active or cron tasks are
    // pending, reacting to each cron fire / background completion as it arrives
    // instead of exiting and dropping that work. A plain one-shot `-p` creates
    // none of these, so this loop exits at once; a run that scheduled cron work
    // stays alive like a small daemon until it settles or is interrupted.
    match mode {
        cli::HeadlessBackgroundMode::Exit => 0,
        cli::HeadlessBackgroundMode::Drain => {
            drain_headless_agents(ctx, ceiling, cli.stream_json, cancel).await;
            0
        }
        cli::HeadlessBackgroundMode::Steer => {
            drive_headless_keepalive(
                ctx,
                llm.as_ref(),
                HeadlessLimits {
                    ceiling,
                    max_follow_ups,
                },
                cli.stream_json,
                cancel,
            )
            .await
        }
    }
}

/// Follow-up turns a steering headless run takes by default, high enough to
/// be no limit for real work.
const DEFAULT_HEADLESS_MAX_TURNS: u32 = 100_000;

/// How long a headless run may keep going after its prompt.
struct HeadlessLimits {
    /// Wall-clock limit on waiting and steering; `None` for none.
    ceiling: Option<std::time::Duration>,
    /// Follow-up turns the steering loop may take.
    max_follow_ups: u32,
}

/// `drain`: wait until no sub-agent is running — without another model turn —
/// or the ceiling passes, or the run is cancelled. Background shell commands
/// are not waited for; a server or a watcher is meant to outlive the run.
async fn drain_headless_agents(
    ctx: &commands::CommandContext,
    ceiling: Option<std::time::Duration>,
    stream_json: bool,
    cancel: &CancellationToken,
) {
    let deadline = ceiling.map(|c| tokio::time::Instant::now() + c);
    loop {
        let active = ctx
            .agent
            .lock()
            .await
            .tool_ctx()
            .background
            .active_agent_task_count();
        if active == 0 {
            return;
        }
        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            stream::report_headless_stop(
                "headless_wait_ceiling",
                "stopped waiting: the headless wait ceiling passed with sub-agents still running",
                stream_json,
            );
            return;
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
        }
    }
}

/// `steer`: keep a headless `-p` run alive while a goal is active or cron
/// tasks are pending, driving one continuation turn per batch of cron-fire /
/// background notifications as they arrive, within
/// `limits`. Returns as soon as nothing is outstanding, when a limit is
/// reached, or when the run is cancelled. A one-shot `-p` has no such work,
/// so this returns immediately.
///
/// Returns the exit code: 1 when a continuation turn failed, which is
/// reported and ends the run, 0 otherwise.
async fn drive_headless_keepalive(
    ctx: &commands::CommandContext,
    llm: &dyn agent_core::agent_loop::llm::Llm,
    limits: HeadlessLimits,
    stream_json: bool,
    cancel: &CancellationToken,
) -> i32 {
    let deadline = limits.ceiling.map(|c| tokio::time::Instant::now() + c);
    let mut follow_ups = 0u32;
    const CONTINUE_NUDGE: &str = "(Scheduled or background activity has occurred; the notifications above describe it. \
Continue the work it implies, then stop when nothing remains.)";

    loop {
        if cancel.is_cancelled() {
            break;
        }
        // Due scheduled prompts become pending notifications here; the branch
        // below then drives a continuation turn that receives them.
        ctx.agent.lock().await.poll_cron();
        let (outstanding, notifications) = {
            let agent = ctx.agent.lock().await;
            (
                agent.has_outstanding_headless_work(),
                agent.pending_notifications(),
            )
        };
        if !outstanding {
            break;
        }
        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            stream::report_headless_stop(
                "headless_wait_ceiling",
                "stopped: the headless wait ceiling passed with background work pending",
                stream_json,
            );
            break;
        }
        if notifications > 0 {
            if follow_ups >= limits.max_follow_ups {
                stream::report_headless_stop(
                    "headless_max_turns",
                    "stopped: the headless follow-up turn cap was reached with background work \
                     pending",
                    stream_json,
                );
                break;
            }
            follow_ups += 1;
            // Each turn prints through a printer of its own, its text and its
            // tool events alike; the one it replaces ends its last line.
            let (callbacks, tool_event_sender) = if stream_json {
                stream::json_callbacks()
            } else {
                stream::headless_callbacks()
            };
            let mut agent = ctx.agent.lock().await;
            agent.set_tool_event_sender(tool_event_sender);
            match agent
                .prompt_from(
                    agent_core::PromptOrigin::Continuation,
                    llm,
                    CONTINUE_NUDGE,
                    callbacks,
                    cancel,
                )
                .await
            {
                Ok(result) => {
                    if stream_json {
                        stream::emit_usage(&result.usage);
                    }
                    if let Some(reason) = result.stop_reason {
                        stream::report_stop(reason, stream_json);
                    }
                }
                Err(e) => {
                    stream::report_turn_error(&e, stream_json);
                    return 1;
                }
            }
        } else {
            // No notification is ready yet: wait briefly for the cron scheduler
            // to fire or a background task to finish, or for cancellation.
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            }
        }
    }
    0
}

/// Load the config layers, the project's through `gate`, which decides what
/// of it this folder is trusted for.
fn load_config(cli: &Cli, gate: &mut trust::Gate) -> config::Config {
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
        Ok(config) => config,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(2);
        }
    }
}

/// How this run settles project settings that need trust: an interactive
/// session asks, a headless run (or one without a terminal to ask at) warns,
/// and a command that runs no session says nothing — `doctor` reports it.
fn trust_mode(cli: &Cli) -> trust::Mode {
    use std::io::IsTerminal;
    let session = match &cli.command {
        None => true,
        #[cfg(feature = "tui")]
        Some(cli::Commands::Tui) => true,
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

fn apply_config(cli: &mut Cli, config: &config::Config) {
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
fn refresh_catalog<T: CatalogEntry>(cli: &Cli, provider: &str) {
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
fn requires_api_key(
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
fn custom_headers_with<F>(
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
fn resolve_api_key_with<F>(cli: &Cli, provider: &str, env: F) -> Option<String>
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

/// Load user and project profiles into a registry.
pub(crate) fn load_profile_registry(cwd: &std::path::Path) -> anyhow::Result<ProfileRegistry> {
    let mut dirs = Vec::new();
    if let Some(config_dir) = agent_core::user_dirs::config_dir() {
        dirs.push(config_dir.join("rusta").join("profiles"));
    }
    dirs.push(cwd.join(".rusta").join("profiles"));
    ProfileRegistry::load_dirs(&dirs)
        .map_err(|e| anyhow::anyhow!("failed to load profile registry: {e}"))
}

/// Load user, shared, and project skills into a registry.
pub(crate) fn load_skill_registry(
    cwd: &std::path::Path,
    extra_dirs: &[std::path::PathBuf],
) -> anyhow::Result<SkillRegistry> {
    let mut dirs = Vec::new();
    if let Some(config_dir) = agent_core::user_dirs::config_dir() {
        dirs.push(config_dir.join("rusta").join("skills"));
    }
    if let Some(agents_dir) = dirs::home_dir() {
        dirs.push(agents_dir.join(".agents").join("skills"));
    }
    dirs.push(cwd.join(".rusta").join("skills"));
    for dir in extra_dirs {
        dirs.push(config::expand_tilde(dir));
    }
    SkillRegistry::load_dirs(&dirs)
        .map_err(|e| anyhow::anyhow!("failed to load skill registry: {e}"))
}

#[cfg(test)]
mod headless_tests {
    use agent_core::agent_loop::llm::{Llm, LlmCallbacks};
    use agent_core::agent_loop::types::StepResult;
    use agent_core::cron::CronFire;
    use lm_bridge::errors::LlmError;
    use lm_bridge::message::Message;
    use lm_bridge::tool::Tool;
    use tokio_util::sync::CancellationToken;

    use super::{HeadlessLimits, drive_headless_keepalive};

    /// A model that answers every request with `done`.
    struct AnsweringLlm;

    #[async_trait::async_trait]
    impl Llm for AnsweringLlm {
        fn model_name(&self) -> &str {
            "answering"
        }

        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, LlmError> {
            Ok(StepResult {
                message: Message::assistant_text("done"),
                usage: lm_bridge::usage::TokenUsage::empty(),
                finish_reason: Some(lm_bridge::provider::FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    /// A model whose every request fails with a 400, which is never retried.
    struct RejectingLlm;

    #[async_trait::async_trait]
    impl Llm for RejectingLlm {
        fn model_name(&self) -> &str {
            "rejecting"
        }

        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, LlmError> {
            Err(LlmError::ApiStatus {
                status: reqwest::StatusCode::BAD_REQUEST,
                message: "invalid request".to_string(),
                retry_hint: false,
                retry_after: None,
            })
        }
    }

    fn due_fire() -> CronFire {
        CronFire {
            job_id: "0123abcd".to_string(),
            cron: "*/5 * * * *".to_string(),
            recurring: true,
            coalesced_count: 1,
            stale: false,
            prompt: "check the queue".to_string(),
        }
    }

    /// A continuation turn is a system trigger to the turn hooks: TurnStarted
    /// says so, and UserPromptSubmit, which is for the user's input, does not
    /// fire.
    #[tokio::test]
    async fn a_follow_up_turn_is_reported_as_a_system_trigger() {
        use agent_core::user_hooks::{UserHookEvent, UserHookRule, UserHooks};
        let dir = tempfile::tempdir().unwrap();
        let rule = |event, log: &str| UserHookRule {
            event,
            matcher: None,
            command: format!("cat >> '{}'", dir.path().join(log).display()),
            timeout: Some(5),
        };
        let hooks = UserHooks::new(
            vec![
                rule(UserHookEvent::UserPromptSubmit, "submitted"),
                rule(UserHookEvent::TurnStarted, "started"),
            ],
            "session_test",
            "Hooks test",
            "rusta_cli",
            dir.path().to_path_buf(),
        )
        .unwrap();
        let mut ctx = crate::commands::CommandContext::for_test(dir.path()).await;
        ctx.agent = std::sync::Arc::new(tokio::sync::Mutex::new(
            agent_core::Agent::new(dir.path().to_path_buf(), "sys")
                .with_user_hooks(Some(std::sync::Arc::new(hooks))),
        ));
        ctx.agent
            .lock()
            .await
            .tool_ctx()
            .cron
            .queue_fires(vec![due_fire()]);
        let limits = HeadlessLimits {
            ceiling: None,
            max_follow_ups: 5,
        };

        let code =
            drive_headless_keepalive(&ctx, &AnsweringLlm, limits, true, &CancellationToken::new())
                .await;

        assert_eq!(code, 0);
        assert!(!dir.path().join("submitted").exists());
        let started: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("started")).unwrap())
                .unwrap();
        assert_eq!(started["origin_kind"], "system_trigger");
        assert_eq!(started["origin_name"], "headless_continuation");
    }

    /// A continuation turn that fails ends the headless run with a failure,
    /// instead of being dropped without a word.
    #[tokio::test]
    async fn a_failed_follow_up_turn_fails_the_headless_run() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::commands::CommandContext::for_test(dir.path()).await;
        ctx.agent
            .lock()
            .await
            .tool_ctx()
            .cron
            .queue_fires(vec![due_fire()]);
        let limits = HeadlessLimits {
            ceiling: None,
            max_follow_ups: 5,
        };

        let code =
            drive_headless_keepalive(&ctx, &RejectingLlm, limits, true, &CancellationToken::new())
                .await;

        assert_eq!(code, 1);
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

#[cfg(test)]
mod thinking_keep_tests {
    use super::{Cli, requires_api_key, resolve_api_key, resolve_base_url, resolve_thinking_keep};
    use crate::endpoints::{Billing, Region};
    use crate::provider_defaults;
    use clap::Parser;

    /// `register_builtin_tools` clears exactly the names in `PROVIDER_BUILTIN_TOOLS`
    /// before registering the new provider's. A builtin missing from that list
    /// would survive a `/provider` switch and be sent to a vendor that does not
    /// accept it — Moonshot's `$web_search` goes out as a `builtin_function`, and
    /// Z.AI answers `tools[N].type: type is illegal` to *every* later request.
    #[test]
    fn every_provider_builtin_is_listed_for_removal() {
        let timeout = super::HttpTimeouts::idle(std::time::Duration::from_secs(1));
        let providers: Vec<Box<dyn lm_bridge::provider::ChatProvider>> = vec![
            Box::new(
                super::KimiChatProvider::new(
                    "kimi-for-coding".to_string(),
                    Some("k".to_string()),
                    "https://api.kimi.com/coding/v1".to_string(),
                    timeout,
                )
                .unwrap(),
            ),
            Box::new(
                super::GlmChatProvider::new(
                    // The default for `--provider glm`, so the test covers what
                    // a user actually gets.
                    "glm-5.3".to_string(),
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
                    super::PROVIDER_BUILTIN_TOOLS.contains(&tool.name.as_str()),
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
        super::apply_config(&mut cli, &config);
        assert_eq!(cli.cache_ttl(), Some(PromptCacheTtl::FiveMinutes));

        let mut cli = Cli::parse_from(["rusta", "--prompt-cache-ttl", "1h"]);
        super::apply_config(&mut cli, &config);
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
        super::apply_config(&mut cli, &config);

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
        super::apply_config(&mut cli, &config);
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
            super::resolve_api_key_with(&cli, "kimi", named).as_deref(),
            Some("sk-named")
        );
        let kimi_key = |var: &str| (var == "KIMI_API_KEY").then(|| "sk-kimi".to_string());
        assert_eq!(super::resolve_api_key_with(&cli, "kimi", kimi_key), None);

        // Nothing sets the named variable in this process.
        let timeouts = super::resolve_timeouts(&cli);
        let err = super::build_provider_for(&cli, None, None, timeouts)
            .err()
            .expect("a missing named variable is an error");
        assert!(err.to_string().contains(NAMED), "{err}");

        // It applies to the configured provider only.
        assert_eq!(super::configured_key_env(&cli, "glm"), None);
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
        let headers = super::custom_headers_with(&cli, "glm", env).unwrap();
        assert_eq!(headers["x-route"], "from-config");
        assert_eq!(headers["x-trace"], "on");
        let other = super::custom_headers_with(&cli, "kimi", |_| None).unwrap();
        assert!(
            other.get("x-route").is_none(),
            "config headers are the provider's"
        );

        cli.custom_headers.insert("Bad Name".into(), "x".into());
        assert!(super::custom_headers_with(&cli, "glm", |_| None).is_err());
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
        super::apply_config(&mut cli, &config);

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
