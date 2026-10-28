//! Shared slash-command dispatcher for the REPL and TUI.

use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{
    Agent, ExecutableTool, GoalStatus, PermissionMode, ProfileRegistry, ProviderLlm, SkillRegistry,
    tools::builtin::{SkillTool, WebSearchTool},
};
use lm_bridge::provider::ThinkingConfig;
use lm_bridge::providers::kimi_switch_rules::{ModelSwitchRule, ModelSwitchRules, SessionKind};

use mcp_client::manager::McpConnectionManager;
use serde_json::json;
use tokio::sync::Mutex;

use crate::{
    cli::Cli,
    config::{Config, expand_tilde},
    reload::{ReloadChanges, Snapshot},
    session_store::SessionStore,
    session_title::SessionTitle,
};

/// Options that must be reapplied when rebuilding a `ProviderLlm` at runtime.
#[derive(Debug, Clone, Default)]
pub struct LlmOptions {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub thinking: Option<ThinkingConfig>,
    pub prompt_cache_key: Option<String>,
    pub reasoning_effort: Option<String>,
    pub thinking_keep: Option<bool>,
    pub max_tokens: Option<u32>,
    pub prompt_cache_ttl: Option<lm_bridge::provider::PromptCacheTtl>,
}

impl LlmOptions {
    /// The sampling settings a `Cli` describes.
    #[must_use]
    pub fn from_cli(cli: &Cli) -> Self {
        Self {
            temperature: cli.temperature,
            top_p: cli.top_p,
            thinking: cli.thinking.then_some(ThinkingConfig::EnabledExplicit),
            // Always keyed: Moonshot routes requests that share a
            // `prompt_cache_key` to the same cache, and a run without
            // `--session` is still one conversation whose prefix repeats on
            // every step.
            prompt_cache_key: cli.cache_key(),
            reasoning_effort: cli.reasoning_effort.clone(),
            thinking_keep: cli.thinking_keep,
            max_tokens: cli.max_tokens,
            prompt_cache_ttl: cli.cache_ttl(),
        }
    }

    /// Wrap `provider` in a `ProviderLlm` carrying these settings.
    ///
    /// One place, because there are three: session start, `/model` and
    /// `/provider`, and each sub-agent bound to a pool entry. Copied per site,
    /// a setting added to one would be silently missing from the others.
    #[must_use]
    pub fn build_llm(&self, provider: Box<dyn lm_bridge::provider::ChatProvider>) -> ProviderLlm {
        let mut llm = ProviderLlm::new(provider);
        if let Some(t) = self.temperature {
            llm = llm.with_temperature(t);
        }
        if let Some(p) = self.top_p {
            llm = llm.with_top_p(p);
        }
        if let Some(thinking) = &self.thinking {
            llm = llm.with_thinking(thinking.clone());
        }
        if let Some(key) = &self.prompt_cache_key {
            llm = llm.with_prompt_cache_key(key.clone());
        }
        if let Some(effort) = &self.reasoning_effort {
            llm = llm.with_reasoning_effort(effort.clone());
        }
        if let Some(keep) = self.thinking_keep {
            llm = llm.with_thinking_keep(keep);
        }
        if let Some(max) = self.max_tokens {
            llm = llm.with_max_tokens(max);
        }
        if let Some(ttl) = self.prompt_cache_ttl {
            llm = llm.with_prompt_cache_ttl(ttl);
        }
        llm
    }
}

/// Mutable state shared between the UI and the command dispatcher.
#[derive(Clone)]
pub struct CommandContext {
    pub agent: Arc<Mutex<Agent>>,
    /// Shared LLM handle; commands that switch model/provider replace this Arc.
    pub llm: Arc<ProviderLlm>,
    pub cli: Cli,
    pub config: Config,
    pub timeouts: lm_bridge::providers::openai_compat::HttpTimeouts,
    pub mcp_manager: McpConnectionManager,
    pub profile_registry: Arc<ProfileRegistry>,
    pub skill_registry: Arc<SkillRegistry>,
    /// The instruction files the system prompt was built from, with their
    /// text: what `/reload` compares to say which changed.
    pub instruction_files: Vec<(PathBuf, String)>,
    pub llm_options: LlmOptions,
    pub session_store: Arc<Mutex<SessionStore>>,
    /// The session's title in this run, shown by the TUI, and the attempts
    /// at making one for an untitled session.
    pub session_title: Arc<SessionTitle>,
    pub model_switch: ModelSwitch,
}

/// What `/model` keeps between invocations: Kimi's rules for confirming a
/// switch, read once per run, and a switch waiting for its confirmation.
#[derive(Clone, Default)]
pub struct ModelSwitch {
    rules: Arc<tokio::sync::OnceCell<Option<ModelSwitchRules>>>,
    pending: Option<String>,
}

