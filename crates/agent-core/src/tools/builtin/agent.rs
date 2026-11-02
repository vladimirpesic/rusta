use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::subagent::{
    SubagentMeta, bind_model, fork_agent, fork_meta, spawn_fork, spawn_with_profile,
};
use crate::{
    Agent, ProfileRegistry,
    agent_loop::types::{
        ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
    },
    model_pool::ModelPool,
    records::Records,
};

/// What the `agent` tool does, before the agent types it lists.
const AGENT_DESCRIPTION: &str = "Spawn a focused subagent with an isolated context to investigate or complete a task. The subagent returns a concise text report.

Do not launch a subagent unless the user explicitly asks for one. Each subagent starts cold and re-derives context you already have, which costs the user time and tokens. Asking means: the user asks for a subagent, delegation, or parallel agents, or requests one of the agent types below by name (for example \"use an explore agent\"). Explicit instructions in AGENTS.md or a skill, and swarm mode when the user turned it on, also count. Nothing else does: a large or multi-part task, or a request for depth, thoroughness, research, or investigation, is not a request to launch — do that work yourself with your own tools. The agent type descriptions below only help you choose a type once launching is authorized; they never authorize it. Resuming a subagent to continue already-authorized work needs no new request.";

pub struct AgentTool {
    tool: Tool,
    registry: Arc<ProfileRegistry>,
    pool: Option<Arc<dyn ModelPool>>,
}

impl AgentTool {
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
        let profile_names = registry.names().join(", ");
        let types = registry.describe_types();
        let description = if types.is_empty() {
            AGENT_DESCRIPTION.to_string()
        } else {
            format!("{AGENT_DESCRIPTION}\n\nAvailable agent types (pass via `profile`):\n{types}")
        };
        let model_property = pool.as_ref().map(|p| {
            json!({
                "type": "string",
                "description": crate::model_pool::describe_parameter(p.as_ref(), caller_model),
            })
        });
        Self {
            tool: Tool::new(
                "agent",
                description,
                json!({
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The task or question to give the subagent"
                        },
                        "profile": {
                            "type": "string",
                            "description": format!("The agent type, from those described above: {profile_names}")
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "Optional stable id for the subagent. Used to resume and persist records."
                        },
                        "resume": {
                            "type": "boolean",
                            "description": "If true and agent_id is set, resume the subagent from its persisted records, as the type and on the model it was started with."
                        },
                        "run_in_background": {
                            "type": "boolean",
                            "description": "If true, detach the subagent and return a task id immediately."
                        },
                        "fork": {
                            "type": "boolean",
                            "description": "If true, the subagent starts from a snapshot of THIS conversation instead of an empty context, inheriting this agent's instructions, tools and model. Use it when the task needs what has already been established here and re-explaining it would be lossy or long. The snapshot is one-time: the two conversations do not stay in sync. Incompatible with `resume`, `profile` and `model` — a fork inherits rather than delegates."
                        }
                    },
                    "required": ["prompt"]
                }),
            )
            .with_optional_property("model", model_property),
            registry,
            pool,
        }
    }
}

/// Refuse to spawn when this agent is already at the nesting bound.
///
/// A sub-agent inherits the full built-in tool set, `agent` included, so nothing
/// but this stops the recursion. The message names the bound and tells the model
/// to do the work itself, because a refusal with no alternative just gets
/// retried.
pub(crate) fn depth_refusal(ctx: &ExecutableToolContext) -> Option<ExecutableToolResult> {
    if ctx.depth < ctx.max_subagent_depth {
        return None;
    }
    Some(ExecutableToolResult::error(format!(
        "Cannot delegate: this agent is already {} sub-agent level(s) deep, which is the limit \
         (max_subagent_depth = {}). Do this work yourself with the tools you have, or return what \
         you have so the agent that spawned you can decide.",
        ctx.depth, ctx.max_subagent_depth
    )))
}

/// Why this fork request cannot be honoured, if it cannot.
///
/// A fork inherits the caller's instructions, tools and model, so an argument
/// asking for a *different* one contradicts it. Refusing is better than picking
/// a winner: either choice would silently give the model something other than
/// what it asked for.
fn fork_conflict(input: &serde_json::Value) -> Option<String> {
    let asked = |key: &str| input.get(key).and_then(serde_json::Value::as_str);

    if input
        .get("resume")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Some(
            "`fork` and `resume` are incompatible: a fork starts from this conversation, a \
             resume from a previous subagent's own records. Pick one."
                .to_string(),
        );
    }
    if let Some(profile) = asked("profile") {
        return Some(format!(
            "`fork` inherits this agent's own instructions, so it cannot also take profile \
             {profile:?}. Drop `profile` to fork, or drop `fork` to delegate to that profile."
        ));
    }
    if let Some(model) = asked("model")
        && !model.trim().is_empty()
        && !model.eq_ignore_ascii_case(crate::model_pool::PRIMARY_ALIAS)
    {
        return Some(format!(
            "`fork` inherits this agent's own model, so it cannot also take model {model:?}. \
             Drop `model` to fork, or drop `fork` to delegate to that model."
        ));
    }
    None
}

