//! What `agent` and `agent_swarm` share: building a sub-agent, and the
//! description of it kept beside its records so that a resume rebuilds the
//! same sub-agent.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::{
    Agent, Profile,
    agent_loop::Llm,
    agent_loop::types::{ExecutableToolContext, ForkSource},
    model_pool::{Binding, ModelPool, resolve_binding},
    tool_disclosure::ToolDisclosureState,
};

/// What a sub-agent is, kept beside its records as `<records>.subagent.json`.
///
/// A resume rebuilds the sub-agent from it, with its own type and model.
/// Without it a resume would take whatever profile the call named — `default`
/// when it named none — so a resumed `explore` or `coder` would come back as
/// another type, with other tools and instructions. A fork keeps the
/// instructions it runs under too: its caller's, which no profile rebuilds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentMeta {
    /// The profile it runs under.
    pub profile: String,
    /// The pool alias it runs on; `None` for the model of the agent that
    /// spawned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The `agent_swarm` item it was spawned for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_item: Option<String>,
    /// For a fork, the system prompt it runs under: its caller's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_instructions: Option<String>,
}

impl SubagentMeta {
    fn path(records: &Path) -> PathBuf {
        crate::records::sidecar_path(records, "subagent")
    }

    /// Save beside the records file `records`.
    pub(crate) async fn save(&self, records: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        tokio::fs::write(Self::path(records), json)
            .await
            .map_err(|e| format!("failed to save the sub-agent's description: {e}"))
    }

    /// The description saved beside `records`; `None` for a sub-agent
    /// recorded before descriptions were kept.
    pub(crate) async fn load(records: &Path) -> Option<Self> {
        let bytes = tokio::fs::read(Self::path(records)).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// A sub-agent's model, and the pool alias to record for it — `None` for the
/// caller's own.
pub(crate) type BoundModel = (Option<Arc<dyn Llm>>, Option<String>);

/// The model a sub-agent runs on, and the pool alias to record for it: the
/// caller's model for `primary` or no request, else the pool entry named.
pub(crate) fn bind_model(
    ctx: &ExecutableToolContext,
    pool: Option<&dyn ModelPool>,
    requested: Option<&str>,
) -> Result<BoundModel, String> {
    match resolve_binding(pool, requested)? {
        Binding::Primary => Ok((ctx.llm.clone(), None)),
        Binding::Alias(alias) => {
            let pool = pool.expect("a binding to an alias implies a pool");
            let llm = pool
                .resolve(&alias)
                .map_err(|e| format!("failed to bind subagent to model {alias:?}: {e}"))?;
            Ok((Some(llm), Some(alias)))
        }
    }
}

/// A sub-agent under `profile`: the session's prompt with the profile's role
/// framed as a sub-agent's, the profile's tool set, and — for a profile that
/// takes them — the session's MCP tools with the guidance for them.
pub(crate) fn spawn_with_profile(
    ctx: &ExecutableToolContext,
    profile: &Profile,
    llm: Option<Arc<dyn Llm>>,
) -> Agent {
    let mcp = profile.mcp_access();
    let role = profile.system_prompt.as_deref().unwrap_or_default();
    let mut system_prompt = match &ctx.subagent_prompt {
        Some(prompt) => prompt.render(role),
        None => crate::prompt::subagent_prompt(role, &ctx.cwd),
    };
    if let Some(guidance) = ctx
        .subagent_mcp_guidance
        .as_deref()
        .and_then(|guidance| guidance.for_tools(mcp))
    {
        system_prompt.truncate(system_prompt.trim_end().len());
        system_prompt.push_str("\n\n");
        system_prompt.push_str(guidance.trim());
        system_prompt.push('\n');
    }
    let mut agent = Agent::spawn_child(ctx, system_prompt, llm);
    if !profile.allowed_tools.is_empty() {
        let allowed: std::collections::HashSet<String> =
            profile.allowed_tools.iter().cloned().collect();
        agent.restrict_tools(&allowed);
    }
    if !profile.denied_tools.is_empty() {
        let denied: std::collections::HashSet<String> =
            profile.denied_tools.iter().cloned().collect();
        agent.remove_tools(&denied);
    }
    if profile.git_context {
        agent.enable_git_context();
    }
    agent.adopt_mcp_tools(ctx, mcp);
    agent.set_profile(Some(profile.name.clone()));
    if let Some(hooks) = &ctx.user_hooks {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "name".to_string(),
            serde_json::Value::String(profile.name.clone()),
        );
        hooks.fire_spawned(
            crate::user_hooks::UserHookEvent::SubagentStart,
            profile.name.clone(),
            fields,
        );
    }
    agent
}

/// A fork of the calling agent: its instructions, conversation, tools and
/// type, under a fresh budget.
///
/// The fork starts without the caller's cron tasks: they stay with
/// the source session, which keeps running them. A sub-agent cannot schedule
/// any of its own, so unlike a session fork it is not told about them.
pub(crate) fn spawn_fork(
    ctx: &ExecutableToolContext,
    source: ForkSource,
    llm: Option<Arc<dyn Llm>>,
) -> Agent {
    let baseline = crate::agent_loop::types::ForkPrefixBaseline {
        system_prompt: source.system_prompt.clone(),
        tools: caller_tool_table(ctx),
    };
    let mut agent = fork_agent(ctx, source.system_prompt, ctx.profile.clone(), llm)
        .with_forked_context(source.history);
    agent.tool_ctx.cron = Arc::new(crate::cron::CronManager::new());
    *agent.tool_ctx.fork_prefix_baseline.lock().unwrap() = Some(baseline);
    if let Some(hooks) = &ctx.user_hooks {
        let name = ctx.profile.clone().unwrap_or_else(|| "fork".to_string());
        let mut fields = serde_json::Map::new();
        fields.insert("name".to_string(), serde_json::Value::String(name.clone()));
        hooks.fire_spawned(
            crate::user_hooks::UserHookEvent::SubagentStart,
            name,
            fields,
        );
    }
    agent
}

/// The tool table the caller's step offers: what its last request declared.
fn caller_tool_table(ctx: &ExecutableToolContext) -> Vec<lm_bridge::tool::Tool> {
    let Some(registry) = &ctx.tool_registry else {
        return Vec::new();
    };
    let disclosure = ctx
        .tool_disclosure
        .clone()
        .unwrap_or_else(|| Arc::new(Mutex::new(ToolDisclosureState::new())));
    crate::agent_loop::run_turn::offered_tools(registry, ctx, &disclosure)
}

/// A fork without its caller's conversation: one rebuilt to resume, whose
/// records hold the conversation.
pub(crate) fn fork_agent(
    ctx: &ExecutableToolContext,
    instructions: String,
    profile: Option<String>,
    llm: Option<Arc<dyn Llm>>,
) -> Agent {
    let mut agent = Agent::spawn_child(ctx, instructions, llm);
    // The fork's whole table is the caller's — same order, same definitions,
    // deferred where the caller defers — so its first request hits the
    // caller's prompt cache. Covers the MCP tools
    // `adopt_mcp_tools` copies for a profile.
    agent.adopt_tool_table(ctx);
    agent.set_profile(profile);
    agent
}

/// The description a fork keeps beside its records.
pub(crate) fn fork_meta(
    ctx: &ExecutableToolContext,
    source: &ForkSource,
    model: Option<String>,
    swarm_item: Option<String>,
) -> SubagentMeta {
    SubagentMeta {
        profile: ctx.profile.clone().unwrap_or_else(|| "default".to_string()),
        model,
        swarm_item,
        fork_instructions: Some(source.system_prompt.clone()),
    }
}