#[cfg(test)]
impl CommandContext {
    /// A context for unit tests: an agent working in `dir`, a provider aimed
    /// at a closed local port (a request fails fast instead of reaching a
    /// vendor), no MCP servers, and a session store under `dir`.
    pub(crate) async fn for_test(dir: &std::path::Path) -> Self {
        use clap::Parser;
        use lm_bridge::providers::openai_compat::{HttpTimeouts, OpenAiCompatProvider};

        let timeouts = HttpTimeouts::idle(std::time::Duration::from_secs(5));
        let provider =
            OpenAiCompatProvider::new("openai", "test-model", "http://127.0.0.1:9", None, timeouts)
                .expect("a provider for a local address");
        Self {
            agent: Arc::new(Mutex::new(Agent::new(dir.to_path_buf(), "sys"))),
            llm: Arc::new(ProviderLlm::new(Box::new(provider))),
            cli: Cli::parse_from(["rusta"]),
            config: Config::default(),
            timeouts,
            // No servers, so the event receiver has nothing to deliver.
            mcp_manager: McpConnectionManager::new(Vec::new(), Default::default()).0,
            profile_registry: Arc::new(ProfileRegistry::with_builtin_profiles()),
            skill_registry: Arc::new(SkillRegistry::new()),
            instruction_files: Vec::new(),
            llm_options: LlmOptions::default(),
            session_store: Arc::new(Mutex::new(SessionStore::new(dir.join("sessions")).await)),
            session_title: Arc::new(SessionTitle::default()),
            model_switch: ModelSwitch::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Yolo,
    Auto,
    Permission(PermissionMode),
    Model(String),
    /// Bare `/model`: list the models the provider knows.
    Models,
    Provider(String),
    /// `/provider refresh [<name>]`: the session's provider, or `name`.
    ProviderRefresh(Option<String>),
    Sessions(Option<String>),
    Tasks,
    Mcp,
    GoalCreate(String),
    GoalPause,
    GoalResume,
    GoalCancel,
    /// `/swarm`: swarm mode on, off, toggled, or for one task.
    Swarm(SwarmCommand),
    AddDir(PathBuf),
    Reload,
    Title(String),
    /// `/takeover`: take the session over from a process that holds it.
    TakeOver,
    Web(String),
    New,
    /// `/compact`: with an argument, the focus instruction the summarizer
    /// is asked to honor.
    Compact(Option<String>),
    Clear,
    /// `/undo`: `None` is the bare command (one turn, as today); `Some(n)`
    /// is `/undo <count>`.
    Undo(Option<usize>),
    Tools,
    Plan,
    Skill {
        name: String,
        args: serde_json::Map<String, serde_json::Value>,
    },
    Exit,
    Help,
    Unknown(String),
}

/// What `/swarm` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwarmCommand {
    /// `/swarm`: on if it is off, off if it is on.
    Toggle,
    /// `/swarm on`.
    On,
    /// `/swarm off`.
    Off,
    /// `/swarm <task>`: on for this task, which is then sent.
    Task(String),
}

/// The `/undo` count: the whole argument must be a positive integer — no sign,
/// no leading zero, nothing else.
fn parse_undo_count(args: &str) -> Option<usize> {
    let value = args.trim();
    if value.is_empty() || value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

impl Command {
    /// A prompt to send once the command has run: `/swarm <task>`'s task.
    #[must_use]
    pub fn follow_up_prompt(&self) -> Option<String> {
        match self {
            Self::Swarm(SwarmCommand::Task(task)) => Some(task.clone()),
            _ => None,
        }
    }

    /// Parse a slash-command line.
    pub fn parse(line: &str) -> Self {
        let trimmed = line.trim();
        if !trimmed.starts_with('/') {
            return Self::Unknown(trimmed.to_string());
        }
        let mut parts = trimmed.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();

        match cmd {
            "/exit" | "/quit" => Self::Exit,
            "/yolo" => Self::Yolo,
            "/auto" => Self::Auto,
            "/permission" => {
                let mode = rest.first().copied().unwrap_or("default");
                match PermissionMode::parse(mode) {
                    Some(m) => Self::Permission(m),
                    None => Self::Unknown(format!("unknown permission mode: {mode}")),
                }
            }
            "/model" => match rest.first() {
                Some(m) => Self::Model(m.to_string()),
                None => Self::Models,
            },
            "/provider" => match rest.as_slice() {
                ["refresh"] => Self::ProviderRefresh(None),
                ["refresh", name] => Self::ProviderRefresh(Some(name.to_string())),
                [name] => Self::Provider(name.to_string()),
                _ => Self::Unknown(PROVIDER_USAGE.to_string()),
            },
            "/sessions" => Self::Sessions(rest.first().map(|s| s.to_string())),
            "/tasks" => Self::Tasks,
            "/mcp" => Self::Mcp,
            "/goal" => match rest.first().copied() {
                Some("create") => Self::GoalCreate(rest.get(1..).unwrap_or(&[]).join(" ")),
                Some("pause") => Self::GoalPause,
                Some("resume") => Self::GoalResume,
                Some("cancel") => Self::GoalCancel,
                _ => Self::Unknown(
                    "/goal usage: create <objective> | pause | resume | cancel".to_string(),
                ),
            },
            "/swarm" => Self::Swarm(match rest.as_slice() {
                [] => SwarmCommand::Toggle,
                ["on"] => SwarmCommand::On,
                ["off"] => SwarmCommand::Off,
                task => SwarmCommand::Task(task.join(" ")),
            }),
            "/add-dir" => match rest.first() {
                Some(p) => Self::AddDir(PathBuf::from(p)),
                None => Self::Unknown("/add-dir requires a path".to_string()),
            },
            "/reload" => Self::Reload,
            "/title" => Self::Title(rest.join(" ")),
            "/takeover" => Self::TakeOver,
            "/web" => Self::Web(rest.join(" ")),
            "/new" => Self::New,
            "/compact" => {
                let instruction = rest.join(" ");
                Self::Compact((!instruction.is_empty()).then_some(instruction))
            }
            "/clear" => Self::Clear,
            "/undo" => {
                if rest.is_empty() {
                    Self::Undo(None)
                } else {
                    match parse_undo_count(&rest.join(" ")) {
                        Some(count) => Self::Undo(Some(count)),
                        None => Self::Unknown(
                            "usage: /undo [count], where count is a positive integer".to_string(),
                        ),
                    }
                }
            }
            "/tools" => Self::Tools,
            "/plan" => Self::Plan,
            "/skill" => {
                if rest.is_empty() {
                    return Self::Unknown("/skill requires a skill name".to_string());
                }
                let name = rest[0].to_string();
                let mut args = serde_json::Map::new();
                for token in &rest[1..] {
                    if let Some((key, value)) = token.split_once('=') {
                        args.insert(
                            key.to_string(),
                            serde_json::Value::String(value.to_string()),
                        );
                    }
                }
                Self::Skill { name, args }
            }
            "/help" => Self::Help,
            _ => Self::Unknown(cmd.to_string()),
        }
    }
}

/// Execute a command and return a human-readable result message.
pub async fn execute(ctx: &mut CommandContext, cmd: Command) -> Result<String, String> {
    match cmd {
        Command::Yolo => {
            ctx.agent
                .lock()
                .await
                .set_permission_mode(PermissionMode::BypassPermissions);
            Ok("Permission mode set to bypass.".to_string())
        }
        Command::Auto => {
            ctx.agent
                .lock()
                .await
                .set_permission_mode(PermissionMode::AcceptEdits);
            Ok("Permission mode set to accept edits.".to_string())
        }
        Command::Permission(mode) => {
            let label = format!("{mode:?}").to_lowercase();
            ctx.agent.lock().await.set_permission_mode(mode);
            Ok(format!("Permission mode set to {label}."))
        }
        Command::Model(model) => switch_model(ctx, model).await,
        Command::Models => Ok(list_models(&ctx.llm)),
        Command::Provider(provider) => {
            ctx.model_switch.pending = None;
            switch_model_provider(ctx, None, Some(provider)).await
        }
        Command::ProviderRefresh(provider) => refresh_provider_models(ctx, provider).await,
        Command::Sessions(id) => list_sessions(ctx, id).await,
        Command::Tasks => list_tasks(ctx).await,
        Command::Mcp => list_mcp(ctx).await,
        Command::GoalCreate(objective) => run_goal_create(ctx, objective).await,
        Command::GoalPause => run_goal_update(ctx, agent_core::GoalStatus::Paused).await,
        Command::GoalResume => run_goal_update(ctx, agent_core::GoalStatus::Active).await,
        Command::GoalCancel => run_goal_update(ctx, agent_core::GoalStatus::Blocked).await,
        Command::Swarm(swarm) => Ok(set_swarm_mode(ctx, &swarm).await),
        Command::AddDir(path) => add_context_dir(ctx, path).await,
        Command::Reload => reload_context(ctx).await,
        Command::TakeOver => match ctx.agent.lock().await.take_over_session() {
            Ok(Some(holder)) => Ok(format!("Took the session over from {}.", holder.describe())),
            Ok(None) => Ok("No other process holds this session.".to_string()),
            Err(e) => Err(format!("Could not take the session over: {e}")),
        },
        Command::Title(title) => {
            ctx.agent.lock().await.set_session_title(title.clone());
            ctx.session_title.set(title.clone());
            crate::record_session_activity(ctx, Some(title.clone())).await;
            Ok(format!("Session title set to '{title}'."))
        }
        Command::Web(query) => run_web_search(ctx, query).await,
        Command::New => {
            ctx.model_switch.pending = None;
            let mut agent = ctx.agent.lock().await;
            agent.clear_context();
            agent.reset_cron();
            Ok("Started a fresh conversation in the current session.".to_string())
        }
        Command::Compact(instruction) => {
            let llm = ctx.llm.clone();
            let cancel = tokio_util::sync::CancellationToken::new();
            let notice = ctx
                .agent
                .lock()
                .await
                .compact_context(llm.as_ref(), &cancel, instruction.as_deref())
                .await
                .map_err(|e| format!("Compaction failed: {e}"))?;
            Ok(format!("Compaction: {}.", notice.describe()))
        }
        Command::Clear => {
            ctx.model_switch.pending = None;
            ctx.agent.lock().await.clear_context();
            Ok("Context cleared.".to_string())
        }
        Command::Undo(count) => {
            let mut agent = ctx.agent.lock().await;
            match count {
                None => {
                    let removed = agent.undo_last_turn();
                    if removed == 0 {
                        Ok("Nothing to undo.".to_string())
                    } else {
                        Ok(format!("Undid last turn (removed {removed} messages)."))
                    }
                }
                Some(1) => match agent.undo_turns(1) {
                    Ok(removed) => Ok(format!("Undid last turn (removed {removed} messages).")),
                    Err(e) => Err(e.to_string()),
                },
                Some(count) => match agent.undo_turns(count) {
                    Ok(removed) => Ok(format!("Undid {count} turns (removed {removed} messages).")),
                    Err(e) => Err(e.to_string()),
                },
            }
        }
        Command::Tools => {
            let tools = ctx.agent.lock().await.list_tools();
            if tools.is_empty() {
                Ok("No tools registered.".to_string())
            } else {
                Ok(format!("Tools: {}", tools.join(", ")))
            }
        }
        Command::Plan => {
            let enabled = ctx.agent.lock().await.toggle_plan_mode();
            Ok(plan_mode_reply(enabled).to_string())
        }
        Command::Skill { name, args } => run_skill(ctx, name, args).await,
        Command::Exit => Ok("Goodbye!".to_string()),
        Command::Help => Ok(help_text()),
        Command::Unknown(cmd) => {
            let cmd = cmd.trim_start_matches('/');
            // Parse-time usage feedback ("/goal usage: …") is shown as-is;
            // anything else is an unrecognized command.
            if let Some(usage) = cmd.strip_prefix("usage:") {
                Err(format!("usage:{usage}"))
            } else {
                Err(format!("Unknown command: /{cmd}. Type /help for a list."))
            }
        }
    }
}

/// The models the current provider knows, marking the one in use.
fn list_models(llm: &ProviderLlm) -> String {
    use agent_core::agent_loop::Llm;
    let current = llm.model_name();
    let models = llm.known_models();
    if models.is_empty() {
        return format!(
            "Model: {current} ({}). This provider publishes no model list; switch with /model <id>.",
            llm.provider_name()
        );
    }
    let mut out = format!("Models on {}:", llm.provider_name());
    for model in &models {
        let marker = if model.id == current { "*" } else { " " };
        out.push_str(&format!(
            "\n {marker} {:<28} {:>9} context{}{}",
            model.id,
            crate::provider_defaults::format_window(model.context_window),
            if model.supports_vision {
                ", vision"
            } else {
                ""
            },
            // The vendor's name says which model an alias is:
            // `kimi-for-coding` is K2.8 Preview.
            model
                .display_name
                .as_deref()
                .map(|name| format!(" — {name}"))
                .unwrap_or_default()
        ));
    }
    if !models.iter().any(|m| m.id == current) {
        out.push_str(&format!("\n * {current} (in use; not in the list)"));
    }
    out.push_str("\nSwitch with /model <id>.");
    out
}

/// `/provider`'s usage, shown for anything it does not take.
const PROVIDER_USAGE: &str = "usage: /provider <name> | /provider refresh [<name>]";

/// `/provider refresh [<name>]`: fetch the model list of the session's
/// provider, or of `name`, now rather than within a day, and say what changed.
/// The session's own provider is rebuilt on the new list.
async fn refresh_provider_models(
    ctx: &mut CommandContext,
    name: Option<String>,
) -> Result<String, String> {
    use agent_core::agent_loop::Llm;
    use lm_bridge::providers::{deepseek_catalog::DeepseekModelInfo, kimi_catalog::KimiModelInfo};

    let current = ctx.llm.provider_name().to_string();
    let provider = match name.map(|name| name.to_lowercase()) {
        Some(name) if name == "zai" => "glm".to_string(),
        Some(name) => name,
        None => current.clone(),
    };
    let refresh = match provider.as_str() {
        "kimi" => refresh_catalog_now::<KimiModelInfo>(ctx, &provider, &current).await?,
        "deepseek" => refresh_catalog_now::<DeepseekModelInfo>(ctx, &provider, &current).await?,
        "glm" | "openai" => {
            return Ok(format!(
                "{provider} publishes no model list to refresh: Rusta's built-in table \
                 describes its models."
            ));
        }
        other => return Err(format!("Unknown provider: {other}. {PROVIDER_USAGE}")),
    };
    let in_use = ctx.llm.model_name().to_string();
    if provider == current && !refresh.models.is_empty() {
        switch_model_provider(ctx, Some(in_use.clone()), None).await?;
    }
    Ok(refresh_report(
        &provider,
        &refresh,
        (provider == current).then_some(in_use.as_str()),
    ))
}

/// Fetch `provider`'s `T` catalogue now. The models compared against are
/// the session's own when `provider` is the session's, else the cached ones.
async fn refresh_catalog_now<T: lm_bridge::providers::catalog_cache::CatalogEntry>(
    ctx: &CommandContext,
    provider: &str,
    current: &str,
) -> Result<crate::catalogs::Refresh, String> {
    let (base_url, key, headers) = crate::catalog_source(&ctx.cli, provider)
        .map_err(|e| format!("Could not refresh {provider}'s models: {e}"))?;
    let known: Vec<String> = if provider == current {
        ctx.llm.known_models().into_iter().map(|m| m.id).collect()
    } else {
        crate::catalogs::cached_models::<T>(&base_url)
            .unwrap_or_default()
            .iter()
            .map(|m| m.id().to_string())
            .collect()
    };
    crate::catalogs::refresh_now::<T>(
        crate::catalogs::catalog_cache::<T>(),
        &base_url,
        &key,
        headers,
        &known,
    )
    .await
    .map_err(|e| format!("Could not refresh {provider}'s models: {e}"))
}

/// What `/provider refresh` tells the user. `in_use` is the session's model
/// when the refreshed provider is the session's.
fn refresh_report(
    provider: &str,
    refresh: &crate::catalogs::Refresh,
    in_use: Option<&str>,
) -> String {
    let count = refresh.models.len();
    let models = |n: usize| if n == 1 { "model" } else { "models" };
    if count == 0 {
        return format!(
            "{provider}: the model list carries no model details, so Rusta's built-in table \
             still applies."
        );
    }
    if refresh.added.is_empty() && refresh.removed.is_empty() {
        return if refresh.details_changed {
            format!(
                "{provider}: model details updated ({count} {}).",
                models(count)
            )
        } else {
            format!("{provider}: no model changes ({count} {}).", models(count))
        };
    }
    let mut counts = Vec::new();
    let mut names = Vec::new();
    if !refresh.added.is_empty() {
        counts.push(format!("+{}", refresh.added.len()));
        names.push(format!("added {}", refresh.added.join(", ")));
    }
    if !refresh.removed.is_empty() {
        counts.push(format!("-{}", refresh.removed.len()));
        names.push(format!("removed {}", refresh.removed.join(", ")));
    }
    let changed = refresh.added.len() + refresh.removed.len();
    let mut report = format!(
        "{provider}: {} {} ({}).",
        counts.join(" / "),
        models(changed),
        names.join("; ")
    );
    // A removed model fails the session's next request when it is in use.
    if let Some(model) = in_use.filter(|model| refresh.removed.iter().any(|id| id == model)) {
        report.push_str(&format!(
            " The model in use, {model}, is no longer listed: switch with /model <id>."
        ));
    }
    report
}

/// `/model <id>`: switch the session's model, confirming first a switch Kimi's
/// rules name. In a session that has begun, such a switch waits: the reply
/// gives Kimi's reasons and the ways on, and the same `/model <id>` again makes
/// it. In a new session it is made at once, with Kimi's note.
async fn switch_model(ctx: &mut CommandContext, model: String) -> Result<String, String> {
    use agent_core::agent_loop::Llm;
    let current = ctx.llm.model_name().to_string();
    let confirmed = ctx.model_switch.pending.take().as_deref() == Some(model.as_str());
    let rule = if confirmed || model == current {
        None
    } else {
        switch_rule(ctx, &current, &model).await
    };
    let Some((rule, session)) = rule else {
        return switch_model_provider(ctx, Some(model), None).await;
    };
    match session {
        SessionKind::Old => {
            let reply = switch_confirmation(ctx, &current, &model, &rule);
            ctx.model_switch.pending = Some(model);
            Ok(reply)
        }
        SessionKind::New => {
            let mut reply = switch_model_provider(ctx, Some(model), None).await?;
            for note in [rule.hint(), rule.warning()].into_iter().flatten() {
                reply.push_str(&format!("\n{note}"));
            }
            Ok(reply)
        }
    }
}

/// Kimi's rule for switching the session from `from` to `to`, and whether
/// the session has begun. Only Kimi Code serves the rules, and their ids
/// are its own; a run that cannot read them confirms nothing.
async fn switch_rule(
    ctx: &CommandContext,
    from: &str,
    to: &str,
) -> Option<(ModelSwitchRule, SessionKind)> {
    if ctx.llm.provider_name() != "kimi"
        || ctx.cli.billing.unwrap_or_default() != crate::endpoints::Billing::Subscription
    {
        return None;
    }
    let rules = ctx
        .model_switch
        .rules
        .get_or_init(|| async {
            match crate::native_web::kimi_services(&ctx.cli) {
                Some(services) => services.model_switch_rules().await,
                None => None,
            }
        })
        .await
        .as_ref()?;
    let session = if ctx.agent.lock().await.has_user_input() {
        SessionKind::Old
    } else {
        SessionKind::New
    };
    rules
        .matching(from, to, session)
        .map(|rule| (rule.clone(), session))
}

/// What a switch waiting for confirmation says: the switch, Kimi's reasons,
/// and the ways on: `/new` first, `/compact` first, or the same `/model`
/// again.
fn switch_confirmation(
    ctx: &CommandContext,
    from: &str,
    to: &str,
    rule: &ModelSwitchRule,
) -> String {
    use lm_bridge::providers::kimi_catalog::KimiModelInfo;
    // Kimi's name for each, from the live list or else the built-in one.
    let models = ctx.llm.known_models();
    let named = |id: &str| {
        let name = models
            .iter()
            .find(|model| model.id == id)
            .and_then(|model| model.display_name.clone())
            .or_else(|| KimiModelInfo::builtin(id).display_name);
        match name {
            Some(name) => format!("{id} ({name})"),
            None => id.to_string(),
        }
    };
    let mut out = format!("Switch model? {} → {}", named(from), named(to));
    for note in [rule.hint(), rule.warning()].into_iter().flatten() {
        out.push_str(&format!("\n{note}"));
    }
    out.push_str(&format!(
        "\n  /new, then /model {to}: start a fresh session on it\
         \n  /compact, then /model {to}: compact and continue, for lower cost and better \
         output quality in this session\
         \n  /model {to} again: continue as-is; output quality may suffer, and cache loss may \
         increase token usage\
         \nUntil then the session stays on {from}."
    ));
    out
}

async fn switch_model_provider(
    ctx: &mut CommandContext,
    model: Option<String>,
    provider: Option<String>,
) -> Result<String, String> {
    let new_provider =
        crate::build_provider_for(&ctx.cli, provider.clone(), model.clone(), ctx.timeouts)
            .map_err(|e| format!("failed to build provider: {e}"))?;

    // Report what was *built*, not what was asked for. Switching provider
    // re-derives the model — `/provider glm` from a Kimi session lands on
    // `glm-5.3`, not on the `kimi-for-coding` still sitting in `cli.model` — and
    // announcing the stale name would describe a session that does not exist.
    let window = new_provider.context_window();
    let provider_label = new_provider.name().to_string();
    let model_label = new_provider.model_name().to_string();

    // Server-side tools belong to the vendor that serves them, so the set has to
    // change with the provider. Leaving Moonshot's `$web_search` registered after
    // a switch to GLM makes Z.AI reject *every* later request with
    // `tools[N].type: type is illegal` — the session is dead until you restart.
    let builtins = new_provider.builtin_tools();

    ctx.llm = Arc::new(ctx.llm_options.build_llm(new_provider));
    ctx.cli.provider = Some(provider_label.clone());
    ctx.cli.model = Some(model_label.clone());

    {
        let mut agent = ctx.agent.lock().await;
        agent.set_llm(ctx.llm.clone());
        agent.set_context_budget(window);
        crate::register_builtin_tools(&mut agent, &builtins);
    }

    sync_native_tool_clients(ctx, &provider_label).await;

    Ok(format!(
        "Switched to provider={provider_label} model={model_label} (context window {window} tokens)."
    ))
}

/// Attach or detach the provider's own clients when it changes at runtime:
/// its web search and page reader (Z.AI's, the Kimi Coding Plan's, or
/// DeepSeek's), and what describes images for a model that cannot see them.
async fn sync_native_tool_clients(ctx: &mut CommandContext, provider: &str) {
    let web = crate::native_web::clients(&ctx.cli, provider);
    let vision = crate::vision_fallback::for_provider(
        &ctx.cli,
        provider,
        &ctx.config,
        &ctx.mcp_manager,
        ctx.timeouts,
    );
    let mut agent = ctx.agent.lock().await;
    match web.search {
        Some(client) => agent.set_web_search_client(client),
        None => agent.clear_web_search_client(),
    }
    match web.reader {
        Some(client) => agent.set_web_reader_client(client),
        None => agent.clear_web_reader_client(),
    }
    // What describes images for a model that cannot see them follows the
    // provider too: DeepSeek's own vision model, or Z.AI's vision server.
    match vision {
        Some(fallback) => agent.set_vision_fallback(fallback),
        None => agent.clear_vision_fallback(),
    }
}

async fn list_sessions(ctx: &CommandContext, id: Option<String>) -> Result<String, String> {
    let store = ctx.session_store.lock().await;
    if let Some(id) = id {
        match store.get(&id) {
            Some(entry) => Ok(format!(
                "Session '{}' ({}): title='{}', provider={}, model={}, last_active={}",
                entry.id,
                entry.records_path.display(),
                entry.display_title,
                entry.provider,
                entry.model,
                entry.last_active_at.format("%Y-%m-%d %H:%M")
            )),
            None => Ok(format!(
                "Session '{id}' not found in index. Start it with --resume --session {id}."
            )),
        }
    } else {
        let entries = store.list();
        if entries.is_empty() {
            return Ok("No saved sessions.".to_string());
        }
        let mut lines = vec!["Saved sessions:".to_string()];
        for entry in entries {
            let title = if entry.display_title.is_empty() {
                &entry.id
            } else {
                &entry.display_title
            };
            lines.push(format!(
                "  {} — '{}' ({} {}), last active {}",
                entry.id,
                title,
                entry.provider,
                entry.model,
                entry.last_active_at.format("%Y-%m-%d %H:%M")
            ));
        }
        Ok(lines.join("\n"))
    }
}

async fn list_tasks(ctx: &CommandContext) -> Result<String, String> {
    let agent = ctx.agent.lock().await;
    let tasks = agent.tool_ctx().background.list();
    if tasks.is_empty() {
        return Ok("No background tasks.".to_string());
    }
    let mut lines = vec!["Background tasks:".to_string()];
    for summary in tasks {
        lines.push(format!("  {summary}"));
    }
    Ok(lines.join("\n"))
}

async fn list_mcp(ctx: &CommandContext) -> Result<String, String> {
    let statuses = ctx.mcp_manager.statuses().await;
    if statuses.is_empty() {
        return Ok("No MCP servers configured.".to_string());
    }
    let mut lines = vec!["MCP servers:".to_string()];
    for (name, status) in statuses {
        lines.push(format!("  {name}: {status:?}"));
    }
    Ok(lines.join("\n"))
}

/// Turn swarm mode on or off (`/swarm`): while it is on, the agent
/// splits the work it is given across `agent_swarm`. For `/swarm <task>` it
/// holds for that task, which the caller then sends as the prompt
/// ([`Command::follow_up_prompt`]).
async fn set_swarm_mode(ctx: &CommandContext, swarm: &SwarmCommand) -> String {
    use agent_core::swarm::SwarmTrigger;
    let mut agent = ctx.agent.lock().await;
    let on = match swarm {
        SwarmCommand::Toggle => agent.swarm_mode().is_none(),
        SwarmCommand::On => true,
        SwarmCommand::Off => false,
        SwarmCommand::Task(_) => {
            agent.set_swarm_mode(Some(SwarmTrigger::Task));
            return "Swarm mode is on for this task.".to_string();
        }
    };
    if on {
        agent.set_swarm_mode(Some(SwarmTrigger::Manual));
        "Swarm mode on: the agent splits the work across agent_swarm until `/swarm off`."
            .to_string()
    } else {
        agent.set_swarm_mode(None);
        "Swarm mode off.".to_string()
    }
}

async fn add_context_dir(ctx: &mut CommandContext, path: PathBuf) -> Result<String, String> {
    let path = expand_tilde(&path);
    ctx.agent.lock().await.add_allowed_path(path.clone());
    // Where startup reads them from, so `/reload` lists the directory.
    if !ctx.cli.additional_dirs.contains(&path) {
        ctx.cli.additional_dirs.push(path.clone());
    }
    Ok(format!(
        "Added context/allowed directory: {}",
        path.display()
    ))
}

/// `/reload`: read the profiles, skills and instruction files again, rebuild
/// the system prompt from them, and tell the model what changed.
async fn reload_context(ctx: &mut CommandContext) -> Result<String, String> {
    let cwd = &ctx.cli.cwd;
    let profiles =
        crate::load_profile_registry(cwd).map_err(|e| format!("failed to reload profiles: {e}"))?;
    let skills = crate::load_skill_registry(cwd, &ctx.config.skill_dirs)
        .map_err(|e| format!("failed to reload skills: {e}"))?;
    let built = crate::system_prompt::build(&ctx.cli, &ctx.config, &profiles)?;
    let changes = ReloadChanges::between(
        &Snapshot {
            instruction_files: &ctx.instruction_files,
            skills: &ctx.skill_registry,
            profiles: &ctx.profile_registry,
        },
        &Snapshot {
            instruction_files: &built.instruction_files,
            skills: &skills,
            profiles: &profiles,
        },
    );
    let reminder = changes.reminder();
    {
        let mut agent = ctx.agent.lock().await;
        // Each replaces its tools in place, which keeps the tool table's
        // order and so the prompt cache.
        if !changes.profiles.is_empty() {
            ctx.profile_registry = Arc::new(profiles);
            // Re-registers `agent` and `agent_swarm` with the model pool kept;
            // registering them afresh here dropped `[subagent.models]` until
            // restart.
            agent.set_profile_registry(ctx.profile_registry.clone());
        }
        if !changes.skills.is_empty() {
            ctx.skill_registry = Arc::new(skills);
            agent.set_skill_registry(ctx.skill_registry.clone());
        }
        agent.set_system_prompt(built.text);
        agent.set_subagent_prompt(Some(built.subagent_prompt));
        agent.set_subagent_mcp_guidance(built.mcp_guidance);
        if let Some(reminder) = &reminder {
            agent.add_system_reminder(reminder);
        }
    }
    ctx.instruction_files = built.instruction_files;

    let mut reply = "Reloaded profiles, skills, project context and the system prompt.".to_string();
    if changes.is_empty() {
        reply.push_str("\nNo instruction file, skill or sub-agent profile changed.");
    } else {
        reply.push_str(&format!("\n{}", changes.summary()));
        if reminder.is_some() {
            reply.push_str("\nThe agent is told what changed with your next message.");
        }
    }
    for warning in built.warnings {
        reply.push_str(&format!("\nWarning: {warning}"));
    }
    Ok(reply)
}

async fn run_web_search(ctx: &CommandContext, query: String) -> Result<String, String> {
    if query.is_empty() {
        return Err("/web requires a query.".to_string());
    }
    let tool = WebSearchTool::new();
    let input = json!({ "query": query });
    let tool_ctx = ctx.agent.lock().await.next_turn_tool_ctx();
    let result = tool.execute(input, &tool_ctx, "slash-web").await;
    if result.is_error {
        Err(result.text_content())
    } else {
        Ok(result.text_content())
    }
}

async fn run_goal_create(ctx: &CommandContext, objective: String) -> Result<String, String> {
    if objective.is_empty() {
        return Err("/goal create requires an objective.".to_string());
    }
    let agent = ctx.agent.lock().await;
    let mut manager = agent.tool_ctx().goal_manager.lock().unwrap();
    match manager.create(objective, None, true) {
        Ok(id) => Ok(format!(
            "Created goal {id}. The agent will pursue it across turns until it is paused, completed, or blocked."
        )),
        Err(e) => Err(e),
    }
}

async fn run_goal_update(ctx: &CommandContext, status: GoalStatus) -> Result<String, String> {
    let agent = ctx.agent.lock().await;
    let mut manager = agent.tool_ctx().goal_manager.lock().unwrap();
    match manager.update(status) {
        Ok(id) => Ok(format!("Goal {id} updated to {status}.")),
        Err(e) => Err(e),
    }
}

async fn run_skill(
    ctx: &CommandContext,
    name: String,
    args: serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    if name.is_empty() {
        return Err("/skill requires a skill name.".to_string());
    }
    let agent = ctx.agent.lock().await;
    let tool = SkillTool::for_user(ctx.skill_registry.clone(), agent.active_skills());
    let input = serde_json::Value::Object({
        let mut map = serde_json::Map::new();
        map.insert("name".to_string(), serde_json::Value::String(name));
        if !args.is_empty() {
            map.insert("args".to_string(), serde_json::Value::Object(args));
        }
        map
    });
    let result = tool.execute(input, agent.tool_ctx(), "slash-skill").await;
    if result.is_error {
        Err(result.text_content())
    } else {
        Ok(result.text_content())
    }
}

/// What `/plan` tells the user. The model hears it with its next turn.
fn plan_mode_reply(enabled: bool) -> &'static str {
    if enabled {
        "Plan mode on: the agent plans, every edit waits for your approval, and you review the \
         plan before plan mode ends. /plan again turns it off."
    } else {
        "Plan mode off."
    }
}

fn help_text() -> String {
    "Slash commands:
  /yolo, /auto, /permission <mode>  — change permission mode
  /model [<id>], /provider <name>   — list models, or switch model/provider
  /provider refresh [<name>]        — fetch the provider's model list again and say what changed
  /sessions [id], /tasks, /mcp      — browse state
  /swarm [on|off] | <task>          — toggle swarm mode, or run one task in it
  /goal create <objective> | pause | resume | cancel
  /skill <name> [key=value ...]     — invoke a reusable skill prompt
  /add-dir <path>                   — add allowed/context directory
  /reload                           — reload profiles, skills and project context
  /title <name>                     — set session title
  /takeover                         — take the session over from another process that holds it
  /web <query>                      — run web search
  /new                              — clear context
  /compact [instruction]            — summarize the conversation to free context; the instruction hints what to preserve
  /clear                            — clear the conversation context
  /undo [count]                     — remove the last turn(s); a count past what can be undone is refused
  /tools                            — list registered tools
  /plan                             — turn plan mode on or off: the agent plans, and you review the plan before it changes anything
  /exit, /quit                      — quit the application
  /help                             — show this help"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_permission() {
        assert!(matches!(
            Command::parse("/permission accept_edits"),
            Command::Permission(PermissionMode::AcceptEdits)
        ));
    }

