//! `agent_swarm` — many sub-agents from one prompt template, resumes of
//! existing sub-agents, or both.
//!
//! Members are checked in a fixed order before any starts, resumes are
//! launched before new members, launches are paced, and the outcome is one
//! `<agent_swarm_result>` report.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use lm_bridge::tool::Tool;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::agent::{RunOutcome, new_agent_id, run_bounded, subagent_records_path};
use super::subagent::{
    SubagentMeta, bind_model, fork_agent, fork_meta, spawn_fork, spawn_with_profile,
};
use crate::{
    Agent, ProfileRegistry,
    agent_loop::types::{
        ExecutableTool, ExecutableToolContext, ExecutableToolResult, ForkSource, StopReason,
        ToolAccesses,
    },
    errors::AgentError,
    model_pool::{ModelPool, PRIMARY_ALIAS},
    records::Records,
};

/// The tool's name.
pub const AGENT_SWARM_TOOL_NAME: &str = "agent_swarm";

/// The placeholder `prompt_template` fills with each item.
pub const PROMPT_TEMPLATE_PLACEHOLDER: &str = "{{item}}";

/// The most sub-agents one swarm may run, resumes included.
pub const MAX_AGENT_SWARM_SUBAGENTS: usize = 128;

/// The type item-spawned sub-agents run as when `subagent_type` is omitted.
const DEFAULT_SUBAGENT_TYPE: &str = "coder";

/// Members launched at once before the rest are paced.
const INITIAL_LAUNCH_LIMIT: usize = 5;

/// The pace after the first [`INITIAL_LAUNCH_LIMIT`]: a large swarm does not
/// hit the provider with every first request at once.
const LAUNCH_INTERVAL: Duration = Duration::from_millis(700);

const DESCRIPTION: &str = "Launch multiple subagents from one prompt template, existing agent resumes, or both.

agent_swarm runs only while swarm mode is on, and only the user can turn swarm mode on. When swarm mode is off, do not call agent_swarm: the call is rejected and no subagents start.

Use agent_swarm when many subagents should run the same kind of task over different inputs. The placeholder is exactly `{{item}}`. For example, with `prompt_template` set to `Review {{item}} for likely regressions.` and `items` set to `[\"src/a.ts\", \"src/b.ts\"]`, agent_swarm launches two new subagents with those two concrete prompts. For a few differently-shaped tasks, make separate `agent` calls in one message instead.

Use `resume_agent_ids` to continue subagents that already exist from earlier work, such as ones that failed or timed out: map each agent id to the prompt for that resumed subagent (usually `continue` if no extra information is needed). You may combine `resume_agent_ids` with `items` in the same call to resume existing subagents and launch new ones. Do not duplicate resumed work in `items`.

Each of these is enforced — a violation is rejected before any subagent starts: provide at least 2 `items` unless you pass `resume_agent_ids`; whenever `items` are present, `prompt_template` is required and must contain `{{item}}`; and the filled-in prompts must be distinct (two items that expand to the same prompt are rejected).

Use enough subagents to keep the work focused and parallel. agent_swarm supports up to 128 subagents, and launches are queued automatically, so it is safe to split large tasks into many clear, independent items.

If `agent_swarm` is called, that call must be the only tool call in the response.";

const FORK_DESCRIPTION: &str = "Context forking: by default, each spawned subagent starts with zero context — brief it through the template. When every item builds on the current conversation, pass `fork: true` instead: each item-spawned subagent then starts with a snapshot of your completed history (inheriting your own agent type, tool set, and model), so the template only needs the task itself. A non-empty `resume_agent_ids` map is rejected with `fork`. If `subagent_type` is provided, it must match your own agent type; if `model` is provided, it must be your own model or `primary`. Different types and model overrides are rejected. Keep `fork` off for independent tasks — it copies the full history into every subagent.";

/// What a call while swarm mode is off returns.
pub const SWARM_MODE_OFF: &str = "Swarm mode is off, so agent_swarm did not run and no subagents were started. Only the user can turn swarm mode on; do not call agent_swarm again until they do. If the user asked for a swarm, tell them swarm mode is off and that they can turn it on with /swarm, run /swarm <task> to start the task as a swarm, or pass --swarm to a -p run. Otherwise, continue without agent_swarm and do not mention swarm mode.";