impl Default for AgentTool {
    fn default() -> Self {
        Self::new(Arc::new(ProfileRegistry::with_builtin_profiles()))
    }
}

#[async_trait]
impl ExecutableTool for AgentTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
        // A subagent may read or write anywhere the parent is allowed.
        ToolAccesses::none()
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        "silent"
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        if let Some(refusal) = depth_refusal(ctx) {
            return refusal;
        }
        let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        if prompt.is_empty() {
            return ExecutableToolResult::error("prompt is required");
        }
        let resume = input
            .get("resume")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let run_in_background = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // A fork is this agent continuing under a fresh budget, not a delegation
        // to a different one — so the three arguments that would make it a
        // different agent are refused rather than quietly ignored.
        let fork = input
            .get("fork")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if fork && let Some(reason) = fork_conflict(&input) {
            return ExecutableToolResult::error(reason);
        }
        // Every delegation gets an id and records, a fork too, so that one
        // that is stopped or fails can be resumed, and what it did read back.
        let agent_id: Option<String> = match input.get("agent_id").and_then(|v| v.as_str()) {
            Some(id) => Some(id.to_string()),
            None if resume => {
                return ExecutableToolResult::error(
                    "resume needs the agent_id of the sub-agent to resume.",
                );
            }
            None => Some(new_agent_id()),
        };
        let records_path: Option<PathBuf> = agent_id
            .as_deref()
            .map(|id| subagent_records_path(ctx.background.data_dir(), id));

        // A resumed sub-agent is rebuilt as it was: its profile and model come
        // from its saved description, not from this call.
        let saved = match (&records_path, resume) {
            (Some(path), true) => SubagentMeta::load(path).await,
            _ => None,
        };
        let profile_name = saved
            .as_ref()
            .map(|meta| meta.profile.clone())
            .or_else(|| {
                input
                    .get("profile")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "default".to_string());
        let requested_model: Option<String> = match &saved {
            Some(meta) => Some(
                meta.model
                    .clone()
                    .unwrap_or_else(|| crate::model_pool::PRIMARY_ALIAS.to_string()),
            ),
            None => input
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        };

        // Which model this sub-agent runs on. An unknown alias fails the call
        // rather than quietly falling back to the parent: falling back would
        // bill the flagship for work deliberately routed elsewhere, and say
        // nothing about it.
        let (llm, alias) = match bind_model(ctx, self.pool.as_deref(), requested_model.as_deref()) {
            Ok(bound) => bound,
            Err(e) => return ExecutableToolResult::error(e),
        };

        // Inherit the parent's permission mode rather than forcing bypass. A
        // sub-agent must never be more privileged than the agent that spawned it:
        // forcing bypass let a sub-agent write and run commands unchecked even
        // while the parent was gating every such action. The parent's mode flows
        // in via the context and stays current across runtime `/yolo` switches.
        let mut fork_source = None;
        let mut agent = if let Some(instructions) = saved
            .as_ref()
            .and_then(|meta| meta.fork_instructions.clone())
        {
            // A fork resumes under the instructions it ran under.
            fork_agent(ctx, instructions, Some(profile_name.clone()), llm)
        } else if fork {
            let source = match ctx.fork_source.lock().unwrap().clone() {
                Some(source) => source,
                // The loop fills this before running the batch that asked to
                // fork, so an empty slot means the call did not come through a
                // turn — a direct invocation with no conversation to inherit.
                None => {
                    return ExecutableToolResult::error(
                        "fork is unavailable here: there is no conversation to inherit. \
                         Spawn without `fork` and describe the task in the prompt.",
                    );
                }
            };
            let agent = spawn_fork(ctx, source.clone(), llm);
            fork_source = Some(source);
            agent
        } else {
            let Some(profile) = self.registry.resolve(&profile_name) else {
                return ExecutableToolResult::error(format!("unknown profile: {profile_name}"));
            };
            spawn_with_profile(ctx, &profile, llm)
        };
        agent.name_subagent(agent_id.clone());

        if let Some(path) = records_path.clone() {
            if let Some(parent) = path.parent()
                && let Err(e) = tokio::fs::create_dir_all(parent).await
            {
                return ExecutableToolResult::error(format!(
                    "failed to create subagent records dir: {e}"
                ));
            }
            let records = Records::new(path.clone());
            if resume {
                match agent.with_resumed_records(records).await {
                    Ok(a) => agent = a,
                    Err(e) => {
                        return ExecutableToolResult::error(format!(
                            "failed to resume subagent records: {e}"
                        ));
                    }
                }
            } else {
                agent = agent.with_records(records);
                // A fork keeps its caller's instructions, which no profile
                // rebuilds.
                let meta = match &fork_source {
                    Some(source) => fork_meta(ctx, source, alias.clone(), None),
                    None => SubagentMeta {
                        profile: profile_name.clone(),
                        model: alias.clone(),
                        swarm_item: None,
                        fork_instructions: None,
                    },
                };
                if let Err(e) = meta.save(&path).await {
                    return ExecutableToolResult::error(e);
                }
            }
        }

        // The sub-agent answers on the model it was bound to. Prompting it with
        // the caller's model instead would ignore the `model` binding entirely.
        let Some(llm) = agent.llm() else {
            return ExecutableToolResult::error("No LLM is available to run the subagent");
        };

        // The sub-agent deadline (`subagent_timeout_ms`) applies whether the
        // call waits or runs in the background; the run bounds itself so a
        // timed-out sub-agent can still say so, and keep its records.
        let limit = ctx.background.agent_timeout();
        let cancel = CancellationToken::new();
        let prompt = prompt.to_string();
        if run_in_background {
            // Nobody is there to answer it once this turn has moved on: a
            // background sub-agent does not ask, and what would need asking is
            // denied.
            agent.set_approval_hook(Arc::new(crate::permissions::DenyAllHook));
            let label = agent_id.clone().unwrap_or_else(|| "subagent".to_string());
            let task_id = ctx.background.spawn_agent_with_timeout(
                label,
                ctx.cwd.clone(),
                None,
                move |task_cancel| async move {
                    // Run under the subagent's own cancel token, but also respect
                    // the background task's cancel token.
                    let combined = CancellationToken::new();
                    let child = combined.clone();
                    let subagent_cancel = cancel.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = task_cancel.cancelled() => child.cancel(),
                            _ = subagent_cancel.cancelled() => child.cancel(),
                        }
                    });

                    let outcome =
                        run_bounded(&mut agent, llm.as_ref(), &prompt, &combined, limit).await;
                    report(outcome, &agent, agent_id.as_deref())
                },
            );
            return ExecutableToolResult::text(format!(
                "Subagent detached as background task {task_id}. Use task_output to check progress."
            ));
        }

        let outcome = run_bounded(&mut agent, llm.as_ref(), &prompt, &cancel, limit).await;
        match report(outcome, &agent, agent_id.as_deref()) {
            Ok(text) => ExecutableToolResult::text(text),
            Err(text) => ExecutableToolResult::error(text),
        }
    }
}