    /// `/provider` switches, refreshes, or shows its usage.
    #[test]
    fn provider_takes_a_name_or_refresh() {
        assert!(matches!(Command::parse("/provider glm"), Command::Provider(p) if p == "glm"));
        assert!(matches!(
            Command::parse("/provider refresh"),
            Command::ProviderRefresh(None)
        ));
        assert!(matches!(
            Command::parse("/provider refresh kimi"),
            Command::ProviderRefresh(Some(p)) if p == "kimi"
        ));
        for line in [
            "/provider",
            "/provider refresh kimi glm",
            "/provider kimi glm",
        ] {
            assert!(
                matches!(Command::parse(line), Command::Unknown(u) if u == PROVIDER_USAGE),
                "{line}"
            );
        }
    }

    #[test]
    fn a_refresh_report_says_what_changed() {
        use crate::catalogs::Refresh;
        let ids = |ids: &[&str]| ids.iter().map(ToString::to_string).collect::<Vec<_>>();
        let changed = Refresh {
            models: ids(&["flash", "pro"]),
            added: ids(&["pro"]),
            removed: ids(&["old"]),
            details_changed: false,
        };
        assert_eq!(
            refresh_report("deepseek", &changed, Some("flash")),
            "deepseek: +1 / -1 models (added pro; removed old)."
        );
        assert_eq!(
            refresh_report("deepseek", &changed, Some("old")),
            "deepseek: +1 / -1 models (added pro; removed old). The model in use, old, is no \
             longer listed: switch with /model <id>."
        );
        let added = Refresh {
            models: ids(&["flash", "pro"]),
            added: ids(&["pro"]),
            ..Refresh::default()
        };
        assert_eq!(
            refresh_report("kimi", &added, None),
            "kimi: +1 model (added pro)."
        );
        let same = Refresh {
            models: ids(&["flash"]),
            ..Refresh::default()
        };
        assert_eq!(
            refresh_report("kimi", &same, None),
            "kimi: no model changes (1 model)."
        );
        let details = Refresh {
            details_changed: true,
            ..same
        };
        assert_eq!(
            refresh_report("kimi", &details, None),
            "kimi: model details updated (1 model)."
        );
        assert!(refresh_report("kimi", &Refresh::default(), None).contains("built-in table"));
    }