const FORK_WITH_RESUME_UNAVAILABLE: &str = "Cannot set resume when forking the current context. Fork creates a new agent; resume continues an existing one.";
const FORK_WITH_TYPE_UNAVAILABLE: &str = "Cannot set a different subagent_type when forking the current context. A fork inherits this agent's own agent type.";
const FORK_WITH_MODEL_UNAVAILABLE: &str = "Cannot override the model when forking the current context. A fork inherits this agent's model.";

pub struct AgentSwarmTool {
    tool: Tool,
    registry: Arc<ProfileRegistry>,
    pool: Option<Arc<dyn ModelPool>>,
    /// Whether swarm mode is on: the session agent's, which only the user
    /// turns on (`/swarm`, `--swarm`); off for a tool not given it.
    swarm_mode: Arc<AtomicBool>,
}

impl AgentSwarmTool {
    pub fn new(registry: Arc<ProfileRegistry>) -> Self {
        Self::with_pool(registry, None, "the calling model")
    }

    /// Build the tool with a sub-agent model pool advertised.
    ///
    /// `caller_model` names the model this agent runs on, so `primary` reads as
    /// a real choice rather than a magic word. With no pool the `model`
    /// parameter is omitted entirely: advertising a parameter whose only legal
    /// value is "the same as now" invites the model to reason about a decision
    /// it does not have.
    pub fn with_pool(
        registry: Arc<ProfileRegistry>,
        pool: Option<Arc<dyn ModelPool>>,
        caller_model: &str,
    ) -> Self {
        let mut types = registry.names();
        types.sort();
        let model_property = pool.as_ref().map(|p| {
            json!({
                "type": "string",
                "description": crate::model_pool::summarize_parameter(p.as_ref(), caller_model),
            })
        });
        Self {
            tool: Tool::new(
                AGENT_SWARM_TOOL_NAME,
                format!("{DESCRIPTION}\n\n{FORK_DESCRIPTION}"),
                json!({
                    "type": "object",
                    "properties": {
                        "description": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Short description for the whole swarm."
                        },
                        "subagent_type": {
                            "type": "string",
                            "minLength": 1,
                            "description": format!(
                                "Subagent type used for every new subagent spawned from items; defaults to coder when omitted. Resumed subagents always keep their original type, so passing subagent_type together with resume_agent_ids is allowed — it only affects the item-based spawns. Available types: {}, as the `agent` tool describes them.",
                                types.join(", ")
                            )
                        },
                        "prompt_template": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Prompt template for each subagent. The {{item}} placeholder is replaced with each item value."
                        },
                        "items": {
                            "type": "array",
                            "items": { "type": "string", "minLength": 1 },
                            "maxItems": MAX_AGENT_SWARM_SUBAGENTS,
                            "description": "Values used to fill {{item}}. Each item launches one new subagent."
                        },
                        "fork": {
                            "type": "boolean",
                            "description": "Fork the current context for every item-spawned subagent: each starts with a snapshot of this agent's completed conversation history instead of zero context, inheriting this agent's agent type, tool set, and model. A non-empty resume_agent_ids map is rejected. If subagent_type is provided, it must match this agent's type; if model is provided, it must be this agent's model or \"primary\". Different types and model overrides are rejected. Use it only when every item builds on this conversation; keep independent tasks zero-context."
                        },
                        "resume_agent_ids": {
                            "type": "object",
                            "additionalProperties": { "type": "string", "minLength": 1 },
                            "description": "Map of existing subagent agent_id to the prompt used to resume that subagent. These resumed subagents are launched before new item-based subagents."
                        }
                    },
                    "required": ["description"],
                    "additionalProperties": false
                }),
            )
            .with_optional_property("model", model_property),
            registry,
            pool,
            swarm_mode: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run only while `swarm_mode` is set: the session agent's swarm mode.
    #[must_use]
    pub fn with_swarm_mode(mut self, swarm_mode: Arc<AtomicBool>) -> Self {
        self.swarm_mode = swarm_mode;
        self
    }
}

impl Default for AgentSwarmTool {
    fn default() -> Self {
        Self::new(Arc::new(ProfileRegistry::with_builtin_profiles()))
    }
}

/// One sub-agent of a swarm, as planned.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Spec {
    /// A new sub-agent, for `item`.
    Spawn { item: String, prompt: String },
    /// An existing sub-agent, continued with `prompt`.
    Resume { agent_id: String, prompt: String },
}

impl Spec {
    fn prompt(&self) -> &str {
        match self {
            Self::Spawn { prompt, .. } | Self::Resume { prompt, .. } => prompt,
        }
    }
}