/// A fresh sub-agent id: `sub-` and a lowercase ULID.
pub(crate) fn new_agent_id() -> String {
    format!("sub-{}", ulid::Ulid::generate().to_string().to_lowercase())
}

/// Where a sub-agent's records live, so `resume` can find them: under the
/// session's data directory (`<data dir>/rusta/subagents/`).
pub(crate) fn subagent_records_path(data_dir: &std::path::Path, id: &str) -> PathBuf {
    data_dir.join("subagents").join(format!("{id}.jsonl"))
}

/// How long a timed-out sub-agent gets to unwind after its cancel token
/// fires, before it is dropped. Unwinding records its last steps, which is
/// what makes it resumable.
pub(crate) const SUBAGENT_CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// How a sub-agent's run ended.
pub(crate) struct RunOutcome {
    pub result: Result<crate::agent_loop::types::StepResult, crate::errors::AgentError>,
    /// The limit it ran past, when it timed out.
    pub timed_out: Option<std::time::Duration>,
}

/// Run a sub-agent's prompt within `limit`. At the deadline its cancel token
/// fires and it gets [`SUBAGENT_CANCEL_GRACE`] to unwind; one that does not
/// is dropped.
pub(crate) async fn run_bounded(
    agent: &mut Agent,
    llm: &dyn crate::agent_loop::Llm,
    prompt: &str,
    cancel: &CancellationToken,
    limit: Option<std::time::Duration>,
) -> RunOutcome {
    let outcome = async {
        let run = agent.prompt(llm, prompt, cancel);
        tokio::pin!(run);
        if let Some(limit) = limit {
            match tokio::time::timeout(limit, &mut run).await {
                Ok(result) => RunOutcome {
                    result,
                    timed_out: None,
                },
                Err(_) => {
                    cancel.cancel();
                    let result = tokio::time::timeout(SUBAGENT_CANCEL_GRACE, &mut run)
                        .await
                        .unwrap_or(Err(crate::errors::AgentError::Interrupted));
                    RunOutcome {
                        result,
                        timed_out: Some(limit),
                    }
                }
            }
        } else {
            RunOutcome {
                result: run.await,
                timed_out: None,
            }
        }
    }
    .await;
    // SubagentStop announces the run, however it ended. The name is the
    // profile a profiled sub-agent runs under, "fork" for a fork, and the
    // fallback for anything else.
    if let Some(hooks) = &agent.tool_ctx.user_hooks {
        let name = agent
            .tool_ctx
            .profile
            .clone()
            .unwrap_or_else(|| "subagent".to_string());
        let mut fields = serde_json::Map::new();
        fields.insert("name".to_string(), serde_json::Value::String(name.clone()));
        let stop_reason = outcome
            .result
            .as_ref()
            .err()
            .map(|e| e.hook_error_kind())
            .or_else(|| outcome.timed_out.map(|_| "timed_out"));
        if let Some(stop_reason) = stop_reason {
            fields.insert(
                "stop_reason".to_string(),
                serde_json::Value::String(stop_reason.to_string()),
            );
        }
        hooks.fire_spawned(crate::user_hooks::UserHookEvent::SubagentStop, name, fields);
    }
    outcome
}