    #[test]
    fn test_parse_model() {
        assert!(matches!(Command::parse("/model glm-5.1"), Command::Model(m) if m == "glm-5.1"));
        assert!(matches!(Command::parse("/model"), Command::Models));
    }

    /// `/model` on DeepSeek lists V4.1-Flash, the default, first.
    #[test]
    fn deepseek_lists_flash_first() {
        let deepseek = lm_bridge::providers::deepseek::DeepseekChatProvider::new(
            "deepseek-flash",
            Some("sk-test".to_string()),
            "https://api.deepseek.com",
            lm_bridge::providers::openai_compat::HttpTimeouts::idle(
                std::time::Duration::from_secs(5),
            ),
        )
        .unwrap();
        let listing = list_models(&ProviderLlm::new(Box::new(deepseek)));
        // Model lines are indented, under the heading and above the hint.
        let models: Vec<&str> = listing
            .lines()
            .filter(|line| line.starts_with(' '))
            .filter_map(|line| {
                line.trim_start_matches([' ', '*'])
                    .split_whitespace()
                    .next()
            })
            .collect();
        assert_eq!(models, ["deepseek-flash", "deepseek-v4-pro"], "{listing}");
        assert!(
            listing
                .lines()
                .nth(1)
                .is_some_and(|line| line.trim_start().starts_with("* ")),
            "the default is the model in use: {listing}"
        );
    }