/// The swarm's input, parsed and checked, before anything starts.
#[derive(Debug)]
struct Plan {
    description: String,
    subagent_type: Option<String>,
    prompt_template: Option<String>,
    items: Vec<String>,
    resume: Vec<(String, String)>,
    fork: bool,
    model: Option<String>,
}

/// A string that must be present when the key is.
fn optional_text(input: &Value, key: &str) -> Result<Option<String>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{key} must be a string.")),
    }
}

/// `text`, trimmed; `None` when that leaves nothing.
fn normalized(text: Option<String>) -> Option<String> {
    text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

fn parse(input: &Value) -> Result<Plan, String> {
    let description = normalized(optional_text(input, "description")?).ok_or_else(|| {
        "description is required: a short description for the whole swarm.".to_string()
    })?;
    let subagent_type = normalized(optional_text(input, "subagent_type")?);
    let prompt_template = normalized(optional_text(input, "prompt_template")?);
    let model = normalized(optional_text(input, "model")?);
    let items = match input.get("items") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(|item| item.trim().to_string())
                    .filter(|item| !item.is_empty())
                    .ok_or_else(|| "items must be non-empty strings.".to_string())
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("items must be an array of strings.".to_string()),
    };
    let resume = match input.get("resume_agent_ids") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(agent_id, prompt)| {
                let agent_id = agent_id.trim().to_string();
                let prompt = prompt
                    .as_str()
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| {
                        format!("resume_agent_ids[{agent_id:?}] must be a non-empty prompt.")
                    })?;
                if agent_id.is_empty() {
                    return Err("resume_agent_ids keys must be agent ids.".to_string());
                }
                Ok((agent_id, prompt))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err("resume_agent_ids must map agent ids to prompts.".to_string());
        }
    };
    let fork = match input.get("fork") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(fork)) => *fork,
        Some(_) => return Err("fork must be a boolean.".to_string()),
    };
    Ok(Plan {
        description,
        subagent_type,
        prompt_template,
        items,
        resume,
        fork,
        model,
    })
}

/// Why a fork cannot take these arguments, if it cannot. A fork inherits the
/// caller's type and model, so naming another is refused.
fn fork_incompatibility(plan: &Plan, ctx: &ExecutableToolContext) -> Option<&'static str> {
    if let Some(requested) = &plan.subagent_type
        && Some(requested) != ctx.profile.as_ref()
    {
        return Some(FORK_WITH_TYPE_UNAVAILABLE);
    }
    if let Some(model) = &plan.model {
        let own = ctx.llm.as_ref().map(|llm| llm.model_name());
        if !model.eq_ignore_ascii_case(PRIMARY_ALIAS) && Some(model.as_str()) != own {
            return Some(FORK_WITH_MODEL_UNAVAILABLE);
        }
    }
    None
}

/// The swarm's members, resumes first, with every check in a fixed order.
fn specs(plan: &Plan) -> Result<Vec<Spec>, String> {
    let total = plan.items.len() + plan.resume.len();
    if plan.resume.is_empty() && plan.items.len() < 2 {
        // The rule, then what this call gave: a call that left
        // `items` out was told only that it needs two.
        let given = match plan.items.len() {
            0 => "This call has no items: give `items`, one value per subagent, each \
                  filling {{item}} in prompt_template."
                .to_string(),
            n => format!("This call has {n}."),
        };
        return Err(format!(
            "agent_swarm requires at least 2 items unless resume_agent_ids is provided. {given}"
        ));
    }
    if total > MAX_AGENT_SWARM_SUBAGENTS {
        return Err(format!(
            "agent_swarm supports at most {MAX_AGENT_SWARM_SUBAGENTS} subagents."
        ));
    }
    if !plan.items.is_empty() && plan.prompt_template.is_none() {
        return Err("prompt_template is required when items are provided.".to_string());
    }
    if let Some(template) = &plan.prompt_template
        && !template.contains(PROMPT_TEMPLATE_PLACEHOLDER)
    {
        return Err(format!(
            "prompt_template must include the {PROMPT_TEMPLATE_PLACEHOLDER} placeholder."
        ));
    }

    let mut specs: Vec<Spec> = plan
        .resume
        .iter()
        .map(|(agent_id, prompt)| Spec::Resume {
            agent_id: agent_id.clone(),
            prompt: prompt.clone(),
        })
        .collect();
    if let Some(template) = &plan.prompt_template {
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (index, item) in plan.items.iter().enumerate() {
            let prompt = template.replace(PROMPT_TEMPLATE_PLACEHOLDER, item);
            if let Some(previous) = seen.get(&prompt) {
                return Err(format!(
                    "Duplicate subagent prompts from items {previous} and {}. agent_swarm requires distinct subagents.",
                    index + 1
                ));
            }
            seen.insert(prompt.clone(), index + 1);
            specs.push(Spec::Spawn {
                item: item.clone(),
                prompt,
            });
        }
    }
    Ok(specs)
}