/// What a sub-agent hands back: its final message and, when it was stopped,
/// timed out or failed, why and how to resume it. `Err` is a failure.
pub(crate) fn report(
    outcome: RunOutcome,
    agent: &Agent,
    agent_id: Option<&str>,
) -> Result<String, String> {
    use crate::agent_loop::types::StopReason;
    use crate::errors::AgentError;

    let resume_hint = |why: &str| match agent_id {
        Some(id) => format!(
            "\nagent_id: {id}\nnext_step: The sub-agent {why}. To continue its work, call agent \
             with agent_id=\"{id}\", resume=true and a prompt saying what to do next."
        ),
        None => String::new(),
    };
    if let Some(limit) = outcome.timed_out {
        let last = agent
            .last_assistant_text()
            .map(|text| format!("\nIts last message:\n{text}"))
            .unwrap_or_default();
        return Err(format!(
            "subagent ran past its {} s time limit and was cancelled\nstop_reason: {}{}{last}",
            limit.as_secs(),
            StopReason::TimedOut,
            resume_hint("ran out of time; its conversation so far is saved")
        ));
    }
    match outcome.result {
        Ok(step) => {
            let text = agent
                .last_assistant_text()
                .unwrap_or_else(|| "(subagent returned no text)".to_string());
            Ok(match step.stop_reason {
                None => text,
                Some(reason) => format!(
                    "{text}\n\nstop_reason: {reason}{}",
                    resume_hint(&format!("stopped before finishing: {}", reason.describe()))
                ),
            })
        }
        Err(AgentError::Interrupted) => Err(format!(
            "subagent was cancelled\nstop_reason: {}{}",
            StopReason::Cancelled,
            resume_hint("was cancelled before finishing")
        )),
        Err(e) => Err(format!(
            "subagent failed: {e}{}",
            resume_hint("failed; its conversation so far is saved")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod runs {
        use std::sync::Mutex;

        use crate::tools::builtin::subagent::SubagentMeta;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        use lm_bridge::message::Message;
        use lm_bridge::provider::FinishReason;
        use lm_bridge::usage::TokenUsage;

        use super::*;
        use crate::agent_loop::types::{StepResult, StopReason};
        use crate::agent_loop::{Llm, LlmCallbacks};
        use crate::errors::AgentError;

        /// `agent_swarm` in swarm mode, which only the user turns on and which
        /// it needs.
        fn swarm_on() -> crate::tools::builtin::AgentSwarmTool {
            crate::tools::builtin::AgentSwarmTool::default()
                .with_swarm_mode(Arc::new(std::sync::atomic::AtomicBool::new(true)))
        }

        /// Answers after `delay`, or waits for cancellation when `hang`, counts
        /// how many calls are in flight at once, and keeps the system prompt
        /// of each call.
        struct PacedLlm {
            delay: Duration,
            hang: bool,
            in_flight: AtomicUsize,
            peak: AtomicUsize,
            prompts: Mutex<usize>,
            systems: Mutex<Vec<String>>,
        }

        impl PacedLlm {
            fn new(delay: Duration, hang: bool) -> Arc<Self> {
                Arc::new(Self {
                    delay,
                    hang,
                    in_flight: AtomicUsize::new(0),
                    peak: AtomicUsize::new(0),
                    prompts: Mutex::new(0),
                    systems: Mutex::new(Vec::new()),
                })
            }
        }

        #[async_trait]
        impl Llm for PacedLlm {
            fn model_name(&self) -> &str {
                "paced"
            }

            async fn chat(
                &self,
                messages: Vec<Message>,
                _tools: Vec<Tool>,
                _callbacks: LlmCallbacks,
                cancel: &CancellationToken,
            ) -> Result<StepResult, lm_bridge::errors::LlmError> {
                *self.prompts.lock().unwrap() += 1;
                if let Some(system) = messages
                    .iter()
                    .find(|m| m.role == lm_bridge::message::Role::System)
                {
                    self.systems.lock().unwrap().push(system.text_content());
                }
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                let outcome = if self.hang {
                    cancel.cancelled().await;
                    Err(lm_bridge::errors::LlmError::Cancelled)
                } else {
                    tokio::time::sleep(self.delay).await;
                    Ok(StepResult {
                        message: Message::assistant_text("finished"),
                        usage: TokenUsage::empty(),
                        finish_reason: Some(FinishReason::Stop),
                        response_id: None,
                        stop_reason: None,
                    })
                };
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                outcome
            }
        }

        fn ctx_with(llm: Arc<PacedLlm>, dir: &std::path::Path) -> ExecutableToolContext {
            let background = Arc::new(crate::background::BackgroundManager::with_data_dir(
                dir.to_path_buf(),
            ));
            ExecutableToolContext {
                llm: Some(llm),
                cron: Arc::new(crate::cron::CronManager::new()),
                background,
                cwd: dir.to_path_buf(),
                ..Default::default()
            }
        }

        fn step(stop_reason: Option<StopReason>) -> StepResult {
            StepResult {
                message: Message::assistant_text("handoff"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason,
            }
        }

        fn agent_saying(text: &str) -> Agent {
            let mut agent = Agent::new(std::env::temp_dir(), "sys");
            agent.resume(vec![Message::assistant_text(text)]);
            agent
        }

        #[test]
        fn a_finished_subagent_reports_only_its_answer() {
            let agent = agent_saying("the answer");
            let outcome = RunOutcome {
                result: Ok(step(None)),
                timed_out: None,
            };
            assert_eq!(
                report(outcome, &agent, Some("sub-1")).unwrap(),
                "the answer"
            );
        }

        /// A stopped sub-agent says why, and how to pick its work up again.
        #[test]
        fn a_stopped_subagent_carries_its_stop_reason_and_a_resume_hint() {
            let agent = agent_saying("did A; B is blocked");
            let outcome = RunOutcome {
                result: Ok(step(Some(StopReason::MaxTurns))),
                timed_out: None,
            };
            let text = report(outcome, &agent, Some("sub-1")).unwrap();
            assert!(
                text.starts_with("did A; B is blocked\n\nstop_reason: max_turns"),
                "{text}"
            );
            assert!(text.contains("agent_id: sub-1"), "{text}");
            assert!(
                text.contains(r#"call agent with agent_id="sub-1", resume=true"#),
                "{text}"
            );

            let failed = RunOutcome {
                result: Err(AgentError::Interrupted),
                timed_out: None,
            };
            let text = report(failed, &agent, Some("sub-1")).unwrap_err();
            assert!(text.contains("stop_reason: cancelled"), "{text}");
        }

        /// A sub-agent past its deadline is cancelled, unwinds, and reports
        /// `timed_out`; its records are on disk for a resume.
        #[tokio::test]
        async fn a_subagent_past_its_deadline_times_out_resumably() {
            let dir = tempfile::tempdir().unwrap();
            let llm = PacedLlm::new(Duration::ZERO, true);
            let ctx = ExecutableToolContext {
                background: Arc::new(
                    crate::background::BackgroundManager::with_data_dir(dir.path().to_path_buf())
                        .with_agent_timeout(Some(Duration::from_millis(200))),
                ),
                ..ctx_with(llm, dir.path())
            };

            let result = AgentTool::default()
                .execute(
                    serde_json::json!({ "prompt": "work", "agent_id": "sub-slow" }),
                    &ctx,
                    "tc",
                )
                .await;

            let text = result.text_content();
            assert!(result.is_error, "{text}");
            assert!(text.contains("stop_reason: timed_out"), "{text}");
            assert!(text.contains(r#"agent_id="sub-slow""#), "{text}");
            let records = dir.path().join("subagents").join("sub-slow.jsonl");
            let saved = std::fs::read_to_string(&records).unwrap();
            assert!(saved.contains("work"), "the prompt is on record: {saved}");
        }

        /// Swarm members run at most `swarm_max_concurrency` at a time.
        #[tokio::test]
        async fn swarm_concurrency_is_capped() {
            let dir = tempfile::tempdir().unwrap();
            let llm = PacedLlm::new(Duration::from_millis(80), false);
            let ctx = ExecutableToolContext {
                swarm_max_concurrency: Some(1),
                ..ctx_with(llm.clone(), dir.path())
            };

            let three = serde_json::json!({
                "description": "work",
                "prompt_template": "work on {{item}}",
                "items": ["one", "two", "three"]
            });
            let result = swarm_on().execute(three.clone(), &ctx, "tc").await;

            assert!(!result.is_error, "{}", result.text_content());
            assert_eq!(*llm.prompts.lock().unwrap(), 3);
            assert_eq!(llm.peak.load(Ordering::SeqCst), 1);
            let text = result.text_content();
            assert!(text.contains("<summary>completed: 3</summary>"), "{text}");
            let first = text.find(r#"item="one""#).unwrap();
            let third = text.find(r#"item="three""#).unwrap();
            assert!(first < third, "results keep the members' order: {text}");

            let uncapped = PacedLlm::new(Duration::from_millis(80), false);
            let ctx = ctx_with(uncapped.clone(), dir.path());
            swarm_on().execute(three, &ctx, "tc").await;
            assert_eq!(uncapped.peak.load(Ordering::SeqCst), 3);
        }

        /// A swarm member resumed by id keeps its own type and item, and its
        /// conversation (`resume_agent_ids`).
        #[tokio::test]
        async fn a_swarm_member_resumes_as_what_it_was() {
            let dir = tempfile::tempdir().unwrap();
            let llm = PacedLlm::new(Duration::ZERO, false);
            let ctx = ctx_with(llm.clone(), dir.path());
            let swarm = swarm_on();

            let first = swarm
                .execute(
                    serde_json::json!({
                        "description": "survey",
                        "subagent_type": "explore",
                        "prompt_template": "survey {{item}}",
                        "items": ["services/", "apps/"]
                    }),
                    &ctx,
                    "tc",
                )
                .await
                .text_content();
            let id_start = first.find(r#"agent_id=""#).unwrap() + r#"agent_id=""#.len();
            let agent_id = &first[id_start..id_start + first[id_start..].find('"').unwrap()];
            let meta = SubagentMeta::load(&subagent_records_path(dir.path(), agent_id))
                .await
                .expect("a spawned member keeps a description");
            assert_eq!(meta.profile, "explore");
            assert_eq!(meta.swarm_item.as_deref(), Some("services/"));

            let resumed = swarm
                .execute(
                    serde_json::json!({
                        "description": "survey again",
                        "subagent_type": "coder",
                        "resume_agent_ids": { agent_id: "continue", "sub-missing": "continue" }
                    }),
                    &ctx,
                    "tc",
                )
                .await
                .text_content();
            assert!(
                resumed.contains(&format!(
                    r#"<subagent mode="resume" agent_id="{agent_id}" item="services/" outcome="completed">"#
                )),
                "{resumed}"
            );
            assert!(
                resumed.contains(
                    r#"state="not_started" outcome="failed">Subagent "sub-missing" does not exist."#
                ),
                "{resumed}"
            );
            let records =
                std::fs::read_to_string(subagent_records_path(dir.path(), agent_id)).unwrap();
            assert!(
                records.contains("survey services/"),
                "the first run is kept: {records}"
            );
            assert!(
                records.contains("continue"),
                "the resume is added: {records}"
            );
            let meta = SubagentMeta::load(&subagent_records_path(dir.path(), agent_id))
                .await
                .unwrap();
            assert_eq!(meta.profile, "explore", "a resume does not retype it");
        }

        /// The context of a caller that forks: its instructions and a turn.
        fn forking(ctx: &ExecutableToolContext) {
            *ctx.fork_source.lock().unwrap() = Some(crate::agent_loop::types::ForkSource {
                system_prompt: "the caller's instructions".into(),
                history: vec![Message::user("the bug is in the retry loop")],
            });
        }

        /// Forked swarm members keep records and a description like other
        /// members, so what they did can be read back, and a resumed one runs
        /// under the caller's instructions again.
        #[tokio::test]
        async fn forked_swarm_members_keep_records_and_resume_as_forks() {
            let dir = tempfile::tempdir().unwrap();
            let llm = PacedLlm::new(Duration::ZERO, false);
            let ctx = ExecutableToolContext {
                profile: Some("coder".into()),
                ..ctx_with(llm.clone(), dir.path())
            };
            forking(&ctx);
            let swarm = swarm_on();

            let first = swarm
                .execute(
                    serde_json::json!({
                        "description": "fix",
                        "prompt_template": "fix {{item}}",
                        "items": ["retry.rs", "backoff.rs"],
                        "fork": true
                    }),
                    &ctx,
                    "tc",
                )
                .await
                .text_content();
            let ids: Vec<&str> = first
                .split(r#"agent_id=""#)
                .skip(1)
                .map(|rest| &rest[..rest.find('"').unwrap()])
                .collect();
            assert_eq!(ids.len(), 2, "every forked member has an id: {first}");
            for (id, item) in ids.iter().zip(["retry.rs", "backoff.rs"]) {
                let path = subagent_records_path(dir.path(), id);
                let records = std::fs::read_to_string(&path).unwrap();
                assert!(records.contains(&format!("fix {item}")), "{records}");
                let meta = SubagentMeta::load(&path).await.unwrap();
                assert_eq!(meta.profile, "coder");
                assert_eq!(meta.swarm_item.as_deref(), Some(item));
                assert_eq!(
                    meta.fork_instructions.as_deref(),
                    Some("the caller's instructions")
                );
            }

            llm.systems.lock().unwrap().clear();
            let resumed = swarm
                .execute(
                    serde_json::json!({
                        "description": "again",
                        "resume_agent_ids": { ids[0]: "continue" }
                    }),
                    &ctx,
                    "tc",
                )
                .await
                .text_content();
            assert!(
                resumed.contains(&format!(r#"mode="resume" agent_id="{}""#, ids[0])),
                "{resumed}"
            );
            let systems = llm.systems.lock().unwrap().clone();
            assert!(!systems.is_empty(), "the resumed fork ran");
            assert!(
                systems
                    .iter()
                    .all(|system| system.contains("the caller's instructions")),
                "{systems:?}"
            );
        }

        /// A fork made by `agent` without an id gets one, with records and a
        /// description.
        #[tokio::test]
        async fn an_agent_fork_keeps_records_without_a_given_id() {
            let dir = tempfile::tempdir().unwrap();
            let llm = PacedLlm::new(Duration::ZERO, false);
            let ctx = ctx_with(llm, dir.path());
            forking(&ctx);
            let result = AgentTool::default()
                .execute(
                    serde_json::json!({"prompt": "check the retry loop", "fork": true}),
                    &ctx,
                    "tc",
                )
                .await;
            assert!(!result.is_error, "{}", result.text_content());

            let records: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path().join("subagents"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect();
            assert_eq!(records.len(), 1, "{records:?}");
            let saved = std::fs::read_to_string(&records[0]).unwrap();
            assert!(saved.contains("check the retry loop"), "{saved}");
            let meta = SubagentMeta::load(&records[0]).await.unwrap();
            assert_eq!(
                meta.fork_instructions.as_deref(),
                Some("the caller's instructions")
            );
        }

        /// `agent` resumes a sub-agent as the type it was started as, whatever
        /// the resuming call names; a new one is described for that.
        #[tokio::test]
        async fn an_agent_resume_keeps_the_saved_profile() {
            let dir = tempfile::tempdir().unwrap();
            let llm = PacedLlm::new(Duration::ZERO, false);
            let ctx = ctx_with(llm, dir.path());
            let tool = AgentTool::default();
            tool.execute(
                serde_json::json!({"prompt": "look", "profile": "explore", "agent_id": "sub-x"}),
                &ctx,
                "tc",
            )
            .await;
            let path = subagent_records_path(dir.path(), "sub-x");
            assert_eq!(SubagentMeta::load(&path).await.unwrap().profile, "explore");
            let resumed = tool
                .execute(
                    serde_json::json!({"prompt": "go on", "agent_id": "sub-x", "resume": true}),
                    &ctx,
                    "tc",
                )
                .await;
            assert!(!resumed.is_error, "{}", resumed.text_content());
            assert_eq!(SubagentMeta::load(&path).await.unwrap().profile, "explore");
        }

        /// A swarm member past `swarm_timeout_ms` reports `timed_out`; the
        /// others are unaffected.
        #[tokio::test]
        async fn a_swarm_member_past_its_deadline_times_out() {
            let dir = tempfile::tempdir().unwrap();
            let llm = PacedLlm::new(Duration::ZERO, true);
            let ctx = ExecutableToolContext {
                swarm_timeout: Some(Duration::from_millis(150)),
                ..ctx_with(llm, dir.path())
            };

            let result = swarm_on()
                .execute(
                    serde_json::json!({
                        "description": "work",
                        "prompt_template": "work on {{item}}",
                        "items": ["a", "b"]
                    }),
                    &ctx,
                    "tc",
                )
                .await;

            let text = result.text_content();
            assert_eq!(
                text.matches(r#"state="started" outcome="failed" stop_reason="timed_out""#)
                    .count(),
                2,
                "{text}"
            );
            assert_eq!(text.matches(" agent_id=\"sub-").count(), 2, "{text}");
            assert!(
                text.contains("<resume_hint>Call agent_swarm with resume_agent_ids"),
                "{text}"
            );
        }
    }

    /// A subagent is launched only when asked for, not for any task that
    /// looks large. What asks is said, and that the agent types below do
    /// not.
    #[test]
    fn the_agent_tool_launches_only_when_asked() {
        let tool = AgentTool::new(Arc::new(ProfileRegistry::with_builtin_profiles()));
        let description = &tool.tool_definition().description;
        for rule in [
            "Do not launch a subagent unless the user explicitly asks for one.",
            "Explicit instructions in AGENTS.md or a skill, and swarm mode when the user turned it on, also count.",
            "they never authorize it",
        ] {
            assert!(description.contains(rule), "{description}");
        }
        assert!(crate::swarm::EXIT_REMINDER.contains(
            "Handle the request yourself, and launch subagents with agent only if it explicitly asks for them."
        ));
    }

    /// The tool says what each agent type is for and has, after what it does.
    #[test]
    fn the_agent_tool_describes_the_agent_types() {
        let tool = AgentTool::new(Arc::new(ProfileRegistry::with_builtin_profiles()));
        let description = &tool.tool_definition().description;
        assert!(description.starts_with(AGENT_DESCRIPTION), "{description}");
        assert!(
            description.contains("\n\nAvailable agent types (pass via `profile`):\n- coder: "),
            "{description}"
        );
        assert!(
            description.contains("the session's read-only MCP tools"),
            "{description}"
        );
    }

    #[test]
    fn test_agent_tool_definition_has_required_fields() {
        let tool = AgentTool::new(Arc::new(ProfileRegistry::with_builtin_profiles()));
        assert_eq!(tool.tool_definition().name, "agent");
    }

    fn ctx_at_depth(depth: usize) -> ExecutableToolContext {
        ExecutableToolContext {
            depth,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::env::current_dir().unwrap(),
            allowed_paths: std::collections::HashSet::new(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            user_interaction: None,
            search_url: None,
            plan_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: crate::permissions::PermissionMode::Default,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            background: std::sync::Arc::new(crate::background::BackgroundManager::new()),
            cron: std::sync::Arc::new(crate::cron::CronManager::new()),
            goal_manager: std::sync::Arc::new(std::sync::Mutex::new(
                crate::goal::GoalManager::new(),
            )),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            ..Default::default()
        }
    }

    /// Sub-agents inherit the full tool set, `agent` included, so without this
    /// bound one can spawn sub-agents forever — each level multiplying rather
    /// than adding, with no natural end.
    #[test]
    fn spawning_is_refused_at_the_depth_bound() {
        let at_limit = ctx_at_depth(crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH);
        let refusal = depth_refusal(&at_limit).expect("must refuse at the bound");
        assert!(refusal.is_error, "the refusal must read as a tool failure");
        assert!(
            format!("{:?}", refusal.output).contains("Do this work yourself"),
            "a refusal with no alternative just gets retried: {:?}",
            refusal.output
        );
    }

    #[test]
    fn spawning_is_refused_beyond_the_bound() {
        assert!(
            depth_refusal(&ctx_at_depth(
                crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH + 5
            ))
            .is_some()
        );
    }

    /// The bound must not stop legitimate delegation on the way there.
    #[test]
    fn spawning_is_allowed_below_the_bound() {
        for depth in 0..crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH {
            assert!(
                depth_refusal(&ctx_at_depth(depth)).is_none(),
                "depth {depth} must still be allowed to delegate"
            );
        }
    }

    /// A bound of zero disables delegation entirely, which is a legitimate way
    /// to run a session that must never fan out.
    #[test]
    fn a_zero_bound_refuses_every_spawn() {
        let mut ctx = ctx_at_depth(0);
        ctx.max_subagent_depth = 0;
        assert!(depth_refusal(&ctx).is_some());
    }
}