    /// Bare `/model` lists the provider's models and marks the one in use.
    #[test]
    fn bare_model_lists_the_known_models() {
        let kimi = lm_bridge::providers::kimi::KimiChatProvider::new(
            "k3".to_string(),
            Some("sk-test".to_string()),
            "https://api.kimi.com/coding/v1",
            lm_bridge::providers::openai_compat::HttpTimeouts::idle(
                std::time::Duration::from_secs(5),
            ),
        )
        .unwrap();
        let listing = list_models(&ProviderLlm::new(Box::new(kimi)));
        assert!(listing.starts_with("Models on kimi:"), "{listing}");
        let k3 = listing
            .lines()
            .find(|line| line.contains(" k3 "))
            .expect("k3 is listed");
        assert!(k3.trim_start().starts_with("* k3"), "{k3}");
        assert!(k3.contains("1M context, vision — K3"), "{k3}");
        assert!(listing.contains("k3-256k"), "{listing}");
        assert!(listing.contains("k3.1-pro "), "{listing}");
        // The alias says which model it serves.
        let default = listing
            .lines()
            .find(|line| line.contains(" kimi-for-coding "))
            .expect("kimi-for-coding is listed");
        assert!(default.ends_with("— K2.8 Preview"), "{default}");
        assert!(
            !listing.contains("not in the list"),
            "the model in use is on the list: {listing}"
        );

        let custom = lm_bridge::providers::kimi::KimiChatProvider::new(
            "kimi-experimental".to_string(),
            Some("sk-test".to_string()),
            "https://api.kimi.com/coding/v1",
            lm_bridge::providers::openai_compat::HttpTimeouts::idle(
                std::time::Duration::from_secs(5),
            ),
        )
        .unwrap();
        let listing = list_models(&ProviderLlm::new(Box::new(custom)));
        assert!(
            listing.contains("* kimi-experimental (in use; not in the list)"),
            "{listing}"
        );
    }