/// How one member ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Completed,
    Failed,
    Aborted,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

/// One member's result, as the report shows it.
#[derive(Debug, Clone)]
struct MemberResult {
    resumed: bool,
    agent_id: Option<String>,
    item: Option<String>,
    status: Status,
    /// Whether it ran at all; reported for a member that did not complete.
    started: bool,
    stop_reason: Option<String>,
    body: String,
}

impl MemberResult {
    fn not_started(
        spec: &Spec,
        agent_id: Option<String>,
        item: Option<String>,
        error: String,
    ) -> Self {
        Self {
            resumed: matches!(spec, Spec::Resume { .. }),
            agent_id,
            item,
            status: Status::Failed,
            started: false,
            stop_reason: None,
            body: error,
        }
    }
}

/// The member's result from how its run ended.
fn classify(outcome: RunOutcome, agent: &Agent) -> (Status, Option<String>, String) {
    let last = agent.last_assistant_text();
    if let Some(limit) = outcome.timed_out {
        let last = last
            .map(|text| format!("\nIts last message:\n{text}"))
            .unwrap_or_default();
        return (
            Status::Failed,
            Some(StopReason::TimedOut.as_str().to_string()),
            format!(
                "The subagent ran past its {} s time limit and was cancelled; its conversation so far is saved.{last}",
                limit.as_secs()
            ),
        );
    }
    match outcome.result {
        Ok(step) => (
            Status::Completed,
            step.stop_reason.map(|reason| reason.as_str().to_string()),
            last.unwrap_or_else(|| "(subagent returned no text)".to_string()),
        ),
        Err(AgentError::Interrupted) => (
            Status::Aborted,
            Some(StopReason::Cancelled.as_str().to_string()),
            "The subagent was cancelled before it finished.".to_string(),
        ),
        Err(e) => (Status::Failed, None, format!("subagent failed: {e}")),
    }
}