    #[test]
    fn test_parse_swarm() {
        assert_eq!(
            Command::parse("/swarm"),
            Command::Swarm(SwarmCommand::Toggle)
        );
        assert_eq!(
            Command::parse("/swarm on"),
            Command::Swarm(SwarmCommand::On)
        );
        assert_eq!(
            Command::parse("/swarm off"),
            Command::Swarm(SwarmCommand::Off)
        );
        let task = Command::parse("/swarm audit every service");
        assert_eq!(
            task,
            Command::Swarm(SwarmCommand::Task("audit every service".to_string()))
        );
        assert_eq!(
            task.follow_up_prompt().as_deref(),
            Some("audit every service")
        );
        assert_eq!(Command::parse("/swarm on").follow_up_prompt(), None);
    }

    #[test]
    fn test_parse_exit() {
        assert!(matches!(Command::parse("/exit"), Command::Exit));
        assert!(matches!(Command::parse("/quit"), Command::Exit));
    }

    #[test]
    fn test_parse_goal_create_multi_word() {
        let cmd = Command::parse("/goal create fix the login bug");
        if let Command::GoalCreate(objective) = cmd {
            assert_eq!(objective, "fix the login bug");
        } else {
            panic!("expected goal create command");
        }
    }

    /// `/compact [<instruction>]`: the argument is the summarizer's focus
    /// instruction; a bare `/compact` passes none.
    #[test]
    fn test_parse_compact_instruction() {
        assert_eq!(Command::parse("/compact"), Command::Compact(None));
        assert_eq!(Command::parse("/compact    "), Command::Compact(None));
        assert_eq!(
            Command::parse("/compact keep the auth notes"),
            Command::Compact(Some("keep the auth notes".to_string()))
        );
    }