/// `value` safe inside a double-quoted XML attribute.
fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The `<agent_swarm_result>` report: each member's outcome, in order.
fn render(results: &[MemberResult]) -> String {
    let count = |status| results.iter().filter(|r| r.status == status).count();
    let summary: Vec<String> = [Status::Completed, Status::Failed, Status::Aborted]
        .into_iter()
        .filter(|status| count(*status) > 0)
        .map(|status| format!("{}: {}", status.as_str(), count(status)))
        .collect();
    let unfinished = results
        .iter()
        .any(|r| r.status != Status::Completed || r.stop_reason.is_some());
    let resumable = results.iter().any(|r| r.agent_id.is_some());

    let mut lines = vec![
        "<agent_swarm_result>".to_string(),
        format!("<summary>{}</summary>", summary.join(", ")),
    ];
    if unfinished && resumable {
        lines.push(
            "<resume_hint>Call agent_swarm with resume_agent_ids using the agent_id values in this result to continue unfinished work.</resume_hint>"
                .to_string(),
        );
    }
    for result in results {
        let mode = if result.resumed {
            r#" mode="resume""#
        } else {
            ""
        };
        let agent_id = result
            .agent_id
            .as_deref()
            .map(|id| format!(r#" agent_id="{id}""#))
            .unwrap_or_default();
        let item = result
            .item
            .as_deref()
            .map(|item| format!(r#" item="{}""#, escape_attribute(item)))
            .unwrap_or_default();
        let state = if result.status == Status::Completed {
            String::new()
        } else if result.started {
            r#" state="started""#.to_string()
        } else {
            r#" state="not_started""#.to_string()
        };
        let stop_reason = result
            .stop_reason
            .as_deref()
            .map(|reason| format!(r#" stop_reason="{}""#, escape_attribute(reason)))
            .unwrap_or_default();
        lines.push(format!(
            r#"<subagent{mode}{agent_id}{item}{state} outcome="{}"{stop_reason}>{}</subagent>"#,
            result.status.as_str(),
            result.body
        ));
    }
    lines.push("</agent_swarm_result>".to_string());
    lines.join("\n")
}

/// A member ready to run, or the reason it cannot.
enum Prepared {
    Ready {
        agent: Box<Agent>,
        agent_id: Option<String>,
        item: Option<String>,
    },
    Unavailable(MemberResult),
}

impl AgentSwarmTool {
    /// Build one member: a resumed sub-agent as it was, a new one under the
    /// swarm's type or as a fork, each with an id and records.
    async fn prepare(
        &self,
        spec: &Spec,
        ctx: &ExecutableToolContext,
        plan: &Plan,
        fork_source: Option<&ForkSource>,
    ) -> Prepared {
        match spec {
            Spec::Resume { agent_id, .. } => {
                let path = subagent_records_path(ctx.background.data_dir(), agent_id);
                if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    return Prepared::Unavailable(MemberResult::not_started(
                        spec,
                        Some(agent_id.clone()),
                        None,
                        format!("Subagent \"{agent_id}\" does not exist."),
                    ));
                }
                let meta = SubagentMeta::load(&path).await;
                let item = meta.as_ref().and_then(|m| m.swarm_item.clone());
                let fail = |error: String| {
                    Prepared::Unavailable(MemberResult::not_started(
                        spec,
                        Some(agent_id.clone()),
                        item.clone(),
                        error,
                    ))
                };
                // A resumed sub-agent keeps its own type and model; one
                // recorded before descriptions were kept resumes as `default`
                // on the caller's model.
                let profile_name = meta
                    .as_ref()
                    .map_or("default".to_string(), |m| m.profile.clone());
                let model = meta
                    .as_ref()
                    .and_then(|m| m.model.clone())
                    .unwrap_or_else(|| PRIMARY_ALIAS.to_string());
                let (llm, _) = match bind_model(ctx, self.pool.as_deref(), Some(&model)) {
                    Ok(bound) => bound,
                    Err(e) => return fail(e),
                };
                // A fork resumes under the instructions it ran under.
                let fork_instructions = meta.as_ref().and_then(|m| m.fork_instructions.clone());
                let mut agent = if let Some(instructions) = fork_instructions {
                    fork_agent(ctx, instructions, Some(profile_name), llm)
                } else {
                    let Some(profile) = self.registry.resolve(&profile_name) else {
                        return fail(format!("Unknown agent type: \"{profile_name}\""));
                    };
                    spawn_with_profile(ctx, &profile, llm)
                };
                agent.name_subagent(Some(agent_id.clone()));
                match agent.with_resumed_records(Records::new(path)).await {
                    Ok(agent) => Prepared::Ready {
                        agent: Box::new(agent),
                        agent_id: Some(agent_id.clone()),
                        item,
                    },
                    Err(e) => fail(format!("failed to resume subagent records: {e}")),
                }
            }
            Spec::Spawn { item, .. } => {
                let (llm, alias) =
                    match bind_model(ctx, self.pool.as_deref(), plan.model.as_deref()) {
                        Ok(bound) => bound,
                        Err(e) => {
                            return Prepared::Unavailable(MemberResult::not_started(
                                spec,
                                None,
                                Some(item.clone()),
                                e,
                            ));
                        }
                    };
                // A fork runs as its caller; any other member under the
                // swarm's type.
                enum Member<'a, P> {
                    Fork(&'a ForkSource),
                    Typed(P),
                }
                let member = match fork_source {
                    Some(source) => Member::Fork(source),
                    None => {
                        let profile_name = plan
                            .subagent_type
                            .clone()
                            .unwrap_or_else(|| DEFAULT_SUBAGENT_TYPE.to_string());
                        let Some(profile) = self.registry.resolve(&profile_name) else {
                            return Prepared::Unavailable(MemberResult::not_started(
                                spec,
                                None,
                                Some(item.clone()),
                                format!("Unknown agent type: \"{profile_name}\""),
                            ));
                        };
                        Member::Typed(profile)
                    }
                };
                let agent_id = new_agent_id();
                let path = subagent_records_path(ctx.background.data_dir(), &agent_id);
                let unavailable = |error: String| {
                    Prepared::Unavailable(MemberResult::not_started(
                        spec,
                        Some(agent_id.clone()),
                        Some(item.clone()),
                        error,
                    ))
                };
                if let Some(parent) = path.parent()
                    && let Err(e) = tokio::fs::create_dir_all(parent).await
                {
                    return unavailable(format!("failed to create subagent records dir: {e}"));
                }
                // A fork keeps its caller's instructions, so that it can be
                // read back and resumed as it ran.
                let (meta, mut agent) = match member {
                    Member::Fork(source) => (
                        fork_meta(ctx, source, alias, Some(item.clone())),
                        spawn_fork(ctx, source.clone(), llm),
                    ),
                    Member::Typed(profile) => (
                        SubagentMeta {
                            profile: profile.name.clone(),
                            model: alias,
                            swarm_item: Some(item.clone()),
                            fork_instructions: None,
                        },
                        spawn_with_profile(ctx, &profile, llm),
                    ),
                };
                if let Err(e) = meta.save(&path).await {
                    return unavailable(e);
                }
                agent.name_subagent(Some(agent_id.clone()));
                Prepared::Ready {
                    agent: Box::new(agent.with_records(Records::new(path))),
                    agent_id: Some(agent_id),
                    item: Some(item.clone()),
                }
            }
        }
    }
}

#[async_trait]
impl ExecutableTool for AgentSwarmTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, _input: &Value) -> ToolAccesses {
        // The swarm is the step's only work: nothing else in the batch runs
        // alongside it.
        ToolAccesses::all()
    }

    fn approval_rule(&self, _input: &Value) -> &str {
        "silent"
    }

    /// Only the user starts a swarm, never the agent on its own. A call
    /// outside swarm mode is refused before its PreToolUse hooks run.
    fn preflight(&self, _input: &Value, _ctx: &ExecutableToolContext) -> Result<(), String> {
        if self.swarm_mode.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(SWARM_MODE_OFF.to_string())
        }
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        // Swarm mode can end between the preflight and the run, so it is
        // checked again.
        if !self.swarm_mode.load(Ordering::SeqCst) {
            return ExecutableToolResult::error(SWARM_MODE_OFF);
        }
        if let Some(refusal) = super::agent::depth_refusal(ctx) {
            return refusal;
        }
        let plan = match parse(&input) {
            Ok(plan) => plan,
            Err(e) => return ExecutableToolResult::error(e),
        };
        if plan.fork && !plan.resume.is_empty() {
            return ExecutableToolResult::error(FORK_WITH_RESUME_UNAVAILABLE);
        }
        let mut fork_source = None;
        if !plan.items.is_empty() {
            if plan.fork {
                if let Some(reason) = fork_incompatibility(&plan, ctx) {
                    return ExecutableToolResult::error(reason);
                }
                // The loop fills this for the batch that asked to fork; empty
                // means there is no conversation to inherit.
                fork_source = ctx.fork_source.lock().unwrap().clone();
                if fork_source.is_none() {
                    return ExecutableToolResult::error(
                        "fork is unavailable here: there is no conversation to inherit. \
                         Launch without `fork` and brief each subagent through the template.",
                    );
                }
            } else {
                let profile_name = plan
                    .subagent_type
                    .as_deref()
                    .unwrap_or(DEFAULT_SUBAGENT_TYPE);
                if self.registry.resolve(profile_name).is_none() {
                    return ExecutableToolResult::error(format!(
                        "Unknown agent type: \"{profile_name}\""
                    ));
                }
            }
            if let Err(e) = bind_model(ctx, self.pool.as_deref(), plan.model.as_deref()) {
                return ExecutableToolResult::error(e);
            }
        }
        let specs = match specs(&plan) {
            Ok(specs) => specs,
            Err(e) => return ExecutableToolResult::error(e),
        };
        tracing::info!(
            "agent_swarm {:?}: {} subagent(s)",
            plan.description,
            specs.len()
        );

        // Members start the first few at once, then one per interval,
        // `swarm_max_concurrency` at a time, each within `swarm_timeout_ms`.
        // Results keep the members' order.
        let limit = ctx.swarm_timeout;
        let concurrency = ctx.swarm_max_concurrency.unwrap_or(specs.len()).max(1);
        let started_at = tokio::time::Instant::now();
        let plan = &plan;
        let fork_source = fork_source.as_ref();
        let mut results: Vec<(usize, MemberResult)> =
            futures::stream::iter(specs.into_iter().enumerate())
                .map(|(index, spec)| async move {
                    let spec = &spec;
                    let delay = LAUNCH_INTERVAL
                        * u32::try_from(index.saturating_sub(INITIAL_LAUNCH_LIMIT - 1))
                            .unwrap_or(u32::MAX);
                    tokio::time::sleep_until(started_at + delay).await;
                    let (mut agent, agent_id, item) =
                        match self.prepare(spec, ctx, plan, fork_source).await {
                            Prepared::Ready {
                                agent,
                                agent_id,
                                item,
                            } => (agent, agent_id, item),
                            Prepared::Unavailable(result) => return (index, result),
                        };
                    let Some(llm) = agent.llm() else {
                        return (
                            index,
                            MemberResult::not_started(
                                spec,
                                agent_id,
                                item,
                                "No LLM is available to run the subagent".to_string(),
                            ),
                        );
                    };
                    let cancel = CancellationToken::new();
                    let outcome =
                        run_bounded(&mut agent, llm.as_ref(), spec.prompt(), &cancel, limit).await;
                    let (status, stop_reason, body) = classify(outcome, &agent);
                    (
                        index,
                        MemberResult {
                            resumed: matches!(spec, Spec::Resume { .. }),
                            agent_id,
                            item,
                            status,
                            started: true,
                            stop_reason,
                            body,
                        },
                    )
                })
                .buffer_unordered(concurrency)
                .collect()
                .await;
        results.sort_by_key(|(index, _)| *index);
        let results: Vec<MemberResult> = results.into_iter().map(|(_, result)| result).collect();
        ExecutableToolResult::text(render(&results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(input: Value) -> Result<Plan, String> {
        parse(&input)
    }

    fn specs_for(input: Value) -> Result<Vec<Spec>, String> {
        specs(&plan(input)?)
    }

    /// The swarm runs only while the switch it is given is on, which only
    /// the user turns on; a tool given none is off.
    #[tokio::test]
    async fn the_swarm_runs_only_while_swarm_mode_is_on() {
        let ctx = ExecutableToolContext::default();
        let input = json!({ "description": "d", "items": ["a"] });
        let off = AgentSwarmTool::default();
        assert_eq!(off.preflight(&input, &ctx), Err(SWARM_MODE_OFF.to_string()));
        assert_eq!(
            off.execute(input.clone(), &ctx, "tc").await.text_content(),
            SWARM_MODE_OFF
        );

        let switch = Arc::new(AtomicBool::new(true));
        let tool = AgentSwarmTool::default().with_swarm_mode(Arc::clone(&switch));
        assert_eq!(tool.preflight(&input, &ctx), Ok(()));
        switch.store(false, Ordering::SeqCst);
        assert!(
            tool.preflight(&input, &ctx).is_err(),
            "it follows the switch"
        );
        assert_eq!(
            tool.execute(input, &ctx, "tc").await.text_content(),
            SWARM_MODE_OFF
        );
    }

    #[test]
    fn the_schema_names_every_parameter() {
        let tool = AgentSwarmTool::default();
        let definition = tool.tool_definition();
        assert_eq!(definition.name, "agent_swarm");
        let properties = definition.parameters["properties"].as_object().unwrap();
        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "description",
                "fork",
                "items",
                "prompt_template",
                "resume_agent_ids",
                "subagent_type"
            ]
        );
        assert_eq!(definition.parameters["required"], json!(["description"]));
        assert_eq!(definition.parameters["additionalProperties"], json!(false));
        assert_eq!(properties["items"]["maxItems"], json!(128));
        assert!(
            definition
                .description
                .contains("The placeholder is exactly `{{item}}`")
        );
        assert!(
            definition
                .description
                .contains("that call must be the only tool call")
        );
        assert!(
            definition
                .description
                .starts_with("Launch multiple subagents")
        );
    }

    #[test]
    fn items_fill_the_template_after_the_resumes() {
        let specs = specs_for(json!({
            "description": "review",
            "prompt_template": "Review {{item}} for regressions.",
            "items": [" src/a.rs ", "src/b.rs"],
            "resume_agent_ids": {"sub-1": "continue"}
        }))
        .unwrap();
        assert_eq!(
            specs,
            [
                Spec::Resume {
                    agent_id: "sub-1".into(),
                    prompt: "continue".into()
                },
                Spec::Spawn {
                    item: "src/a.rs".into(),
                    prompt: "Review src/a.rs for regressions.".into()
                },
                Spec::Spawn {
                    item: "src/b.rs".into(),
                    prompt: "Review src/b.rs for regressions.".into()
                },
            ]
        );
    }

    /// Every rule enforced before a subagent starts, with its wording.
    #[test]
    fn the_checks_reject_before_anything_starts() {
        let reject = |input: Value| specs_for(input).unwrap_err();
        assert_eq!(
            reject(json!({"description": "d", "prompt_template": "{{item}}", "items": ["a"]})),
            "agent_swarm requires at least 2 items unless resume_agent_ids is provided. \
             This call has 1."
        );
        assert_eq!(
            reject(json!({"description": "d", "prompt_template": "{{item}}"})),
            "agent_swarm requires at least 2 items unless resume_agent_ids is provided. \
             This call has no items: give `items`, one value per subagent, each filling \
             {{item}} in prompt_template."
        );
        assert_eq!(
            reject(json!({"description": "d", "items": ["a", "b"]})),
            "prompt_template is required when items are provided."
        );
        assert_eq!(
            reject(json!({"description": "d", "prompt_template": "Check it", "items": ["a", "b"]})),
            "prompt_template must include the {{item}} placeholder."
        );
        assert_eq!(
            reject(
                json!({"description": "d", "prompt_template": "Same task", "resume_agent_ids": {"sub-1": "go"}})
            ),
            "prompt_template must include the {{item}} placeholder."
        );
        assert_eq!(
            reject(
                json!({"description": "d", "prompt_template": "Check {{item}}", "items": ["a", "b", " a "]})
            ),
            "Duplicate subagent prompts from items 1 and 3. agent_swarm requires distinct subagents."
        );
        let many: Vec<String> = (0..129).map(|n| n.to_string()).collect();
        assert_eq!(
            reject(json!({"description": "d", "prompt_template": "{{item}}", "items": many})),
            "agent_swarm supports at most 128 subagents."
        );
        assert!(
            plan(json!({"prompt_template": "{{item}}"}))
                .unwrap_err()
                .starts_with("description is required")
        );
        assert_eq!(
            plan(json!({"description": "d", "items": ["a", ""]})).unwrap_err(),
            "items must be non-empty strings."
        );
        // A resume alone is enough.
        assert_eq!(
            specs_for(json!({"description": "d", "resume_agent_ids": {"sub-1": "continue"}}))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_fork_keeps_its_callers_type_and_model() {
        let fork = |input: Value| plan(input).unwrap();
        let main = ExecutableToolContext::default();
        assert_eq!(
            fork_incompatibility(
                &fork(json!({"description": "d", "subagent_type": "coder"})),
                &main
            ),
            Some(FORK_WITH_TYPE_UNAVAILABLE)
        );
        let coder = ExecutableToolContext {
            profile: Some("coder".into()),
            ..Default::default()
        };
        assert_eq!(
            fork_incompatibility(
                &fork(json!({"description": "d", "subagent_type": "coder"})),
                &coder
            ),
            None
        );
        assert_eq!(
            fork_incompatibility(&fork(json!({"description": "d", "model": "fast"})), &coder),
            Some(FORK_WITH_MODEL_UNAVAILABLE)
        );
        assert_eq!(
            fork_incompatibility(
                &fork(json!({"description": "d", "model": "primary"})),
                &coder
            ),
            None
        );
    }

    #[test]
    fn the_report_lists_every_member() {
        let results = [
            MemberResult {
                resumed: true,
                agent_id: Some("sub-1".into()),
                item: Some("src/\"a\".rs".into()),
                status: Status::Completed,
                started: true,
                stop_reason: None,
                body: "done".into(),
            },
            MemberResult {
                resumed: false,
                agent_id: Some("sub-2".into()),
                item: Some("b".into()),
                status: Status::Failed,
                started: true,
                stop_reason: Some("timed_out".into()),
                body: "ran out of time".into(),
            },
            MemberResult {
                resumed: false,
                agent_id: None,
                item: Some("c".into()),
                status: Status::Failed,
                started: false,
                stop_reason: None,
                body: "Unknown agent type: \"x\"".into(),
            },
        ];
        assert_eq!(
            render(&results),
            [
                "<agent_swarm_result>",
                "<summary>completed: 1, failed: 2</summary>",
                "<resume_hint>Call agent_swarm with resume_agent_ids using the agent_id values in this result to continue unfinished work.</resume_hint>",
                r#"<subagent mode="resume" agent_id="sub-1" item="src/&quot;a&quot;.rs" outcome="completed">done</subagent>"#,
                r#"<subagent agent_id="sub-2" item="b" state="started" outcome="failed" stop_reason="timed_out">ran out of time</subagent>"#,
                r#"<subagent item="c" state="not_started" outcome="failed">Unknown agent type: "x"</subagent>"#,
                "</agent_swarm_result>",
            ]
            .join("\n")
        );
        // All finished: no resume hint.
        let done = [MemberResult {
            status: Status::Completed,
            stop_reason: None,
            ..results[1].clone()
        }];
        assert!(!render(&done).contains("<resume_hint>"));
    }
}