    /// `/undo [<count>]`: no argument keeps the single-turn command; a
    /// positive integer count is parsed; anything else is a usage error.
    #[test]
    fn test_parse_undo_count() {
        assert_eq!(Command::parse("/undo"), Command::Undo(None));
        assert_eq!(Command::parse("/undo 3"), Command::Undo(Some(3)));
        assert_eq!(Command::parse("/undo 1"), Command::Undo(Some(1)));
        for bad in [
            "/undo 0",
            "/undo 03",
            "/undo -1",
            "/undo +2",
            "/undo two",
            "/undo 3 extra",
        ] {
            assert_eq!(
                Command::parse(bad),
                Command::Unknown(
                    "usage: /undo [count], where count is a positive integer".to_string()
                ),
                "{bad}"
            );
        }
        // Overflowing the machine integer is a usage error too.
        assert_eq!(
            Command::parse("/undo 99999999999999999999999999"),
            Command::Unknown("usage: /undo [count], where count is a positive integer".to_string())
        );
    }

    /// The agent's conversation as text, and how many `/reload` reminders it
    /// holds: adjacent user messages, a reminder among them, fold into one.
    async fn reload_reminders(ctx: &CommandContext) -> (String, usize) {
        let agent = ctx.agent.lock().await;
        let text = agent
            .history()
            .iter()
            .map(lm_bridge::message::Message::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        let count = text
            .matches("<system-reminder>\nThe user ran /reload")
            .count();
        (text, count)
    }

    /// `/reload` reads the instruction files, skills and profiles again: what
    /// changed is named in the reply and told to the model once, the prompt
    /// and the `skill` tool carry it, and a reload that finds nothing new
    /// says so and tells the model nothing.
    #[tokio::test]
    async fn reload_names_what_changed_to_the_user_and_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = CommandContext::for_test(dir.path()).await;
        ctx.cli.cwd = dir.path().to_path_buf();
        // The first reload takes in whatever this machine's user-level
        // files hold; the test counts from there.
        reload_context(&mut ctx).await.unwrap();
        let (_, baseline) = reload_reminders(&ctx).await;

        let reply = reload_context(&mut ctx).await.unwrap();
        assert_eq!(
            reply,
            "Reloaded profiles, skills, project context and the system prompt.\n\
             No instruction file, skill or sub-agent profile changed."
        );
        assert_eq!(reload_reminders(&ctx).await.1, baseline);

        let agents_md = dir.path().join("AGENTS.md");
        std::fs::write(&agents_md, "Run the tests.").unwrap();
        let skills = dir.path().join(".rusta/skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(skills.join("review.md"), "Review the diff.").unwrap();
        let profiles = dir.path().join(".rusta/profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(profiles.join("reviewer.yaml"), "name: reviewer\n").unwrap();

        let reply = reload_context(&mut ctx).await.unwrap();
        let changed = format!(
            "Instruction files: added {}\nSkills: added review\nSub-agent profiles: added reviewer",
            agents_md.display()
        );
        assert!(reply.contains(&format!("\n{changed}\n")), "{reply}");
        let (text, count) = reload_reminders(&ctx).await;
        assert_eq!(count, baseline + 1);
        let reminder = text.rsplit("The user ran /reload").next().unwrap();
        assert!(
            reminder.contains("\n- Skills: added review\n"),
            "{reminder}"
        );
        {
            let agent = ctx.agent.lock().await;
            assert!(agent.history()[0].text_content().contains("Run the tests."));
            // Sub-agents get the new prompt too.
            let subagents = agent.subagent_prompt().unwrap().render("");
            assert!(subagents.contains("Run the tests."), "{subagents}");
            let tools = agent.tool_definitions();
            let skill = tools.iter().find(|tool| tool.name == "skill").unwrap();
            assert!(
                skill.description.contains("\n- review: Review the diff."),
                "{}",
                skill.description
            );
        }
        assert!(ctx.profile_registry.get("reviewer").is_some());

        std::fs::write(&agents_md, "Run the tests, then the linter.").unwrap();
        let reply = reload_context(&mut ctx).await.unwrap();
        assert!(
            reply.contains(&format!(
                "\nInstruction files: changed {}\n",
                agents_md.display()
            )),
            "{reply}"
        );
        assert_eq!(reload_reminders(&ctx).await.1, baseline + 2);

        // A directory `/add-dir` adds is read as startup reads `--add-dir`.
        let shared = tempfile::tempdir().unwrap();
        std::fs::write(shared.path().join("AGENTS.md"), "Shared rules.").unwrap();
        add_context_dir(&mut ctx, shared.path().to_path_buf())
            .await
            .unwrap();
        let reply = reload_context(&mut ctx).await.unwrap();
        assert!(
            reply.contains(&format!(
                "\nInstruction files: added {}\n",
                shared.path().join("AGENTS.md").display()
            )),
            "{reply}"
        );
        assert_eq!(reload_reminders(&ctx).await.1, baseline + 3);

        // A skill kept for the user is named to the user, not the model.
        std::fs::write(
            skills.join("deploy.md"),
            "---\ndisable_model_invocation: true\n---\nShip it.",
        )
        .unwrap();
        let reply = reload_context(&mut ctx).await.unwrap();
        assert!(reply.ends_with("\nSkills: added deploy"), "{reply}");
        assert_eq!(reload_reminders(&ctx).await.1, baseline + 3);
    }

    /// `/model` rebuilds the session's model from the session's settings, so
    /// what the session started with — here the hour-long prompt cache an
    /// interactive session asks Kimi's platform for — reaches the new model.
    #[tokio::test]
    async fn a_model_switch_keeps_the_sessions_settings() {
        use agent_core::agent_loop::{Llm, LlmCallbacks};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Only the request matters: whatever comes back is not a stream.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = CommandContext::for_test(dir.path()).await;
        ctx.cli.provider = Some("kimi".to_string());
        ctx.cli.base_url = Some(server.uri());
        ctx.cli.api_key = Some("sk-test".to_string());
        ctx.llm_options = LlmOptions::from_cli(&ctx.cli);

        execute(&mut ctx, Command::parse("/model kimi-k3.1-pro"))
            .await
            .unwrap();
        ctx.llm
            .chat(
                vec![lm_bridge::message::Message::user("hi")],
                Vec::new(),
                LlmCallbacks::default(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], json!("kimi-k3.1-pro"));
        assert_eq!(
            body["prompt_cache_options"],
            json!({"mode": "implicit", "ttl": "1h"}),
            "{body}"
        );
    }

    /// A session on `model` at a mock Kimi Code host that serves Kimi's
    /// switch rules, and the number of times they are read.
    async fn kimi_coding_session(
        dir: &std::path::Path,
        model: &str,
    ) -> (CommandContext, wiremock::MockServer) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/client_configs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "model_switch_confirm",
                "config": {"enable": true, "rules": [
                    {"to": ["k3.1-pro-highspeed"], "from": "*", "session_type": "new",
                     "hint": {"en": "Note: K3.1 Pro Highspeed consumption is approximately 2x that of K3.1 Pro"}},
                    {"to": ["k3.1-pro-highspeed"], "from": "*", "session_type": "old",
                     "hint": {"en": "Switching to K3.1 Pro Highspeed in this session may affect output quality. Choose how to continue"},
                     "warning": {"en": "Note: K3.1 Pro Highspeed consumption is approximately 2x that of K3.1 Pro"}},
                    {"to": ["k3.1-pro", "k3.1-pro-highspeed"],
                     "from": ["k3", "k3-256k", "kimi-for-coding", "kimi-for-coding-highspeed"],
                     "session_type": "old",
                     "hint": {"en": "Switching to a K3.1 series model in this session may affect output quality. Choose how to continue"}}
                ]}
            })))
            .mount(&server)
            .await;
        let mut ctx = CommandContext::for_test(dir).await;
        ctx.cli.provider = Some("kimi".to_string());
        ctx.cli.model = Some(model.to_string());
        ctx.cli.base_url = Some(server.uri());
        ctx.cli.api_key = Some("sk-test".to_string());
        let provider =
            crate::build_provider_for(&ctx.cli, None, None, ctx.timeouts).expect("a Kimi provider");
        ctx.llm = Arc::new(ProviderLlm::new(provider));
        (ctx, server)
    }

    async fn rule_reads(server: &wiremock::MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/client_configs")
            .count()
    }

    /// In a session that has begun, a switch Kimi's rules name waits: the
    /// reply gives Kimi's reason and the ways on, and the same `/model`
    /// again makes it. `/new` drops the waiting switch.
    #[tokio::test]
    async fn a_switch_to_k3_1_in_a_begun_session_waits_for_confirmation() {
        use agent_core::agent_loop::Llm;
        let dir = tempfile::tempdir().unwrap();
        let (mut ctx, server) = kimi_coding_session(dir.path(), "kimi-for-coding").await;
        // As a session records its first turn: the date reminder, then what
        // the user wrote.
        ctx.agent.lock().await.resume(vec![
            lm_bridge::message::Message::user(
                "<system-reminder>Today's date is 2026-10-27.</system-reminder>",
            ),
            lm_bridge::message::Message::user("fix the login test"),
            lm_bridge::message::Message::assistant_text("Fixed."),
        ]);

        let reply = execute(&mut ctx, Command::parse("/model k3.1-pro"))
            .await
            .unwrap();
        assert!(
            reply.starts_with("Switch model? kimi-for-coding (K2.8 Preview) → k3.1-pro (K3.1 Pro)"),
            "{reply}"
        );
        assert!(
            reply.contains("\nSwitching to a K3.1 series model in this session"),
            "{reply}"
        );
        assert!(reply.contains("/compact, then /model k3.1-pro"), "{reply}");
        assert!(reply.contains("/new, then /model k3.1-pro"), "{reply}");
        assert_eq!(ctx.llm.model_name(), "kimi-for-coding", "not switched yet");

        let reply = execute(&mut ctx, Command::parse("/model k3.1-pro"))
            .await
            .unwrap();
        assert!(
            reply.starts_with("Switched to provider=kimi model=k3.1-pro"),
            "{reply}"
        );
        assert_eq!(ctx.llm.model_name(), "k3.1-pro");

        // The warning comes apart from the reason; a different switch is
        // asked about afresh, and /new forgets it.
        let reply = execute(&mut ctx, Command::parse("/model k3.1-pro-highspeed"))
            .await
            .unwrap();
        assert!(
            reply.contains("\nNote: K3.1 Pro Highspeed consumption"),
            "{reply}"
        );
        assert_eq!(
            rule_reads(&server).await,
            1,
            "the rules are read once per run"
        );
        for forgets in ["/clear", "/provider kimi", "/new"] {
            ctx.model_switch.pending = Some("k3.1-pro-highspeed".to_string());
            execute(&mut ctx, Command::parse(forgets)).await.unwrap();
            assert!(ctx.model_switch.pending.is_none(), "{forgets}");
        }
    }

    /// In a new session the switch is made at once, with Kimi's note; a
    /// switch no rule names, or one off Kimi Code, is made without a word.
    #[tokio::test]
    async fn a_switch_in_a_new_session_is_made_with_kimis_note() {
        use agent_core::agent_loop::Llm;
        let dir = tempfile::tempdir().unwrap();
        let (mut ctx, _server) = kimi_coding_session(dir.path(), "k3.1-pro").await;
        let reply = execute(&mut ctx, Command::parse("/model k3.1-pro-highspeed"))
            .await
            .unwrap();
        assert!(
            reply.starts_with("Switched to provider=kimi model=k3.1-pro-highspeed"),
            "{reply}"
        );
        assert!(
            reply.ends_with(
                "\nNote: K3.1 Pro Highspeed consumption is approximately 2x that of K3.1 Pro"
            ),
            "{reply}"
        );

        let reply = execute(&mut ctx, Command::parse("/model k3"))
            .await
            .unwrap();
        assert!(!reply.contains('\n'), "{reply}");
        assert_eq!(ctx.llm.model_name(), "k3");

        // On the pay-per-token platform the rules are not asked for.
        let dir = tempfile::tempdir().unwrap();
        let (mut ctx, server) = kimi_coding_session(dir.path(), "kimi-for-coding").await;
        ctx.cli.billing = Some(crate::endpoints::Billing::Api);
        ctx.agent
            .lock()
            .await
            .resume(vec![lm_bridge::message::Message::user("hi")]);
        let reply = execute(&mut ctx, Command::parse("/model k3.1-pro"))
            .await
            .unwrap();
        assert!(reply.starts_with("Switched to"), "{reply}");
        assert_eq!(rule_reads(&server).await, 0);
    }

    /// `/web` searches through the provider's own search from a session's
    /// start, and through the new provider's right after a switch, before
    /// any turn has taken the client up. It serves no prompt, so it sends
    /// none along.
    #[tokio::test]
    async fn web_searches_through_the_attached_client() {
        struct Search(&'static str);

        #[async_trait::async_trait]
        impl agent_core::web_search_client::WebSearchClient for Search {
            async fn search(&self, query: &str, question: Option<&str>) -> Result<String, String> {
                Ok(format!("{} found {query} for {question:?}", self.0))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut ctx = CommandContext::for_test(dir.path()).await;
        for provider in ["deepseek", "kimi"] {
            ctx.agent
                .lock()
                .await
                .set_web_search_client(Arc::new(Search(provider)));
            let reply = execute(&mut ctx, Command::parse("/web luhn check"))
                .await
                .unwrap();
            assert_eq!(reply, format!("{provider} found luhn check for None"));
        }
    }

    /// `/title` names the session in the index, in this run (which the TUI
    /// shows) and for the hooks.
    #[tokio::test]
    async fn title_names_the_session_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = CommandContext::for_test(dir.path()).await;
        let reply = execute(&mut ctx, Command::parse("/title Lexer port"))
            .await
            .unwrap();
        assert_eq!(reply, "Session title set to 'Lexer port'.");
        assert_eq!(ctx.session_title.current(), "Lexer port");
        assert_eq!(ctx.agent.lock().await.session_title(), "Lexer port");
        let entry = ctx.session_store.lock().await.get("default").unwrap();
        assert_eq!(entry.label(), "Lexer port");
    }

    /// The usage error renders as itself, not as "Unknown command:".
    #[test]
    fn test_undo_usage_error_message() {
        let ctx_err = match Command::parse("/undo nope") {
            Command::Unknown(cmd) => cmd.trim_start_matches('/').to_string(),
            other => panic!("expected Unknown, got {other:?}"),
        };
        assert_eq!(
            ctx_err,
            "usage: /undo [count], where count is a positive integer"
        );
    }
}
