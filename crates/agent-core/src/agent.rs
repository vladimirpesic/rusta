use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use lm_bridge::message::Message;
use tokio_util::sync::CancellationToken;

use crate::{
    agent_loop::types::{
        ExecutableToolContext, LoopHooks, StepResult, ToolEventSender, UserInteraction,
    },
    agent_loop::{Llm, LlmCallbacks, TurnContext, run_turn},
    background::BackgroundManager,
    context::ContextMemory,
    cron::CronManager,
    errors::AgentError,
    goal::{GoalManager, GoalStatus},
    permissions::{ApprovalHook, PermissionEngine, PermissionMode},
    profile::{McpTools, ProfileRegistry},
    records::Records,
    skills::{ActiveSkills, SkillRegistry},
    stale_guard::StaleGuard,
    todo::TodoList,
    tool_disclosure::ToolDisclosureState,
    tools::{ToolRegistry, builtin::*},
    vision_fallback::VisionFallback,
    web_reader_client::WebReaderClient,
    web_search_client::WebSearchClient,
};

/// Default cap on model round-trips (steps) within a single agent turn. A real
/// coding task — scan, navigate, read several files, edit, run tests, iterate —
/// routinely needs dozens of steps, so this is deliberately generous while still
/// bounding a runaway loop. Override per-run with `Agent::with_max_turns`
/// (CLI `--max-turns`, `RUSTA_MAX_TURNS`, or the `max_turns` config field).
pub const DEFAULT_MAX_TURNS: u32 = 100;

pub struct Agent {
    context: ContextMemory,
    registry: Arc<Mutex<ToolRegistry>>,
    pub(crate) tool_ctx: ExecutableToolContext,
    permissions: PermissionEngine,
    plan_mode: Arc<AtomicBool>,
    tool_event_sender: Option<ToolEventSender>,
    loop_hooks: LoopHooks,
    llm: Option<Arc<dyn Llm>>,
    vision_fallback: Option<Arc<dyn VisionFallback>>,
    web_search_client: Option<Arc<dyn WebSearchClient>>,
    web_reader_client: Option<Arc<dyn WebReaderClient>>,
    skills: Option<Arc<SkillRegistry>>,
    active_skills: Arc<Mutex<ActiveSkills>>,
    session_title: String,
    drain_agent_tasks_on_stop: bool,
    max_turns: u32,
    /// Prepend the repository's git context to this agent's first user
    /// message: the `explore` profile's orientation block. Set only on a
    /// fresh profile sub-agent.
    git_context: bool,
    /// Set by the front-end before it cancels a turn's token when the cancel
    /// is the user's own (Ctrl+C / Esc) rather than SIGTERM, a limit, or a
    /// programmatic abort; a user-cancelled turn is followed by the
    /// interruption reminder. Shared with the signal task, which cannot take
    /// the agent's lock, and never inherited by a spawned sub-agent.
    user_cancel: Arc<AtomicBool>,
    /// The locks this process holds sessions with, and this session's id:
    /// set for the session a front-end runs, not for sub-agents
    /// ([`crate::session_lock`]).
    session_lock: Option<(Arc<crate::session_lock::SessionLocks>, String)>,
    /// Swarm mode, while it is on ([`crate::swarm`]).
    swarm_mode: Option<crate::swarm::SwarmTrigger>,
    /// Whether swarm mode is on, as `agent_swarm` reads it: it runs only
    /// then.
    swarm_switch: Arc<AtomicBool>,
    retry: crate::agent_loop::retry::RetryPolicy,
    profile_registry: Arc<ProfileRegistry>,
    model_pool: Option<Arc<dyn crate::model_pool::ModelPool>>,
    caller_model: String,
}

/// Context budget for a sub-agent whose model does not report its window.
pub const DEFAULT_CHILD_CONTEXT_TOKENS: u64 = 128_000;

/// Framing appended after a forked sub-agent's inherited conversation.
///
/// See [`Agent::with_forked_context`].
pub const FORK_INHERITANCE_NOTICE: &str = "The conversation above was inherited from the agent that spawned you. It is reference \
     material — context you may draw on — not your own prior turns, and it will not update as that \
     agent continues. Any tool call left unanswered in it was still running when you were spawned. \
     Your task follows.";

/// Where a turn's prompt came from, as the turn hooks report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOrigin {
    /// Typed or piped in by the user.
    User,
    /// A scheduled prompt's fire, delivered as a turn of its own.
    Cron,
    /// A turn Rusta starts to continue the work: the headless run's nudge
    /// after scheduled or background activity.
    Continuation,
}

impl PromptOrigin {
    /// The origin kind: `user`, `cron_job` or `system_trigger`.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Cron => "cron_job",
            Self::Continuation => "system_trigger",
        }
    }

    /// The trigger's name, which only a system trigger has.
    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        match self {
            Self::Continuation => Some("headless_continuation"),
            Self::User | Self::Cron => None,
        }
    }
}

impl Agent {
    pub fn new(cwd: PathBuf, system_prompt: impl Into<String>) -> Self {
        Self::new_with_budget(cwd, system_prompt, 128_000)
    }

    pub fn new_with_budget(
        cwd: PathBuf,
        system_prompt: impl Into<String>,
        max_tokens: u64,
    ) -> Self {
        let system_prompt = system_prompt.into();
        let agents_md = Arc::new(Mutex::new(crate::agents_md::AgentsMdReminder::new(
            &cwd,
            &system_prompt,
        )));
        let registry = Arc::new(Mutex::new(ToolRegistry::new()));
        // Kept on the agent as well as handed to the sub-agent tools, so
        // `rebuild_subagent_tools` can re-register them without the caller
        // having to restate the profiles it never changed.
        let builtin_profiles = Arc::new(ProfileRegistry::with_builtin_profiles());
        let swarm_switch = Arc::new(AtomicBool::new(false));
        {
            let mut reg = registry.lock().unwrap();
            reg.register(Arc::new(ReadFileTool::new()));
            reg.register(Arc::new(WriteFileTool::new()));
            reg.register(Arc::new(EditFileTool::new()));
            reg.register(Arc::new(GrepTool::new()));
            reg.register(Arc::new(GlobTool::new()));
            reg.register(Arc::new(ListDirTool::new()));
            reg.register(Arc::new(FetchTool::new()));
            reg.register(Arc::new(WebSearchTool::new()));
            reg.register(Arc::new(BashTool::new()));
            reg.register(Arc::new(AskUserTool::new()));
            reg.register(Arc::new(AskUserQuestionTool::new()));
            reg.register(Arc::new(TodoListTool::new()));
            reg.register(Arc::new(ExitPlanModeTool::new()));
            reg.register(Arc::new(AgentTool::new(Arc::clone(&builtin_profiles))));
            reg.register(Arc::new(
                AgentSwarmTool::new(Arc::clone(&builtin_profiles))
                    .with_swarm_mode(Arc::clone(&swarm_switch)),
            ));
            reg.register(Arc::new(ReadMediaTool::new()));
            reg.register(Arc::new(TaskListTool::new()));
            reg.register(Arc::new(TaskOutputTool::new()));
            reg.register(Arc::new(TaskStopTool::new()));
            reg.register(Arc::new(WaitForTool::new()));
            reg.register(Arc::new(CronCreateTool::new()));
            reg.register(Arc::new(CronListTool::new()));
            reg.register(Arc::new(CronDeleteTool::new()));
            reg.register(Arc::new(CreateGoalTool::new()));
            reg.register(Arc::new(UpdateGoalTool::new()));
            reg.register(Arc::new(GetGoalTool::new()));
            reg.register(Arc::new(SetGoalBudgetTool::new()));
        }

        let tool_disclosure = Arc::new(Mutex::new(ToolDisclosureState::new()));
        {
            let mut reg = registry.lock().unwrap();
            reg.register(Arc::new(SelectToolsTool::new(
                Arc::downgrade(&registry),
                Arc::clone(&tool_disclosure),
            )));
        }

        let plan_mode = Arc::new(AtomicBool::new(false));
        let mut permissions = PermissionEngine::new(PermissionMode::Default);
        permissions.set_plan_mode(Arc::clone(&plan_mode));
        let background = Arc::new(BackgroundManager::new());
        let cron = Arc::new(CronManager::new());
        let goal_manager = Arc::new(Mutex::new(GoalManager::new()));
        let active_skills = Arc::new(Mutex::new(ActiveSkills::new()));

        Self {
            context: ContextMemory::new(system_prompt, max_tokens),
            registry: Arc::clone(&registry),
            tool_ctx: ExecutableToolContext {
                depth: 0,
                max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
                cwd,
                allowed_paths: HashSet::new(),
                todo_list: TodoList::shared(),
                tool_event_sender: None,
                user_interaction: None,
                search_url: None,
                plan_mode: Arc::clone(&plan_mode),
                permission_mode: crate::permissions::PermissionMode::Default,
                llm: None,
                vision_fallback: None,
                web_search_client: None,
                web_reader_client: None,
                background,
                cron,
                goal_manager,
                tool_disclosure: Some(Arc::clone(&tool_disclosure)),
                stale_guard: Default::default(),
                fork_source: Default::default(),
                agents_md,
                tool_registry: Some(Arc::clone(&registry)),
                ..Default::default()
            },
            permissions,
            plan_mode,
            tool_event_sender: None,
            loop_hooks: LoopHooks::default(),
            drain_agent_tasks_on_stop: false,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            skills: None,
            active_skills,
            session_title: String::new(),
            max_turns: DEFAULT_MAX_TURNS,
            retry: crate::agent_loop::retry::RetryPolicy::default(),
            profile_registry: builtin_profiles,
            model_pool: None,
            caller_model: "the calling model".to_string(),
            swarm_mode: None,
            swarm_switch,
            git_context: false,
            user_cancel: Arc::new(AtomicBool::new(false)),
            session_lock: None,
        }
    }

    pub fn with_records(mut self, records: Records) -> Self {
        // A fresh session under an existing file sees only what it writes,
        // and what other processes write after it begins.
        records.start_fresh();
        self.attach_sidecars(&records, false);
        self.context = self.context.with_records(records);
        self.expose_records_to_readers();
        self
    }

    /// Attach the session state kept beside the records file
    /// ([`crate::records::SESSION_SIDECARS`]): the todo list, goals and active
    /// skills, and the files this agent has read ([`crate::stale_guard`]).
    ///
    /// What was read is loaded only for a `resumed` session. A new one starts
    /// with an empty context, so it has read nothing yet, whatever an earlier
    /// session under the same records file saw.
    fn attach_sidecars(&mut self, records: &Records, resumed: bool) {
        let sidecar = |kind: &str| crate::records::sidecar_path(records.path(), kind);
        let todo_path = sidecar("todos");
        if let Ok(list) = TodoList::load(&todo_path) {
            self.tool_ctx.todo_list = Arc::new(Mutex::new(list));
        } else if let Ok(mut guard) = self.tool_ctx.todo_list.lock() {
            guard.set_persist_path(todo_path);
        }
        let goals_path = sidecar("goals");
        self.tool_ctx.goal_manager = Arc::new(Mutex::new(
            GoalManager::load(goals_path.clone()).unwrap_or_else(|_| {
                let mut manager = GoalManager::new();
                manager.set_persist_path(goals_path);
                manager
            }),
        ));
        // Loaded into the agent's own tracker, not a new one: the `skill`
        // tool, registered before the records were attached, records into it.
        let skills_path = sidecar("skills");
        let skills = ActiveSkills::load(&skills_path).unwrap_or_else(|_| {
            let mut skills = ActiveSkills::new();
            skills.set_persist_path(skills_path);
            skills
        });
        *self.active_skills.lock().unwrap() = skills;
        let reads_path = sidecar("reads");
        let guard = if resumed {
            StaleGuard::load(&reads_path).unwrap_or_else(|_| StaleGuard::saved_to(reads_path))
        } else {
            StaleGuard::saved_to(reads_path)
        };
        self.tool_ctx.stale_guard = Arc::new(Mutex::new(guard));
        // Cron tasks are per session: a resumed session gets its
        // tasks back from the sidecar, a fresh one starts empty. Either way
        // changes persist to the same sidecar.
        let cron_path = sidecar("cron");
        self.tool_ctx.cron = Arc::new(CronManager::load(&cron_path, resumed));
    }

    /// Lets `read_file` and `grep` open the records file and blob directory,
    /// which compaction summaries point the model at.
    fn expose_records_to_readers(&mut self) {
        let paths = [self.context.records_path(), self.context.blob_dir()];
        for path in paths.into_iter().flatten() {
            let path = path.to_path_buf();
            if !self.tool_ctx.readable_paths.contains(&path) {
                self.tool_ctx.readable_paths.push(path);
            }
        }
    }

    /// Lock the session `id` with `locks` while this agent writes it: each
    /// turn and compaction holds the lock of the records file, and first
    /// reloads what another process wrote since ([`crate::session_lock`]).
    #[must_use]
    pub fn with_session_lock(
        mut self,
        locks: Arc<crate::session_lock::SessionLocks>,
        id: impl Into<String>,
    ) -> Self {
        self.session_lock = Some((locks, id.into()));
        self
    }

    /// The locked session, as its lock knows it: `None` without a lock or a
    /// records file.
    #[must_use]
    pub fn locked_session(&self) -> Option<crate::session_lock::LockedSession> {
        let (_, id) = self.session_lock.as_ref()?;
        let records = self.context.records_path()?;
        Some(crate::session_lock::LockedSession::new(id.clone(), records))
    }

    /// Remove the session's lock another process holds, for a holder judged
    /// alive by mistake; the holder it named, `None` when there was none to
    /// take over.
    ///
    /// # Errors
    /// The lock file could not be removed.
    pub fn take_over_session(&self) -> std::io::Result<Option<crate::session_lock::LockHolder>> {
        let (Some((locks, _)), Some(session)) = (&self.session_lock, self.locked_session()) else {
            return Ok(None);
        };
        locks.take_over(&session)
    }

    /// Take the session's lock for a write, and bring the history up to date
    /// when another process wrote the session since this one last did.
    async fn hold_session(
        &mut self,
    ) -> Result<Option<crate::session_lock::SessionLease>, AgentError> {
        let Some(session) = self.locked_session() else {
            return Ok(None);
        };
        let Some((locks, _)) = &self.session_lock else {
            return Ok(None);
        };
        let lease = locks.lease(&session)?;
        if self
            .context
            .records()
            .is_some_and(Records::appended_elsewhere)
        {
            self.reload_records().await?;
            tracing::warn!(
                "session '{}' was written by another process: its history was reloaded",
                session.id
            );
        }
        Ok(Some(lease))
    }

    /// Rebuild the conversation from this session's view of its records:
    /// what it held, and what another process added.
    async fn reload_records(&mut self) -> Result<(), AgentError> {
        let Some(records) = self.context.records().cloned() else {
            return Ok(());
        };
        let messages = records.load_view().await?;
        self.context.clear_messages();
        self.resume(messages);
        Ok(())
    }

    /// Load records from disk and replay them into the conversation context.
    pub async fn with_resumed_records(mut self, records: Records) -> Result<Self, AgentError> {
        let messages = records.load().await?;
        // Attached before the replay: the records' blob directory holds the
        // large tool results the replayed history refers to.
        self.context = self.context.with_records(records.clone());
        self.resume(messages);
        self.expose_records_to_readers();
        self.attach_sidecars(&records, true);
        Ok(self)
    }

    /// Replay a list of messages into the context as if they were part of an
    /// ongoing conversation.
    ///
    /// The tools the conversation loaded with `select_tools` are loaded again.
    /// All of `messages` are consulted, including any a recorded compaction
    /// folds away: in a live session a loaded tool stays loaded across a
    /// compaction, so it does across a resume too.
    pub fn resume(&mut self, messages: Vec<Message>) {
        if let Some(disclosure) = &self.tool_ctx.tool_disclosure {
            let context = &self.context;
            disclosure
                .lock()
                .unwrap()
                .fold_loaded_from_history(&messages, |text| context.expand_blob_text(text));
        }
        self.context.replay_messages(messages);
        if let Some(disclosure) = &self.tool_ctx.tool_disclosure {
            disclosure
                .lock()
                .unwrap()
                .fold_announcements_from_history(&self.context.build_history());
        }
    }

    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        let mut permissions = PermissionEngine::new(mode);
        permissions.set_plan_mode(Arc::clone(&self.plan_mode));
        // The guard switch holds whether it was set before the mode or after.
        permissions.set_dangerous_command_guard(self.tool_ctx.dangerous_command_guard);
        self.permissions = permissions;
        // A fresh engine has no hook and no rules, so there is nothing yet for
        // a sub-agent to inherit.
        self.tool_ctx.approval = crate::permissions::InheritedApproval::default();
        // Keep the context in step so a sub-agent spawned from this agent
        // inherits this mode rather than a stale one.
        self.tool_ctx.permission_mode = mode;
        self
    }

    /// Replace the permission mode at runtime.
    ///
    /// Updates the context alongside the engine so a `/yolo`, `/auto`, or
    /// `/permission` switch reaches sub-agents spawned after it.
    pub fn set_permission_mode(&mut self, mode: PermissionMode) {
        self.permissions.set_mode(mode);
        self.tool_ctx.permission_mode = mode;
    }

    /// The current permission mode. This is what a spawned sub-agent inherits.
    #[must_use]
    pub fn permission_mode(&self) -> PermissionMode {
        self.permissions.mode()
    }

    /// Attach an interactive approval hook.
    #[must_use]
    pub fn with_approval_hook(mut self, hook: std::sync::Arc<dyn ApprovalHook>) -> Self {
        self.set_approval_hook(hook);
        self
    }

    /// Replace the interactive approval hook. A foreground sub-agent spawned
    /// from here asks through it too.
    pub fn set_approval_hook(&mut self, hook: std::sync::Arc<dyn ApprovalHook>) {
        self.tool_ctx.approval.hook = Some(Arc::clone(&hook));
        self.permissions.set_hook(hook);
    }

    /// Add configured permission policy rules. Sub-agents spawned from here
    /// are held to them too.
    #[must_use]
    pub fn with_permission_rules(mut self, rules: Vec<crate::permissions::PolicyRule>) -> Self {
        self.tool_ctx.approval.rules.extend(rules.iter().cloned());
        self.permissions.add_rules(rules);
        self
    }

    /// Name this agent — a sub-agent, by its `agent_id` — on its approval
    /// requests.
    pub fn set_approval_requester(&mut self, requester: Option<String>) {
        self.permissions.set_requester(requester);
    }

    /// Name this sub-agent `agent_id`: on its approval requests, and on its
    /// tool events where its session shows sub-agents' work.
    pub fn name_subagent(&mut self, agent_id: Option<String>) {
        if let (Some(id), Some(sender)) = (&agent_id, &self.tool_ctx.subagent_tool_events) {
            let (id, sender) = (id.clone(), Arc::clone(sender));
            self.tool_event_sender = Some(Arc::new(move |event| sender(&id, event)));
        }
        self.set_approval_requester(agent_id);
    }

    /// Where this agent's sub-agents report their tool events.
    pub fn set_subagent_tool_event_sender(
        &mut self,
        sender: crate::agent_loop::types::SubagentToolEventSender,
    ) {
        self.tool_ctx.subagent_tool_events = Some(sender);
    }

    /// Guidance for the system prompt of a sub-agent that gets MCP tools —
    /// Devscriptor's workflow, when its server is configured.
    pub fn set_subagent_mcp_guidance(&mut self, guidance: Option<crate::McpGuidance>) {
        self.tool_ctx.subagent_mcp_guidance =
            guidance.filter(|g| !g.all.trim().is_empty()).map(Arc::new);
    }

    /// The session's prompt as its sub-agents get it, with their role in its
    /// slot; `None` gives them their role and the environment section alone.
    pub fn set_subagent_prompt(&mut self, prompt: Option<crate::prompt::SubagentPrompt>) {
        self.tool_ctx.subagent_prompt = prompt.map(Arc::new);
    }

    /// The session's prompt as its sub-agents get it, when it is set.
    #[must_use]
    pub fn subagent_prompt(&self) -> Option<&crate::prompt::SubagentPrompt> {
        self.tool_ctx.subagent_prompt.as_deref()
    }

    /// Start in plan mode, as the user asked: the model plans, every edit
    /// and command waits for the user's approval, and the plan waits for
    /// their review (see [`crate::plan_mode`]).
    #[must_use]
    pub fn with_plan_mode(self, enabled: bool) -> Self {
        self.plan_mode.store(enabled, Ordering::SeqCst);
        self
    }

    /// Attach a sender for tool lifecycle events.
    #[must_use]
    pub fn with_tool_event_sender(mut self, sender: ToolEventSender) -> Self {
        self.tool_event_sender = Some(sender);
        self
    }

    /// Replace the tool lifecycle event sender.
    pub fn set_tool_event_sender(&mut self, sender: ToolEventSender) {
        self.tool_event_sender = Some(sender);
    }

    /// Hold a turn open until background sub-agents finish, instead of ending
    /// the run and abandoning them. Intended for the headless (`-p`) flow,
    /// which has no later prompt in which to collect their results.
    pub fn set_drain_agent_tasks_on_stop(&mut self, drain: bool) {
        self.drain_agent_tasks_on_stop = drain;
    }

    /// Ask (or not) before dangerous `bash` commands — `rm -rf`, `shutdown`,
    /// `dd` to a device — in every permission mode but bypass. On by default.
    #[must_use]
    pub fn with_dangerous_command_guard(mut self, enabled: bool) -> Self {
        self.tool_ctx.dangerous_command_guard = enabled;
        self.permissions.set_dangerous_command_guard(enabled);
        self
    }

    /// Turn the repeat breaker on or off: whether a tool call repeated across
    /// steps earns reminders and, at the end of the ladder, stops the turn.
    /// A duplicate within one step runs once either way.
    #[must_use]
    pub fn with_repeat_breaker(mut self, enabled: bool) -> Self {
        self.tool_ctx.repeat_breaker = enabled;
        self
    }

    /// Bound how long each `agent_swarm` member may run; `None` removes the
    /// bound. A member past it is cancelled and reports `timed_out`.
    #[must_use]
    pub fn with_swarm_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.tool_ctx.swarm_timeout = timeout;
        self
    }

    /// Cap how many `agent_swarm` members run at once; `None` runs them all
    /// together.
    #[must_use]
    pub fn with_swarm_max_concurrency(mut self, max: Option<usize>) -> Self {
        self.tool_ctx.swarm_max_concurrency = max.filter(|&n| n > 0);
        self
    }

    /// Bound how deep sub-agent delegation may nest.
    ///
    /// `0` disables delegation entirely — a legitimate setting for a session
    /// that must never fan out. See [`ExecutableToolContext::max_subagent_depth`].
    #[must_use]
    pub fn with_max_subagent_depth(mut self, depth: usize) -> Self {
        self.tool_ctx.max_subagent_depth = depth;
        self
    }

    /// Set the maximum number of model round-trips (steps) allowed within a
    /// single agent turn. A value of 0 is treated as [`DEFAULT_MAX_TURNS`] so a
    /// misconfiguration can never wedge the loop into returning immediately.
    #[must_use]
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = if max_turns == 0 {
            DEFAULT_MAX_TURNS
        } else {
            max_turns
        };
        self
    }

    /// Set optional hooks that observe or influence the turn loop.
    #[must_use]
    pub fn with_loop_hooks(mut self, hooks: LoopHooks) -> Self {
        self.loop_hooks = hooks;
        self
    }

    /// Attach the session's user hooks (`[[hooks]]` in the user config).
    ///
    /// The hooks reach the tool context (PreToolUse, Stop, …) and the
    /// background manager (TaskStarted, Notification) from here, so both see
    /// them no matter which is rebuilt later.
    pub fn with_user_hooks(mut self, hooks: Option<Arc<crate::user_hooks::UserHooks>>) -> Self {
        self.tool_ctx.user_hooks = hooks.clone();
        if let Some(hooks) = hooks {
            self.tool_ctx.background.set_user_hooks(hooks);
        }
        self
    }

    /// Configure the maximum number of concurrently running background tasks.
    #[must_use]
    pub fn with_background_max_tasks(self, max: usize) -> Self {
        let timeout = self.tool_ctx.background.agent_timeout();
        self.rebuild_background(max, timeout)
    }

    /// Set how long a delegated sub-agent may run before it is cancelled.
    ///
    /// `None` removes the deadline. Prefer a bound: a headless turn waits for
    /// sub-agents to settle, so an unbounded one that wedges hangs the run.
    #[must_use]
    pub fn with_subagent_timeout(self, timeout: Option<std::time::Duration>) -> Self {
        let max = self.tool_ctx.background.max_running_tasks();
        self.rebuild_background(max, timeout)
    }

    /// Set the maximum attempts for a single step before its error is surfaced.
    ///
    /// Counts the first attempt, so `1` disables retrying. A value of 0 is
    /// treated as the default, mirroring [`Self::with_max_turns`].
    #[must_use]
    pub fn with_max_attempts_per_step(mut self, max: u32) -> Self {
        self.retry.max_attempts = if max == 0 {
            crate::agent_loop::retry::DEFAULT_MAX_ATTEMPTS
        } else {
            max
        };
        self
    }

    /// Retry failing model calls without limit (`RUSTA_INFINITE_RETRY`).
    ///
    /// For unattended runs that should wait out an outage or a spent quota
    /// window instead of failing. Failures no retry can fix still end the call;
    /// see [`crate::agent_loop::retry::retryable_without_limit`].
    #[must_use]
    pub fn with_infinite_retry(mut self, infinite: bool) -> Self {
        self.retry.infinite = infinite;
        self
    }

    /// Keep background-task output and sub-agent records in `data_dir`
    /// instead of the user's data directory.
    #[must_use]
    pub fn with_data_dir(self, data_dir: PathBuf) -> Self {
        let max_tasks = self.tool_ctx.background.max_running_tasks();
        let timeout = self.tool_ctx.background.agent_timeout();
        self.install_background(
            BackgroundManager::with_data_dir(data_dir),
            max_tasks,
            timeout,
        )
    }

    /// Replace the background manager, carrying both of its settings across.
    ///
    /// Both builders above have to rebuild it — the limits are fixed at
    /// construction — and each therefore has to restate the *other's* value.
    /// Rebuilding from defaults instead meant whichever builder ran second
    /// silently discarded the first one's setting.
    #[must_use]
    fn rebuild_background(self, max_tasks: usize, timeout: Option<std::time::Duration>) -> Self {
        let background = self.tool_ctx.background.for_same_data_dir();
        self.install_background(background, max_tasks, timeout)
    }

    /// Run background tasks and sub-agents on `background`, with these limits.
    #[must_use]
    fn install_background(
        mut self,
        background: BackgroundManager,
        max_tasks: usize,
        timeout: Option<std::time::Duration>,
    ) -> Self {
        let background = Arc::new(
            background
                .with_max_running_tasks(max_tasks)
                .with_agent_timeout(timeout),
        );
        self.tool_ctx.background = background.clone();
        self
    }

    /// Bound tool results with `policy` before they enter the context.
    ///
    /// See [`crate::tool_output`]. Sub-agents inherit the policy.
    #[must_use]
    pub fn with_tool_output_policy(mut self, policy: crate::tool_output::ToolOutputPolicy) -> Self {
        self.tool_ctx.tool_output = policy;
        self
    }

    /// Model requests one context compaction may make, retries and shrinks
    /// included (`compaction_max_attempts`). `0` restores the default of
    /// [`crate::context::DEFAULT_COMPACTION_MAX_ATTEMPTS`]. Sub-agents inherit
    /// it.
    #[must_use]
    pub fn with_compaction_max_attempts(mut self, attempts: u32) -> Self {
        self.context.set_compaction_max_attempts(attempts);
        self.tool_ctx.compaction_max_attempts = if attempts == 0 {
            crate::context::DEFAULT_COMPACTION_MAX_ATTEMPTS
        } else {
            attempts
        };
        self
    }

    /// Attach a user-interaction channel for tools like `ask_user`.
    #[must_use]
    pub fn with_user_interaction(
        mut self,
        interaction: std::sync::Arc<dyn UserInteraction>,
    ) -> Self {
        self.tool_ctx.user_interaction = Some(interaction);
        self
    }

    /// Replace the user-interaction channel.
    pub fn set_user_interaction(&mut self, interaction: std::sync::Arc<dyn UserInteraction>) {
        self.tool_ctx.user_interaction = Some(interaction);
    }

    /// Set the URL template for the built-in `web_search` tool.
    #[must_use]
    pub fn with_search_url(mut self, url: impl Into<String>) -> Self {
        self.tool_ctx.search_url = Some(url.into());
        self
    }

    /// Add an additional allowed path outside the agent cwd.
    #[must_use]
    pub fn with_allowed_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.tool_ctx.allowed_paths.insert(path.into());
        self
    }

    /// Attach an LLM reference so that tools (such as the `agent` subagent
    /// tool) can spawn nested agents.
    #[must_use]
    pub fn with_llm(mut self, llm: Arc<dyn Llm>) -> Self {
        self.set_llm(llm);
        self
    }

    /// Replace the LLM reference at runtime (used by `/model` and `/provider`).
    /// The context budget keeps free the output the new model reserves.
    pub fn set_llm(&mut self, llm: Arc<dyn Llm>) {
        self.llm = Some(llm);
        self.set_context_budget(self.context.max_tokens());
    }

    /// Reconfigure the context token budget: `max_tokens` of window, less the
    /// output the attached model reserves ([`Llm::output_reservation`]).
    pub fn set_context_budget(&mut self, max_tokens: u64) {
        let reservation = self.llm.as_ref().and_then(|llm| llm.output_reservation());
        self.context
            .set_budget(crate::context::ContextBudget::reserving_output(
                max_tokens,
                reservation,
            ));
    }

    /// Replace the system prompt at runtime.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        let prompt = prompt.into();
        // The instruction files the new prompt carries are seen.
        self.tool_ctx
            .agents_md
            .lock()
            .unwrap()
            .seed_from_prompt(&prompt);
        self.context.set_system_prompt(prompt);
    }

    /// Add an additional allowed path at runtime.
    pub fn add_allowed_path(&mut self, path: impl Into<std::path::PathBuf>) {
        self.tool_ctx.allowed_paths.insert(path.into());
    }

    /// Attach a vision fallback for non-vision models.
    #[must_use]
    pub fn with_vision_fallback(mut self, fallback: Arc<dyn VisionFallback>) -> Self {
        self.vision_fallback = Some(fallback);
        self
    }

    /// Replace the vision fallback at runtime.
    pub fn set_vision_fallback(&mut self, fallback: Arc<dyn VisionFallback>) {
        self.vision_fallback = Some(fallback);
    }

    /// Clear the vision fallback at runtime: a provider switch to one that
    /// has none.
    pub fn clear_vision_fallback(&mut self) {
        self.vision_fallback = None;
    }

    /// Replace the web-search client at runtime.
    pub fn set_web_search_client(&mut self, client: Arc<dyn WebSearchClient>) {
        self.web_search_client = Some(client);
    }

    /// Clear the web-search client at runtime.
    pub fn clear_web_search_client(&mut self) {
        self.web_search_client = None;
    }

    /// Attach a web-reader client for the built-in `web_reader` tool.
    #[must_use]
    pub fn with_web_reader_client(mut self, client: Arc<dyn WebReaderClient>) -> Self {
        self.web_reader_client = Some(client);
        self.registry
            .lock()
            .unwrap()
            .register(Arc::new(WebReaderTool::new()));
        self
    }

    /// Replace the web-reader client at runtime and register the tool if needed.
    pub fn set_web_reader_client(&mut self, client: Arc<dyn WebReaderClient>) {
        self.web_reader_client = Some(client);
        if self.registry.lock().unwrap().get("web_reader").is_none() {
            self.registry
                .lock()
                .unwrap()
                .register(Arc::new(WebReaderTool::new()));
        }
    }

    /// Clear the web-reader client and unregister the built-in tool.
    pub fn clear_web_reader_client(&mut self) {
        self.web_reader_client = None;
        self.registry.lock().unwrap().unregister("web_reader");
    }

    /// Update the session display title, which the hooks' payloads carry.
    pub fn set_session_title(&mut self, title: impl Into<String>) {
        self.session_title = title.into();
        if let Some(hooks) = &self.tool_ctx.user_hooks {
            hooks.set_session_title(self.session_title.clone());
        }
    }

    /// Return the current session display title, if any.
    pub fn session_title(&self) -> &str {
        &self.session_title
    }

    /// Whether the conversation has begun: it holds a message the user
    /// wrote, not only Rusta's own reminders. Read from the recorded
    /// messages, not the history sent to the model, which joins the first
    /// prompt to the date reminder before it.
    pub fn has_user_input(&self) -> bool {
        self.context.has_real_user_input()
    }

    /// The conversation's first exchange: the user's first prompt and the
    /// model's answer, what a session title is made from. `None` until the
    /// first turn has an answer.
    pub fn first_exchange(&self) -> Option<(String, String)> {
        self.context.first_exchange()
    }

    /// Compact the conversation now (`/compact`): a handoff summary written by
    /// `llm`, or — when none can be had — eviction that keeps the first and the
    /// latest user message. The summary is written to the records right away.
    /// `instruction` is `/compact <instruction>`'s focus text for the
    /// summarizer; auto compactions pass `None`.
    pub async fn compact_context(
        &mut self,
        llm: &dyn Llm,
        cancel: &CancellationToken,
        instruction: Option<&str>,
    ) -> Result<crate::agent_loop::llm::CompactionNotice, AgentError> {
        let _lease = self.hold_session().await?;
        let disclosure = self
            .tool_ctx
            .tool_disclosure
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(ToolDisclosureState::new())));
        let tools = crate::agent_loop::run_turn::step_tool_table(
            &self.registry,
            &self.tool_ctx,
            &disclosure,
        );
        let target = self.context.compaction_target();
        let notice = crate::agent_loop::run_turn::compact_context(
            &mut self.context,
            crate::agent_loop::run_turn::CompactionRun {
                llm: Some(llm),
                tools: &tools,
                tool_ctx: &self.tool_ctx,
                callbacks: &LlmCallbacks::default(),
                cancel,
                trigger: crate::agent_loop::llm::CompactionTrigger::Manual,
                instruction: instruction.map(str::to_string),
                target,
            },
        )
        .await?;
        disclosure.lock().unwrap().forget_announcements();
        self.tool_ctx.agents_md.lock().unwrap().forget_reminders();
        self.context.persist().await?;
        Ok(notice)
    }

    /// Clear all non-system messages from the conversation context.
    pub fn clear_context(&mut self) {
        self.context.clear_messages();
        self.forget_context_artifacts();
    }

    /// The history lost what the turn loop put in it — tool announcements,
    /// delivered tool definitions, instruction-file reminders — so the next
    /// step puts them back.
    fn forget_context_artifacts(&self) {
        if let Some(disclosure) = &self.tool_ctx.tool_disclosure {
            disclosure.lock().unwrap().forget_announcements();
        }
        self.tool_ctx.agents_md.lock().unwrap().forget_reminders();
    }

    /// Undo the last user/assistant turn.
    ///
    /// Returns the number of messages removed.
    pub fn undo_last_turn(&mut self) -> usize {
        let removed = self.context.undo_last_turn();
        if removed > 0 {
            // Announcements and reminders may have gone with the turn.
            self.forget_context_artifacts();
        }
        removed
    }

    /// Undo the last `count` user/assistant turns (`/undo <count>`).
    ///
    /// A count that exceeds what can be undone is an
    /// [`UndoError`](crate::context::UndoError) and removes nothing.
    pub fn undo_turns(&mut self, count: usize) -> Result<usize, crate::context::UndoError> {
        let removed = self.context.undo_turns(count)?;
        if removed > 0 {
            // Announcements and reminders may have gone with the turns.
            self.forget_context_artifacts();
        }
        Ok(removed)
    }

    /// Replace the profile registry used by the `agent` subagent tool.
    #[must_use]
    pub fn with_profile_registry(mut self, registry: Arc<ProfileRegistry>) -> Self {
        self.set_profile_registry(registry);
        self
    }

    /// Turn swarm mode on — until turned off, or for the next prompt only —
    /// or off. The conversation hears of it at the next prompt.
    pub fn set_swarm_mode(&mut self, trigger: Option<crate::swarm::SwarmTrigger>) {
        self.swarm_mode = trigger;
        self.swarm_switch.store(trigger.is_some(), Ordering::SeqCst);
    }

    /// Swarm mode, while it is on.
    #[must_use]
    pub fn swarm_mode(&self) -> Option<crate::swarm::SwarmTrigger> {
        self.swarm_mode
    }

    /// Replace the profiles `agent` and `agent_swarm` offer, keeping the
    /// sub-agent model pool.
    pub fn set_profile_registry(&mut self, registry: Arc<ProfileRegistry>) {
        self.profile_registry = registry;
        self.rebuild_subagent_tools();
    }

    /// Seed this agent with a conversation forked from the agent that spawned it.
    ///
    /// The seed and the notice are one operation on purpose. The inherited
    /// messages are the *caller's* turns, not this agent's: left unframed the
    /// model reads them as its own prior context and carries that conversation
    /// on, instead of doing the task it was given. Seeding without framing is
    /// never the right thing, so there is no way to ask for it.
    #[must_use]
    pub fn with_forked_context(mut self, history: Vec<lm_bridge::message::Message>) -> Self {
        // Close the caller's still-open tool exchange before the seed lands.
        // That call is the `agent` invocation creating this fork, so it can
        // never have a real result; done here rather than at the snapshot so it
        // holds however the history was obtained, and so the child's own
        // projection finds it answered instead of filling it with the generic
        // missing-result placeholder.
        let history = crate::context_projector::close_trailing_open_tool_exchange(
            &history,
            crate::context_projector::INHERITED_IN_FLIGHT_TOOL_TEXT,
        );
        // The caller's system prompt is not this agent's. A second system
        // message buried mid-history is instruction the model cannot act on, and
        // a resumed caller's records can carry one into the seed.
        let history: Vec<_> = history
            .into_iter()
            .filter(|m| m.role != lm_bridge::message::Role::System)
            .collect();

        // The inherited `select_tools` answers stay true for the fork: its
        // tools are the caller's.
        if let Some(disclosure) = &self.tool_ctx.tool_disclosure {
            disclosure
                .lock()
                .unwrap()
                .fold_loaded_from_history(&history, str::to_string);
        }
        self.context.replay_messages(history);
        self.context.add_user_message(FORK_INHERITANCE_NOTICE);
        self
    }

    /// A sub-agent of the agent whose tool context is `parent`.
    ///
    /// It runs on `llm` — the parent's model when `None` — and its context
    /// budget follows that model's window rather than a fixed guess, so a
    /// sub-agent on a 1M-token model does not compact at a tenth of it. It is
    /// one level deeper than the parent and shares its runtime: permission mode
    /// (never more privileged than the parent), allowed paths, background tasks
    /// (through its own handle, so its tasks report to it) and the cron manager,
    /// tool hooks, tool-output policy, and the web and vision clients. Its
    /// `wait_for` tells it at run time that ending its turn is its hand-off:
    /// one description serves every agent, and a sub-agent's timeout advice
    /// and repeat warning carry what its context needs.
    pub(crate) fn spawn_child(
        parent: &ExecutableToolContext,
        system_prompt: impl Into<String>,
        llm: Option<Arc<dyn Llm>>,
    ) -> Self {
        let llm = llm.or_else(|| parent.llm.clone());
        let budget = llm
            .as_ref()
            .and_then(|llm| llm.context_limit())
            .unwrap_or(DEFAULT_CHILD_CONTEXT_TOKENS);
        let mut agent = Agent::new_with_budget(parent.cwd.clone(), system_prompt, budget)
            .with_permission_mode(parent.permission_mode);
        // Provider built-ins (`$web_search` on Kimi) are the provider's own
        // server-side tools; the session registers them on its agent only, so
        // they are copied here — a fork and a profile sub-agent alike must
        // offer them or their prefix diverges from the session's.
        agent.adopt_provider_builtins(parent);
        agent.tool_ctx.allowed_paths = parent.allowed_paths.clone();
        agent.tool_ctx.user_interaction = parent.user_interaction.clone();
        agent.tool_ctx.search_url = parent.search_url.clone();
        // The same tasks, through the child's own handle: what it starts is
        // reported to it, and it cannot wait on or drain its parent's tasks.
        agent.tool_ctx.background = Arc::new(parent.background.scoped());
        agent.tool_ctx.cron = parent.cron.clone();
        agent.tool_ctx.user_hooks = parent.user_hooks.clone();
        agent.tool_ctx.tool_output = parent.tool_output.clone();
        agent.tool_ctx.compaction_max_attempts = parent.compaction_max_attempts;
        agent.tool_ctx.readable_paths = parent.readable_paths.clone();
        agent.tool_ctx.repeat_breaker = parent.repeat_breaker;
        agent.tool_ctx.swarm_timeout = parent.swarm_timeout;
        agent.tool_ctx.swarm_max_concurrency = parent.swarm_max_concurrency;
        agent.tool_ctx.dangerous_command_guard = parent.dangerous_command_guard;
        agent
            .permissions
            .set_dangerous_command_guard(parent.dangerous_command_guard);
        agent
            .context
            .set_compaction_max_attempts(parent.compaction_max_attempts);
        // One level deeper than the agent that spawned it, so the depth bound is
        // reached no matter how the nesting was arrived at. `wait_for` reads
        // the depth to tell a sub-agent at run time that ending its turn is
        // its hand-off; no tool is re-registered with a different description.
        agent.tool_ctx.depth = parent.depth + 1;
        agent.tool_ctx.subagent_usage = Arc::clone(&parent.subagent_usage);
        agent.tool_ctx.subagent_tool_events = parent.subagent_tool_events.clone();
        agent.tool_ctx.max_subagent_depth = parent.max_subagent_depth;
        agent.vision_fallback = parent.vision_fallback.clone();
        agent.web_search_client = parent.web_search_client.clone();
        if let Some(reader) = parent.web_reader_client.clone() {
            agent = agent.with_web_reader_client(reader);
        }
        // Asks the user as its parent does, under the same rules — never more
        // freely than the parent, and without denying everything that needs
        // an answer. A caller running it in the background takes the hook away.
        if let Some(hook) = parent.approval.hook.clone() {
            agent.set_approval_hook(hook);
        }
        agent = agent.with_permission_rules(parent.approval.rules.clone());
        agent.tool_ctx.subagent_mcp_guidance = parent.subagent_mcp_guidance.clone();
        agent.tool_ctx.subagent_prompt = parent.subagent_prompt.clone();
        // The session's skills, through the sub-agent's own `skill` tool, which
        // records into its own skills; a profile that does not list `skill`
        // drops it. A coder had none, and looked for a skill's file on disk.
        if let Some(registry) = parent.skill_registry.clone() {
            agent.set_skill_registry(registry);
        }
        if let Some(llm) = llm {
            agent = agent.with_llm(llm);
        }
        agent
    }

    /// Give this sub-agent the MCP tools (`mcp__*`) of the agent whose context
    /// is `parent` that `access` takes: the same tools, deferred where they
    /// are deferred there, so they are announced and loaded with
    /// `select_tools` as in the session.
    ///
    /// For a profile that takes them ([`crate::Profile::mcp_access`]).
    pub(crate) fn adopt_mcp_tools(&mut self, parent: &ExecutableToolContext, access: McpTools) {
        let Some(source) = &parent.tool_registry else {
            return;
        };
        let tools: Vec<(Arc<dyn crate::agent_loop::types::ExecutableTool>, bool)> = {
            let reg = source.lock().unwrap();
            reg.all()
                .iter()
                .filter(|tool| tool.tool_definition().name.starts_with("mcp__"))
                .filter(|tool| match access {
                    McpTools::None => false,
                    McpTools::ReadOnly => tool.is_read_only(),
                    McpTools::All => true,
                })
                .map(|tool| {
                    let dynamic = reg.is_dynamic(&tool.tool_definition().name);
                    (Arc::clone(tool), dynamic)
                })
                .collect()
        };
        let adopts_deferred = tools.iter().any(|(_, dynamic)| *dynamic);
        let mut reg = self.registry.lock().unwrap();
        for (tool, dynamic) in tools {
            if dynamic {
                reg.register_dynamic(tool);
            } else {
                reg.register(tool);
            }
        }
        // Deferred tools are loaded with `select_tools`, which a profile's own
        // tool list may have left out.
        if adopts_deferred
            && reg
                .get(crate::tools::builtin::select_tools::SELECT_TOOLS_TOOL_NAME)
                .is_none()
            && let Some(disclosure) = &self.tool_ctx.tool_disclosure
        {
            reg.register(Arc::new(SelectToolsTool::new(
                Arc::downgrade(&self.registry),
                Arc::clone(disclosure),
            )));
        }
    }

    /// Give this sub-agent the provider built-ins of the agent whose context
    /// is `parent`: server-side tools the provider executes (`$web_search` on
    /// Kimi), which the session registers on its own agent only. Copied in the
    /// caller's order and deferred where they are deferred there; what is
    /// already registered keeps its place.
    pub(crate) fn adopt_provider_builtins(&mut self, parent: &ExecutableToolContext) {
        let Some(source) = &parent.tool_registry else {
            return;
        };
        let builtins: Vec<(Arc<dyn crate::agent_loop::types::ExecutableTool>, bool)> = {
            let reg = source.lock().unwrap();
            reg.all()
                .iter()
                .filter(|tool| tool.tool_definition().is_builtin_function())
                .map(|tool| {
                    let dynamic = reg.is_dynamic(&tool.tool_definition().name);
                    (Arc::clone(tool), dynamic)
                })
                .collect()
        };
        if builtins.is_empty() {
            return;
        }
        let mut reg = self.registry.lock().unwrap();
        for (tool, dynamic) in builtins {
            if reg.get(&tool.tool_definition().name).is_some() {
                continue;
            }
            if dynamic {
                reg.register_dynamic(tool);
            } else {
                reg.register(tool);
            }
        }
    }

    /// Rebuild this fork's tool table as a copy of its caller's: every tool in
    /// the caller's order, the same definitions, deferred where the caller
    /// defers it. The fork's first request then carries the same `tools` prefix
    /// as its caller's and hits the caller's prompt cache. Includes the
    /// provider built-ins [`Self::adopt_provider_builtins`] copies and the
    /// session's MCP tools, so it replaces `adopt_mcp_tools`.
    ///
    /// A tool that holds its agent's own state is re-bound to this agent's
    /// ([`ExecutableTool::rebind_for_fork`]) and keeps its place in the order:
    /// `select_tools` loads into this agent's registry and disclosure, and
    /// `skill` records into this agent's skills.
    ///
    /// [`ExecutableTool::rebind_for_fork`]: crate::agent_loop::types::ExecutableTool::rebind_for_fork
    pub(crate) fn adopt_tool_table(&mut self, parent: &ExecutableToolContext) {
        let Some(source) = &parent.tool_registry else {
            return;
        };
        let tools: Vec<(Arc<dyn crate::agent_loop::types::ExecutableTool>, bool)> = {
            let reg = source.lock().unwrap();
            reg.all()
                .iter()
                .map(|tool| {
                    let dynamic = reg.is_dynamic(&tool.tool_definition().name);
                    (Arc::clone(tool), dynamic)
                })
                .collect()
        };
        let disclosure = self
            .tool_ctx
            .tool_disclosure
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(ToolDisclosureState::new())));
        let binding = crate::agent_loop::types::ForkBinding {
            registry: Arc::downgrade(&self.registry),
            disclosure: Arc::clone(&disclosure),
            active_skills: Arc::clone(&self.active_skills),
        };
        let mut reg = self.registry.lock().unwrap();
        reg.clear();
        for (tool, dynamic) in tools {
            let tool = tool.rebind_for_fork(&binding).unwrap_or(tool);
            if dynamic {
                reg.register_dynamic(tool);
            } else {
                reg.register(tool);
            }
        }
        drop(reg);
        self.tool_ctx.tool_disclosure = Some(disclosure);
    }

    /// Record the profile this sub-agent runs under.
    pub(crate) fn set_profile(&mut self, profile: Option<String>) {
        self.tool_ctx.profile = profile;
    }

    /// The model this agent runs on, if one was attached.
    #[must_use]
    pub fn llm(&self) -> Option<Arc<dyn Llm>> {
        self.llm.clone()
    }

    /// Attach a pool of models sub-agents may be bound to.
    ///
    /// `caller_model` is the model *this* agent runs on, named in the pool's
    /// `primary` entry. With no pool the `model` parameter is not advertised and
    /// every sub-agent inherits the parent. See [`crate::model_pool`].
    #[must_use]
    pub fn with_model_pool(
        mut self,
        pool: Option<Arc<dyn crate::model_pool::ModelPool>>,
        caller_model: impl Into<String>,
    ) -> Self {
        self.model_pool = pool;
        self.caller_model = caller_model.into();
        self.rebuild_subagent_tools();
        self
    }

    /// Re-register `agent` and `agent_swarm` against the current profile
    /// registry and model pool.
    ///
    /// Both are baked into the tool *descriptions*, which are fixed at
    /// construction, so changing either means rebuilding the tools. Doing it in
    /// one place is what keeps a later `with_profile_registry` from dropping the
    /// pool, or a later `with_model_pool` from dropping the profiles.
    /// Register `agent` and `agent_swarm` afresh, in their places: replacing
    /// a tool keeps the table's order, and with it the prompt cache.
    fn rebuild_subagent_tools(&self) {
        let mut reg = self.registry.lock().unwrap();
        reg.register(Arc::new(AgentTool::with_pool(
            Arc::clone(&self.profile_registry),
            self.model_pool.clone(),
            &self.caller_model,
        )));
        reg.register(Arc::new(
            AgentSwarmTool::with_pool(
                Arc::clone(&self.profile_registry),
                self.model_pool.clone(),
                &self.caller_model,
            )
            .with_swarm_mode(Arc::clone(&self.swarm_switch)),
        ));
    }

    /// Attach a skill registry and register the `skill` tool.
    #[must_use]
    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.set_skill_registry(registry);
        self
    }

    /// Replace the skill registry, and the `skill` tool listing it in its
    /// place in the tool table (`/reload`).
    pub fn set_skill_registry(&mut self, registry: Arc<SkillRegistry>) {
        self.registry
            .lock()
            .unwrap()
            .register(Arc::new(SkillTool::new(
                Arc::clone(&registry),
                Arc::clone(&self.active_skills),
            )));
        self.tool_ctx.skill_registry = Some(Arc::clone(&registry));
        self.skills = Some(registry);
    }

    /// Add `reminder` to the conversation as a `<system-reminder>`, for the
    /// model to read with the next prompt.
    pub fn add_system_reminder(&mut self, reminder: &str) {
        self.context.add_message(Message::user(format!(
            "<system-reminder>\n{reminder}\n</system-reminder>"
        )));
    }

    /// Toggle plan mode, at the user's request, and return the new state.
    /// The next turn starts with the reminder that says so.
    pub fn toggle_plan_mode(&self) -> bool {
        let current = self.plan_mode.load(Ordering::SeqCst);
        self.plan_mode.store(!current, Ordering::SeqCst);
        !current
    }

    /// The message history this agent would send to its provider next.
    ///
    /// Projected and repaired exactly as a real request is, so it answers what
    /// the model actually receives rather than what is stored — the two differ,
    /// and only the former is worth asserting on.
    #[must_use]
    pub fn context_history(&self) -> Vec<lm_bridge::message::Message> {
        self.context.build_history()
    }

    /// The full definitions of all registered tools, as the model is shown them.
    ///
    /// `list_tools` returns only names, which cannot answer what a tool actually
    /// advertises — its description and parameter schema are what the model
    /// chooses on.
    #[must_use]
    pub fn tool_definitions(&self) -> Vec<lm_bridge::tool::Tool> {
        self.registry
            .lock()
            .unwrap()
            .definitions()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Return the names of all registered tools.
    ///
    /// Goal-related tools that only make sense when a goal is active are hidden
    /// when there is no active goal, and so is a tool that cannot work with
    /// the model and the clients attached now
    /// ([`crate::agent_loop::types::ExecutableTool::is_available`]).
    pub fn list_tools(&self) -> Vec<String> {
        let has_goal = self
            .tool_ctx
            .goal_manager
            .lock()
            .unwrap()
            .active_goal()
            .is_some();
        let ctx = self.next_turn_tool_ctx();
        self.registry
            .lock()
            .unwrap()
            .all()
            .iter()
            .filter(|tool| tool.is_available(&ctx))
            .map(|tool| tool.tool_definition())
            .filter_map(|t| {
                if !has_goal && (t.name == "update_goal" || t.name == "set_goal_budget") {
                    return None;
                }
                Some(t.name.clone())
            })
            .collect()
    }

    /// Access the shared tool execution context (used by slash commands
    /// that invoke tools directly, e.g. `/web` and `/swarm`).
    pub fn tool_ctx(&self) -> &ExecutableToolContext {
        &self.tool_ctx
    }

    /// The context the next turn's tools run in: this one, with the model
    /// and the clients attached now, which a turn takes up when it starts.
    /// It answers no prompt until the turn gives it one.
    pub fn next_turn_tool_ctx(&self) -> ExecutableToolContext {
        let mut ctx = self.tool_ctx.clone();
        ctx.llm = self.llm.clone();
        ctx.vision_fallback = self.vision_fallback.clone();
        ctx.web_search_client = self.web_search_client.clone();
        ctx.web_reader_client = self.web_reader_client.clone();
        ctx.prompt = None;
        ctx
    }

    /// Access the active-skills tracker for session persistence.
    pub fn active_skills(&self) -> Arc<Mutex<ActiveSkills>> {
        Arc::clone(&self.active_skills)
    }

    /// Return the most recent non-empty assistant text, if any.
    pub fn last_assistant_text(&self) -> Option<String> {
        self.context.last_assistant_text()
    }

    /// Whether plan mode is enabled.
    #[must_use]
    pub fn plan_mode(&self) -> bool {
        self.plan_mode.load(Ordering::SeqCst)
    }

    pub async fn prompt(
        &mut self,
        llm: &dyn Llm,
        text: impl Into<String>,
        cancel: &CancellationToken,
    ) -> Result<StepResult, AgentError> {
        self.prompt_with_callbacks(llm, text, LlmCallbacks::default(), cancel)
            .await
    }

    /// Run the user's prompt with streaming callbacks for text/think/tool-call
    /// deltas.
    ///
    /// When a goal is active, the agent will automatically continue turns (up
    /// to a small cap) until the goal is paused, completed, blocked, or the
    /// model stops without making progress.
    pub async fn prompt_with_callbacks(
        &mut self,
        llm: &dyn Llm,
        text: impl Into<String>,
        callbacks: LlmCallbacks,
        cancel: &CancellationToken,
    ) -> Result<StepResult, AgentError> {
        self.prompt_from(PromptOrigin::User, llm, text, callbacks, cancel)
            .await
    }

    /// [`Self::prompt_with_callbacks`] for a prompt from `origin`.
    /// UserPromptSubmit fires for the user's own prompts only, and
    /// TurnStarted reports the origin.
    pub async fn prompt_from(
        &mut self,
        origin: PromptOrigin,
        llm: &dyn Llm,
        text: impl Into<String>,
        callbacks: LlmCallbacks,
        cancel: &CancellationToken,
    ) -> Result<StepResult, AgentError> {
        // The session is locked while the turn writes it, and its history
        // brought up to date first.
        let _lease = self.hold_session().await?;
        // A flag left over from an earlier turn must not dress up the next
        // abort as a user cancel.
        self.user_cancel.store(false, Ordering::SeqCst);
        let outcome = self
            .run_prompt(origin, llm, text.into(), callbacks, cancel)
            .await;
        // A turn the user cancelled (Ctrl+C / Esc — never SIGTERM, a limit, or
        // a programmatic abort) is followed by the interruption reminder: the
        // partial output above is incomplete, and the next message continues
        // the conversation. A front-end's cancel surfaces as either
        // cancellation shape, depending on where the token fired.
        if let Err(e) = &outcome {
            if e.is_cancellation() {
                if self.user_cancel.swap(false, Ordering::SeqCst) {
                    self.context.add_interruption_reminder();
                    self.fire_interrupt_hook().await;
                }
            } else {
                self.fire_stop_failure_hook(e).await;
            }
        }
        // `/swarm <task>` holds for that task only.
        if self.swarm_mode == Some(crate::swarm::SwarmTrigger::Task) {
            self.set_swarm_mode(None);
        }
        // Whatever the turn produced is recorded, finished or not: the steps
        // of a stopped, cancelled or failed turn are what a resume picks up.
        self.context.persist().await?;
        // And with them what the turn read, so the resume may edit those files.
        if let Err(e) = self.tool_ctx.stale_guard.lock().unwrap().persist() {
            tracing::warn!(
                "could not save which files this session has read: {e}. A resumed session \
                 will have to read them again before editing them."
            );
        }
        outcome
    }

    async fn run_prompt(
        &mut self,
        origin: PromptOrigin,
        llm: &dyn Llm,
        text: String,
        callbacks: LlmCallbacks,
        cancel: &CancellationToken,
    ) -> Result<StepResult, AgentError> {
        let prompt = Arc::<str>::from(text.as_str());
        // UserPromptSubmit runs before the user's message lands. Allowing
        // hooks may add stdout to the context; it is folded into the prompt
        // message rather than added as a separate one.
        // A blocking hook skips the model call entirely, with its reason as
        // the error. A scheduled or continuation prompt is no user input.
        let text = match &self.tool_ctx.user_hooks {
            Some(hooks) if origin == PromptOrigin::User && !text.trim().is_empty() => {
                let mut fields = serde_json::Map::new();
                fields.insert(
                    "prompt".to_string(),
                    serde_json::Value::String(text.clone()),
                );
                let outcome = hooks
                    .fire(
                        crate::user_hooks::UserHookEvent::UserPromptSubmit,
                        &text,
                        fields,
                    )
                    .await;
                if let Some(reason) = outcome.blocked {
                    return Err(AgentError::HookBlocked(reason));
                }
                if outcome.stdout.is_empty() {
                    text
                } else {
                    format!("{text}\n\n{}", outcome.stdout)
                }
            }
            _ => text,
        };
        // TurnStarted announces the turn and where its prompt came from; the
        // matcher tests the origin kind, and only a system trigger has a
        // name.
        if let Some(hooks) = &self.tool_ctx.user_hooks {
            let mut fields = serde_json::Map::new();
            fields.insert(
                "turn_id".to_string(),
                serde_json::Value::String(
                    (self.tool_ctx.turn_id.load(Ordering::SeqCst) + 1).to_string(),
                ),
            );
            fields.insert(
                "origin_kind".to_string(),
                serde_json::Value::String(origin.kind().to_string()),
            );
            if let Some(name) = origin.name() {
                fields.insert(
                    "origin_name".to_string(),
                    serde_json::Value::String(name.to_string()),
                );
            }
            fields.insert(
                "prompt".to_string(),
                serde_json::Value::String(text.clone()),
            );
            hooks
                .fire(
                    crate::user_hooks::UserHookEvent::TurnStarted,
                    origin.kind(),
                    fields,
                )
                .await;
        }
        self.context
            .announce_date(chrono::Local::now().date_naive());
        for marker in self.tool_ctx.cron.drain_fire_markers() {
            self.context.add_message(Message::user(marker));
        }
        for marker in self.tool_ctx.background.drain_completion_markers() {
            self.context.add_message(Message::user(marker));
        }
        // Swarm mode is said when it starts, and when it ends.
        let history = self.context.build_history();
        let last_entry = crate::swarm::last_reminder_was_entry(&history);
        if let Some(reminder) = crate::swarm::reminder_due(self.swarm_mode.is_some(), last_entry) {
            self.context
                .add_message(crate::swarm::reminder_message(reminder));
        }
        // Plan mode is the session agent's alone: a fork carries its
        // parent's plan reminders, but not its plan mode.
        if self.tool_ctx.depth == 0
            && let Some(reminder) = crate::plan_mode::reminder_due(self.plan_mode(), &history)
        {
            self.context
                .add_message(crate::plan_mode::reminder_message(reminder));
        }
        self.tool_ctx = ExecutableToolContext {
            prompt: Some(prompt),
            ..self.next_turn_tool_ctx()
        };

        let mut first_text = Some(text);
        let mut first = true;
        let mut result: Option<StepResult> = None;
        // What the prompt used over all its turns, for its result.
        let mut usage = lm_bridge::usage::TokenUsage::empty();
        const MAX_GOAL_CONTINUES: usize = 5;

        for _continue in 0..MAX_GOAL_CONTINUES {
            if cancel.is_cancelled() {
                return Err(AgentError::Interrupted);
            }
            let start = std::time::Instant::now();

            if first {
                let text = first_text.take().expect("the first text is used once");
                let text = if self.git_context && !self.context.has_real_user_input() {
                    // A fresh profile sub-agent (explore) starts with the
                    // repository's state; a resumed one already recorded it
                    // with its first message.
                    match crate::git_context::collect(&self.tool_ctx.cwd).await {
                        prefix if !prefix.is_empty() => format!("{prefix}\n\n{text}"),
                        _ => text,
                    }
                } else {
                    text
                };
                self.context.add_user_message(text);
                first = false;
            } else if let Some(reminder) = self.goal_reminder() {
                self.context.add_message(Message::system(reminder));
            }

            let step = run_turn(
                llm,
                TurnContext {
                    context: &mut self.context,
                    registry: Arc::clone(&self.registry),
                    tool_ctx: &self.tool_ctx,
                    permissions: &self.permissions,
                    cancel,
                    tool_event_sender: self.tool_event_sender.clone(),
                    loop_hooks: self.loop_hooks.clone(),
                    drain_agent_tasks_on_stop: self.drain_agent_tasks_on_stop,
                    max_turns: self.max_turns,
                    retry: self.retry,
                    swarm_mode: self.swarm_mode.is_some(),
                },
                None,
                callbacks.clone(),
            )
            .await?;

            // This turn's time and tokens: the goal adds them to its totals.
            let elapsed_ms = start.elapsed().as_millis() as u64;
            let tokens = step.usage.total_tokens;
            usage.add(&step.usage);
            result = Some(step);

            if let Some(reason) = self
                .tool_ctx
                .goal_manager
                .lock()
                .unwrap()
                .record_turn(tokens, elapsed_ms)
            {
                self.context
                    .add_message(Message::system(format!("Goal blocked: {reason}")));
                break;
            }

            let status = self
                .tool_ctx
                .goal_manager
                .lock()
                .unwrap()
                .active_goal()
                .map(|g| g.status);
            if status != Some(GoalStatus::Active) {
                break;
            }

            // If the model returned a final answer without acting on the goal,
            // loop with a reminder. If it made tool calls, also loop so it can
            // observe the results and continue working toward the goal.
        }

        let mut result = result.expect("at least one turn was run");
        result.usage = usage;
        Ok(result)
    }

    fn goal_reminder(&self) -> Option<String> {
        self.tool_ctx
            .goal_manager
            .lock()
            .unwrap()
            .active_goal()
            .map(|g| format!("Continue pursuing the active goal: {}", g.objective))
    }

    /// The Interrupt event: fired when the user cut the turn short (Ctrl+C /
    /// Esc), in place of Stop.
    async fn fire_interrupt_hook(&self) {
        let Some(hooks) = &self.tool_ctx.user_hooks else {
            return;
        };
        let mut fields = serde_json::Map::new();
        fields.insert(
            "reason".to_string(),
            serde_json::Value::String("user_interrupt".to_string()),
        );
        hooks
            .fire(crate::user_hooks::UserHookEvent::Interrupt, "", fields)
            .await;
    }

    /// The StopFailure event: the turn failed with an error. The matcher
    /// input is the error's kind; cancellation is an Interrupt instead and
    /// never reaches here.
    async fn fire_stop_failure_hook(&self, error: &AgentError) {
        let Some(hooks) = &self.tool_ctx.user_hooks else {
            return;
        };
        let mut fields = serde_json::Map::new();
        fields.insert(
            "error".to_string(),
            serde_json::Value::String(error.to_string()),
        );
        hooks
            .fire(
                crate::user_hooks::UserHookEvent::StopFailure,
                error.hook_error_kind(),
                fields,
            )
            .await;
    }

    /// Whether an active goal is still in progress.
    pub fn has_active_goal(&self) -> bool {
        self.tool_ctx
            .goal_manager
            .lock()
            .unwrap()
            .active_goal()
            .is_some_and(|g| g.status == GoalStatus::Active)
    }

    /// Whether a headless `-p` run still has work that will surface *after* the
    /// current turn and is worth staying alive for: an active goal, cron tasks
    /// that are scheduled or have already fired, or background completions not
    /// yet delivered to the model. Detached shell tasks are deliberately
    /// excluded — a dev server or watcher is meant to outlive the run.
    pub fn has_outstanding_headless_work(&self) -> bool {
        self.has_active_goal()
            || self.tool_ctx.cron.has_pending_work()
            || self.tool_ctx.background.pending_completion_count() > 0
    }

    /// The number of cron-fire / background-completion notifications waiting to
    /// be delivered to the model in a continuation turn.
    pub fn pending_notifications(&self) -> usize {
        self.tool_ctx.cron.pending_fire_count()
            + self.tool_ctx.background.pending_completion_count()
    }

    pub fn history(&self) -> Vec<Message> {
        self.context.build_history()
    }

    /// Register an arbitrary executable tool as a static tool.
    pub fn register_tool(&mut self, tool: Arc<dyn crate::agent_loop::types::ExecutableTool>) {
        self.registry.lock().unwrap().register(tool);
    }

    /// Register a dynamic tool. Its definition is withheld until the model
    /// calls `select_tools`.
    pub fn register_dynamic_tool(
        &mut self,
        tool: Arc<dyn crate::agent_loop::types::ExecutableTool>,
    ) {
        self.registry.lock().unwrap().register_dynamic(tool);
    }

    /// Remove a tool from the registry.
    pub fn unregister_tool(
        &mut self,
        name: &str,
    ) -> Option<Arc<dyn crate::agent_loop::types::ExecutableTool>> {
        self.registry.lock().unwrap().unregister(name)
    }

    /// Restrict the tool registry to only the named tools. Provider built-ins
    /// (`$web_search` on Kimi) are never removed: they are the provider's own
    /// server-side tools, not tools the profile gates.
    pub(crate) fn restrict_tools(&mut self, allowed: &std::collections::HashSet<String>) {
        let to_remove: Vec<String> = {
            let reg = self.registry.lock().unwrap();
            reg.definitions()
                .iter()
                .map(|t| t.name.clone())
                .filter(|n| !allowed.contains(n))
                .filter(|n| {
                    reg.get(n)
                        .is_some_and(|tool| !tool.tool_definition().is_builtin_function())
                })
                .collect()
        };
        for name in to_remove {
            self.registry.lock().unwrap().unregister(&name);
        }
    }

    /// Remove `denied` from the tool table however long the table is: the
    /// "everything except …" half of a profile's tool set. Provider built-ins
    /// are never cut (same rule as [`Agent::restrict_tools`]).
    pub(crate) fn remove_tools(&mut self, denied: &std::collections::HashSet<String>) {
        let to_remove: Vec<String> = {
            let reg = self.registry.lock().unwrap();
            reg.definitions()
                .iter()
                .map(|t| t.name.clone())
                .filter(|n| denied.contains(n))
                .filter(|n| {
                    reg.get(n)
                        .is_some_and(|tool| !tool.tool_definition().is_builtin_function())
                })
                .collect()
        };
        for name in to_remove {
            self.registry.lock().unwrap().unregister(&name);
        }
    }

    /// Prepend the repository's git context to this agent's first user
    /// message (`explore`'s orientation block).
    pub(crate) fn enable_git_context(&mut self) {
        self.git_context = true;
    }

    /// The shared user-cancel flag: the front-end sets it before cancelling a
    /// turn's token when the cancel is the user's own (Ctrl+C / Esc), so the
    /// interrupted turn is followed by the interruption reminder. SIGTERM,
    /// limits and programmatic aborts leave it clear.
    #[must_use]
    pub fn user_cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.user_cancel)
    }

    /// Steer `message` into the turn this agent is running: appended as a
    /// user message before the turn's next model request, once the current
    /// tool batch has finished (the TUI's Ctrl-S). A `wait_for` in
    /// progress ends early. Not a cancel: no interruption reminder follows.
    pub fn steer(&self, message: impl Into<String>) {
        self.tool_ctx.steer.steer(message);
    }

    /// This agent's steering channel. A front-end that holds the agent in a
    /// mutex for a whole turn (the TUI) clones this handle up front and
    /// steers through it, never touching the lock.
    #[must_use]
    pub fn steer_channel(&self) -> Arc<crate::steer::SteerChannel> {
        Arc::clone(&self.tool_ctx.steer)
    }

    /// Tick the cron schedule and queue what came due, for delivery ahead of
    /// the next turn's prompt: one poll of the headless keepalive, or of the
    /// REPL when a line is entered. Returns the jobs that were not already
    /// waiting, so each is announced once until it is delivered.
    pub fn poll_cron(&self) -> Vec<String> {
        let fires = self.tool_ctx.cron.tick();
        self.tool_ctx.cron.queue_fires(fires)
    }

    /// Drop every cron task: what `/new` does, so a fresh conversation in
    /// the same session does not inherit scheduled work. The now-empty state
    /// is persisted over the session's sidecar. The store is cleared in
    /// place, so a handle a front-end took earlier still reaches it.
    pub fn reset_cron(&self) {
        self.tool_ctx.cron.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lm_bridge::{message::ToolCall, provider::FinishReason, tool::Tool, usage::TokenUsage};

    struct MockLlm {
        responses: std::sync::Mutex<Vec<StepResult>>,
    }

    impl MockLlm {
        fn new(responses: Vec<StepResult>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl Llm for MockLlm {
        fn model_name(&self) -> &str {
            "mock"
        }

        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Ok(StepResult {
                    message: Message::assistant_text(""),
                    usage: TokenUsage::empty(),
                    finish_reason: Some(FinishReason::Stop),
                    response_id: None,
                    stop_reason: None,
                });
            }
            Ok(responses.remove(0))
        }
    }

    /// Answers from a script and keeps the system prompt and the tools JSON of
    /// every request, so a test can assert what the model was actually sent.
    struct ScriptedLlm {
        responses: std::sync::Mutex<Vec<StepResult>>,
        requests: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
        /// The full message list of every request, in order.
        messages: std::sync::Mutex<Vec<Vec<Message>>>,
        /// Text steered into the turn from inside the first model call —
        /// what a user's Ctrl-S does while the turn runs.
        steer_on_first_call: std::sync::Mutex<Option<(Arc<crate::steer::SteerChannel>, String)>>,
    }

    impl ScriptedLlm {
        fn new(responses: Vec<StepResult>) -> Arc<Self> {
            Arc::new(Self {
                responses: std::sync::Mutex::new(responses),
                requests: std::sync::Mutex::new(Vec::new()),
                messages: std::sync::Mutex::new(Vec::new()),
                steer_on_first_call: std::sync::Mutex::new(None),
            })
        }

        /// Steer `text` into the turn during the first model call.
        fn steering_on_first_call(
            self: &Arc<Self>,
            channel: Arc<crate::steer::SteerChannel>,
            text: &str,
        ) -> Arc<Self> {
            *self.steer_on_first_call.lock().unwrap() = Some((channel, text.to_string()));
            Arc::clone(self)
        }
    }

    #[async_trait]
    impl Llm for ScriptedLlm {
        fn model_name(&self) -> &str {
            "recording"
        }

        async fn chat(
            &self,
            messages: Vec<Message>,
            tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            if let Some((channel, text)) = self.steer_on_first_call.lock().unwrap().take() {
                channel.steer(text);
            }
            let system = messages
                .iter()
                .find(|m| m.role == lm_bridge::message::Role::System)
                .map(Message::text_content)
                .unwrap_or_default();
            self.requests
                .lock()
                .unwrap()
                .push((system, serde_json::to_value(&tools).unwrap()));
            self.messages.lock().unwrap().push(messages);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Ok(StepResult {
                    message: Message::assistant_text(""),
                    usage: TokenUsage::empty(),
                    finish_reason: Some(FinishReason::Stop),
                    response_id: None,
                    stop_reason: None,
                });
            }
            Ok(responses.remove(0))
        }
    }

    #[tokio::test]
    async fn test_agent_executes_tool_call_and_returns() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("test.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let llm = MockLlm::new(vec![
            StepResult {
                message: Message::assistant_tool_calls(vec![
                    ToolCall::new("tc-1", "read_file")
                        .with_arguments(serde_json::json!({ "path": file_path }).to_string()),
                ]),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::ToolCalls),
                response_id: None,
                stop_reason: None,
            },
            StepResult {
                message: Message::assistant_text(""),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            },
        ]);

        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let cancel = CancellationToken::new();

        let result = agent.prompt(&llm, "read the file", &cancel).await.unwrap();
        assert!(result.tool_calls().is_empty());

        let history = agent.history();
        assert_eq!(
            history.first().unwrap().role,
            lm_bridge::message::Role::System
        );
        assert!(history.first().unwrap().text_content().contains("sys"));

        let tool_results: Vec<_> = history
            .iter()
            .filter(|m| matches!(m.role, lm_bridge::message::Role::Tool))
            .collect();
        assert_eq!(tool_results.len(), 1);
        assert!(tool_results[0].text_content().contains("hello world"));
    }

    /// A search carries the prompt its turn answers, as the user wrote it;
    /// the next turn's search carries the next prompt, and the context of a
    /// turn not yet begun carries none.
    #[tokio::test]
    async fn a_search_carries_the_prompt_of_its_turn() {
        struct Recorder(Arc<Mutex<Vec<Option<String>>>>);

        #[async_trait]
        impl WebSearchClient for Recorder {
            async fn search(&self, _query: &str, question: Option<&str>) -> Result<String, String> {
                self.0.lock().unwrap().push(question.map(str::to_string));
                Ok("No results.".to_string())
            }
        }

        let search_then_answer = || {
            ScriptedLlm::new(vec![
                StepResult {
                    message: Message::assistant_tool_calls(vec![
                        ToolCall::new("tc-1", "web_search").with_arguments(r#"{"query": "luhn"}"#),
                    ]),
                    usage: TokenUsage::empty(),
                    finish_reason: Some(FinishReason::ToolCalls),
                    response_id: None,
                    stop_reason: None,
                },
                StepResult {
                    message: Message::assistant_text("Found nothing."),
                    usage: TokenUsage::empty(),
                    finish_reason: Some(FinishReason::Stop),
                    response_id: None,
                    stop_reason: None,
                },
            ])
        };
        let temp = tempfile::tempdir().unwrap();
        let questions = Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        agent.set_web_search_client(Arc::new(Recorder(Arc::clone(&questions))));
        let cancel = CancellationToken::new();
        let prompts = ["How does the\nLuhn check work?", "And for IBANs?"];
        for prompt in prompts {
            let llm = search_then_answer();
            agent.prompt(llm.as_ref(), prompt, &cancel).await.unwrap();
        }
        assert_eq!(
            *questions.lock().unwrap(),
            prompts.map(|prompt| Some(prompt.to_string()))
        );
        assert_eq!(agent.next_turn_tool_ctx().prompt, None);
    }

    /// A message steered while the turn runs reaches the next request of the
    /// same turn (Ctrl-S): the tool batch finishes, then the steered text is a
    /// user message in the following request.
    #[tokio::test]
    async fn a_steered_message_reaches_the_next_request_of_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new(vec![
            StepResult {
                message: Message::assistant_tool_calls(vec![
                    ToolCall::new("tc-1", "list_dir").with_arguments("{}"),
                ]),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::ToolCalls),
                response_id: None,
                stop_reason: None,
            },
            StepResult {
                message: Message::assistant_text("done"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            },
        ]);
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let llm = llm.steering_on_first_call(agent.steer_channel(), "steered mid-turn");

        agent
            .prompt(llm.as_ref(), "start", &CancellationToken::new())
            .await
            .unwrap();

        let requests = llm.messages.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "two model requests: the tool-call step and the one after it"
        );
        assert!(
            requests[0]
                .iter()
                .all(|m| m.text_content() != "steered mid-turn"),
            "the first request predates the steering"
        );
        assert!(
            requests[1].iter().any(|m| {
                m.role == lm_bridge::message::Role::User && m.text_content() == "steered mid-turn"
            }),
            "the steered message is a user message in the next request: {:?}",
            requests[1]
                .iter()
                .map(|m| (m.role, m.text_content()))
                .collect::<Vec<_>>()
        );
    }

    /// Input steered while the model's final request is in flight keeps the
    /// turn alive: the loop goes round once more so the message reaches the
    /// model instead of sitting until a later turn.
    #[tokio::test]
    async fn steering_at_the_models_stop_continues_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new(vec![
            StepResult {
                message: Message::assistant_text("first answer"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            },
            StepResult {
                message: Message::assistant_text("answer after steering"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            },
        ]);
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let llm = llm.steering_on_first_call(agent.steer_channel(), "one more thing");

        let result = agent
            .prompt(llm.as_ref(), "start", &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.text(), "answer after steering");
        let requests = llm.messages.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "the steered message turned the stop into another step"
        );
        assert!(
            requests[1].iter().any(|m| {
                m.role == lm_bridge::message::Role::User && m.text_content() == "one more thing"
            }),
            "the second request carries the steered message"
        );
    }

    /// A write cut off by the output limit must not run: its arguments can be
    /// valid JSON and still be half a file. The model is told why instead.
    #[tokio::test]
    async fn tool_calls_cut_off_by_the_output_limit_are_not_run() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("out.txt");
        let llm = MockLlm::new(vec![
            StepResult {
                message: Message::assistant_tool_calls(vec![
                    ToolCall::new("tc-1", "write_file").with_arguments(
                        serde_json::json!({ "path": target, "content": "half a fi" }).to_string(),
                    ),
                ]),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Length),
                response_id: None,
                stop_reason: None,
            },
            StepResult {
                message: Message::assistant_text("I will write it in smaller parts."),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            },
        ]);
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let result = agent
            .prompt(&llm, "write the file", &CancellationToken::new())
            .await
            .expect("the turn recovers");
        assert_eq!(result.text(), "I will write it in smaller parts.");
        assert!(!target.exists(), "a cut-off write must not run");
        let history = agent.history();
        assert!(
            history
                .iter()
                .all(|m| m.role != lm_bridge::message::Role::Tool),
            "no tool result: nothing ran"
        );
        assert!(
            history
                .iter()
                .any(|m| m.text_content().contains("hit the output-token limit")),
            "the model is told why"
        );
    }

    /// A model that keeps overrunning ends the turn instead of looping.
    #[tokio::test]
    async fn repeatedly_cut_off_tool_calls_end_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let truncated = || StepResult {
            message: Message::assistant_tool_calls(vec![
                ToolCall::new("tc-1", "list_dir").with_arguments("{}"),
            ]),
            usage: TokenUsage::empty(),
            finish_reason: Some(FinishReason::Length),
            response_id: None,
            stop_reason: None,
        };
        let llm = MockLlm::new(vec![truncated(), truncated(), truncated(), truncated()]);
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let err = agent
            .prompt(&llm, "list", &CancellationToken::new())
            .await
            .expect_err("three cut-off steps in a row end the turn");
        assert!(
            matches!(err, AgentError::TruncatedToolCalls { count: 3, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn test_loop_hooks_are_invoked() {
        let before_tools = Arc::new(Mutex::new(Vec::new()));
        let after_step_tools = Arc::new(Mutex::new(Vec::new()));
        let after_reasons = Arc::new(Mutex::new(Vec::new()));
        let before_tools_clone = before_tools.clone();
        let after_step_tools_clone = after_step_tools.clone();
        let after_reasons_clone = after_reasons.clone();

        let hooks = LoopHooks {
            before_step: Some(Arc::new(move |tools| {
                *before_tools_clone.lock().unwrap() =
                    tools.iter().map(|t| t.name.clone()).collect();
                tools.retain(|t| t.name != "bash");
                *after_step_tools_clone.lock().unwrap() =
                    tools.iter().map(|t| t.name.clone()).collect();
            })),
            after_step: Some(Arc::new(move |result| {
                after_reasons_clone
                    .lock()
                    .unwrap()
                    .push(result.finish_reason);
            })),
            should_continue_after_stop: Some(Arc::new(|_result| false)),
        };

        let llm = MockLlm::new(vec![StepResult {
            message: Message::assistant_text(""),
            usage: TokenUsage::empty(),
            finish_reason: Some(FinishReason::Stop),
            response_id: None,
            stop_reason: None,
        }]);

        let mut agent = Agent::new(std::path::PathBuf::from("."), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_loop_hooks(hooks);
        let cancel = CancellationToken::new();

        let _ = agent.prompt(&llm, "hi", &cancel).await.unwrap();

        assert!(
            before_tools.lock().unwrap().contains(&"bash".to_string()),
            "before_step should see the original tool list"
        );
        assert!(
            !after_step_tools
                .lock()
                .unwrap()
                .contains(&"bash".to_string()),
            "bash should have been removed by before_step"
        );
        assert_eq!(
            after_reasons.lock().unwrap().as_slice(),
            &[Some(FinishReason::Stop)]
        );
    }

    struct StreamingMockLlm;

    #[async_trait]
    impl Llm for StreamingMockLlm {
        fn model_name(&self) -> &str {
            "streaming-mock"
        }

        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            if let Some(cb) = callbacks.on_text_delta {
                cb("hello ");
                cb("world");
            }
            Ok(StepResult {
                message: Message::assistant_text(""),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn test_prompt_with_callbacks_streams_text_deltas() {
        let mut agent = Agent::new(std::path::PathBuf::from("."), "sys");
        let cancel = CancellationToken::new();
        let deltas = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured = deltas.clone();
        let callbacks = LlmCallbacks {
            on_text_delta: Some(std::sync::Arc::new(move |s: &str| {
                captured.lock().unwrap().push_str(s);
            })),
            ..Default::default()
        };

        agent
            .prompt_with_callbacks(&StreamingMockLlm, "hi", callbacks, &cancel)
            .await
            .unwrap();

        assert_eq!(deltas.lock().unwrap().as_str(), "hello world");
    }

    #[tokio::test]
    async fn test_tool_events_are_emitted() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("test.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let llm = MockLlm::new(vec![
            StepResult {
                message: Message::assistant_tool_calls(vec![
                    ToolCall::new("tc-1", "read_file")
                        .with_arguments(serde_json::json!({ "path": file_path }).to_string()),
                ]),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::ToolCalls),
                response_id: None,
                stop_reason: None,
            },
            StepResult {
                message: Message::assistant_text(""),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            },
        ]);

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let sender = std::sync::Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        });

        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_tool_event_sender(sender);
        let cancel = CancellationToken::new();

        agent.prompt(&llm, "read the file", &cancel).await.unwrap();

        let ids: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .map(|e: &crate::agent_loop::types::ToolEvent| e.id().to_string())
            .collect();
        assert!(ids.iter().any(|id| id == "tc-1"));
    }

    #[tokio::test]
    async fn test_unknown_tool_call_synthesizes_descriptive_result() {
        let temp = tempfile::tempdir().unwrap();
        let llm = MockLlm::new(vec![
            StepResult {
                message: Message::assistant_tool_calls(vec![ToolCall::new(
                    "tc-1",
                    "definitely_not_a_real_tool",
                )]),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::ToolCalls),
                response_id: None,
                stop_reason: None,
            },
            StepResult {
                message: Message::assistant_text(""),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            },
        ]);

        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let cancel = CancellationToken::new();

        let result = agent
            .prompt(&llm, "call something weird", &cancel)
            .await
            .unwrap();
        assert!(result.tool_calls().is_empty());

        let history = agent.history();
        let tool_results: Vec<_> = history
            .iter()
            .filter(|m| matches!(m.role, lm_bridge::message::Role::Tool))
            .collect();
        assert_eq!(tool_results.len(), 1);
        assert!(
            tool_results[0]
                .text_content()
                .contains(r#"Tool "definitely_not_a_real_tool" not found"#)
        );
    }

    #[tokio::test]
    async fn test_resume_replays_messages() {
        let mut agent = Agent::new(std::path::PathBuf::from("."), "sys");
        agent.resume(vec![
            Message::user("hello"),
            Message::assistant_text("hi there"),
        ]);

        let history = agent.history();
        assert_eq!(history.len(), 3); // system + 2 replayed
        assert_eq!(history[0].role, lm_bridge::message::Role::System);
        assert_eq!(history[1].role, lm_bridge::message::Role::User);
        assert_eq!(history[2].role, lm_bridge::message::Role::Assistant);
    }

    #[test]
    fn test_restrict_tools_limits_registry() {
        let mut agent = Agent::new_with_budget(std::path::PathBuf::from("."), "sys", 128_000);
        let allowed: std::collections::HashSet<String> =
            ["read_file".to_string(), "write_file".to_string()]
                .into_iter()
                .collect();
        agent.restrict_tools(&allowed);

        let names: std::collections::HashSet<String> = agent.list_tools().into_iter().collect();
        assert!(names.contains("read_file"));
        assert!(names.contains("write_file"));
        assert!(!names.contains("bash"));
    }

    /// A fresh agent has nothing to keep a headless run alive for; creating a
    /// goal or scheduling cron makes `has_outstanding_headless_work` true.
    #[test]
    fn headless_keepalive_tracks_goals_and_cron() {
        let agent = Agent::new_with_budget(std::path::PathBuf::from("."), "sys", 128_000);
        assert!(!agent.has_outstanding_headless_work());
        assert!(!agent.has_active_goal());
        assert_eq!(agent.pending_notifications(), 0);

        // An active goal keeps the run alive.
        agent
            .tool_ctx
            .goal_manager
            .lock()
            .unwrap()
            .create("ship the feature", None, false)
            .unwrap();
        assert!(agent.has_active_goal());
        assert!(agent.has_outstanding_headless_work());

        // A cron task keeps it alive too; a queued fire is a pending
        // notification worth a continuation turn.
        agent
            .tool_ctx
            .cron
            .add("*/5 * * * *", "check the queue", true)
            .unwrap();
        assert!(agent.has_outstanding_headless_work());
        agent.tool_ctx.cron.queue_fires(vec![due_fire()]);
        assert_eq!(agent.pending_notifications(), 1);
    }

    fn due_fire() -> crate::cron::CronFire {
        crate::cron::CronFire {
            job_id: "0123abcd".to_string(),
            cron: "*/5 * * * *".to_string(),
            recurring: true,
            coalesced_count: 1,
            stale: false,
            prompt: "check the queue".to_string(),
        }
    }

    /// `/new` drops every scheduled prompt and persists the empty state over
    /// the session's sidecar, so a later resume of this session sees no tasks.
    #[test]
    fn reset_cron_clears_tasks_and_the_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("records.jsonl");
        let agent =
            Agent::new(temp.path().to_path_buf(), "sys").with_records(Records::new(&records));
        agent
            .tool_ctx
            .cron
            .add("*/5 * * * *", "ping", true)
            .unwrap();
        agent.tool_ctx.cron.queue_fires(vec![due_fire()]);
        assert_eq!(agent.tool_ctx.cron.task_count(), 1);
        // A front-end's handle, taken before `/new`.
        let handle = Arc::clone(&agent.tool_ctx.cron);

        agent.reset_cron();
        assert_eq!(agent.tool_ctx.cron.task_count(), 0);
        assert_eq!(agent.tool_ctx.cron.pending_fire_count(), 0);
        assert!(
            Arc::ptr_eq(&handle, &agent.tool_ctx.cron),
            "the store is cleared in place, so earlier handles stay live"
        );
        let sidecar = crate::records::sidecar_path(&records, "cron");
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "[]");
    }

    /// A sub-agent fork continues the caller's conversation but not its
    /// scheduled work: the tasks stay with the source session. A sub-agent
    /// cannot schedule any, so it is not told to create new ones, as a session
    /// fork is.
    #[test]
    fn a_fork_leaves_cron_tasks_with_the_source() {
        let ctx = ExecutableToolContext::default();
        ctx.cron.add("*/5 * * * *", "ping", true).unwrap();
        let source = crate::agent_loop::types::ForkSource {
            system_prompt: "sys".to_string(),
            history: vec![],
        };

        let fork = crate::tools::builtin::subagent::spawn_fork(&ctx, source, None);
        assert_eq!(fork.tool_ctx().cron.task_count(), 0);
        assert_eq!(ctx.cron.task_count(), 1, "the source keeps its task");
        assert!(
            !fork.history().iter().any(|m| m
                .text_content()
                .contains(crate::cron::CRON_FORK_CLEARED_REMINDER)),
            "a sub-agent fork is never told to schedule"
        );
    }

    /// A sub-agent inherits the parent's mode through the tool context, so the
    /// context's mode must track the engine's — at construction, through the
    /// builder, and across a runtime switch. If it drifts, a sub-agent spawned
    /// after `/yolo` would run under the pre-switch mode.
    #[test]
    fn context_permission_mode_tracks_the_engine() {
        use crate::permissions::PermissionMode;

        let agent = Agent::new(std::path::PathBuf::from("."), "sys");
        assert_eq!(agent.permission_mode(), PermissionMode::Default);
        assert_eq!(agent.tool_ctx.permission_mode, PermissionMode::Default);

        let agent = agent.with_permission_mode(PermissionMode::BypassPermissions);
        assert_eq!(
            agent.tool_ctx.permission_mode,
            PermissionMode::BypassPermissions,
            "the builder must seed the context a sub-agent reads"
        );

        let mut agent = agent;
        agent.set_permission_mode(PermissionMode::AcceptEdits);
        assert_eq!(agent.permission_mode(), PermissionMode::AcceptEdits);
        assert_eq!(
            agent.tool_ctx.permission_mode,
            PermissionMode::AcceptEdits,
            "a runtime switch must reach the context too"
        );
    }

    #[test]
    fn test_goal_tools_hidden_when_no_goal_active() {
        let agent = Agent::new_with_budget(std::path::PathBuf::from("."), "sys", 128_000);
        let names: std::collections::HashSet<String> = agent.list_tools().into_iter().collect();
        assert!(names.contains("create_goal"));
        assert!(names.contains("get_goal"));
        assert!(!names.contains("update_goal"));
        assert!(!names.contains("set_goal_budget"));

        agent
            .tool_ctx()
            .goal_manager
            .lock()
            .unwrap()
            .create("test", Some("done".to_string()), false)
            .unwrap();
        let names: std::collections::HashSet<String> = agent.list_tools().into_iter().collect();
        assert!(names.contains("update_goal"));
        assert!(names.contains("set_goal_budget"));
    }

    #[test]
    fn test_new_with_budget_uses_custom_context_window() {
        let agent = Agent::new_with_budget(std::path::PathBuf::from("."), "sys", 8_192);
        // The system prompt plus no messages should fit inside the smaller budget.
        assert!(!agent.context.is_over_budget());
        assert_eq!(agent.context.max_tokens(), 8_192);
    }

    #[test]
    fn with_max_turns_overrides_default_and_clamps_zero() {
        let agent = Agent::new(std::path::PathBuf::from("."), "sys");
        assert_eq!(agent.max_turns, DEFAULT_MAX_TURNS);

        let agent = Agent::new(std::path::PathBuf::from("."), "sys").with_max_turns(7);
        assert_eq!(agent.max_turns, 7);

        // 0 would make the loop return `MaxTurnsExceeded` immediately, so it
        // clamps back to the default rather than wedging the agent.
        let agent = Agent::new(std::path::PathBuf::from("."), "sys").with_max_turns(0);
        assert_eq!(agent.max_turns, DEFAULT_MAX_TURNS);
    }

    /// A dynamic tool that does nothing, standing in for an MCP tool.
    struct Loadable(Tool);

    #[async_trait]
    impl crate::agent_loop::types::ExecutableTool for Loadable {
        fn tool_definition(&self) -> &Tool {
            &self.0
        }

        fn accesses(&self, _input: &serde_json::Value) -> crate::agent_loop::types::ToolAccesses {
            crate::agent_loop::types::ToolAccesses::all()
        }

        fn approval_rule(&self, _input: &serde_json::Value) -> &str {
            "silent"
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ExecutableToolContext,
            _tool_call_id: &str,
        ) -> crate::agent_loop::types::ExecutableToolResult {
            crate::agent_loop::types::ExecutableToolResult::text("ok")
        }
    }

    /// [`Loadable`], declared read-only as an MCP server's `readOnlyHint`
    /// declares a tool.
    struct Reading(Loadable);

    #[async_trait]
    impl crate::agent_loop::types::ExecutableTool for Reading {
        fn tool_definition(&self) -> &Tool {
            self.0.tool_definition()
        }

        fn accesses(&self, input: &serde_json::Value) -> crate::agent_loop::types::ToolAccesses {
            self.0.accesses(input)
        }

        fn approval_rule(&self, input: &serde_json::Value) -> &str {
            self.0.approval_rule(input)
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(
            &self,
            input: serde_json::Value,
            ctx: &ExecutableToolContext,
            tool_call_id: &str,
        ) -> crate::agent_loop::types::ExecutableToolResult {
            self.0.execute(input, ctx, tool_call_id).await
        }
    }

    /// A resumed session keeps what the original had: the tools it loaded
    /// with `select_tools`, which its requests must declare again, and the
    /// files it read, which it may edit without reading them first.
    #[tokio::test]
    async fn a_resumed_session_keeps_its_loaded_tools_and_its_reads() {
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("records.jsonl");
        let file = temp.path().join("lib.rs");
        std::fs::write(&file, "pub fn f() {}\n").unwrap();
        let loadable = || {
            Arc::new(Loadable(Tool::new(
                "mcp__code__search",
                "",
                serde_json::json!({"type": "object"}),
            )))
        };
        let step = |message: Message, finish: FinishReason| StepResult {
            message,
            usage: TokenUsage::empty(),
            finish_reason: Some(finish),
            response_id: None,
            stop_reason: None,
        };
        let llm = MockLlm::new(vec![step(
            Message::assistant_tool_calls(vec![
                ToolCall::new("tc-1", "select_tools")
                    .with_arguments(r#"{"names":["mcp__code__search"]}"#),
                ToolCall::new("tc-2", "read_file")
                    .with_arguments(serde_json::json!({ "path": file }).to_string()),
            ]),
            FinishReason::ToolCalls,
        )]);
        {
            let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
                .with_permission_mode(PermissionMode::BypassPermissions)
                .with_records(Records::new(&records));
            agent.register_dynamic_tool(loadable());
            agent
                .prompt(&llm, "look around", &CancellationToken::new())
                .await
                .unwrap();
        }

        let mut resumed = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_resumed_records(Records::new(&records))
            .await
            .unwrap();
        resumed.register_dynamic_tool(loadable());
        let offered = crate::agent_loop::run_turn::offered_tools(
            &resumed.registry,
            &resumed.tool_ctx,
            resumed.tool_ctx.tool_disclosure.as_ref().unwrap(),
        );
        assert!(
            offered.iter().any(|tool| tool.name == "mcp__code__search"),
            "the tool loaded before the resume is declared again"
        );
        let current = crate::stale_guard::FileStamp::of(&file).await;
        // Under the path the tools resolve it to, as a write checks it.
        let resolved = std::fs::canonicalize(&file).unwrap();
        assert_eq!(
            resumed
                .tool_ctx
                .stale_guard
                .lock()
                .unwrap()
                .check(&resolved, current),
            Ok(()),
            "the file read before the resume may be overwritten"
        );
    }

    /// A `coder` or `default` sub-agent gets the session's MCP tools — the
    /// deferred ones deferred, with `select_tools` to load them even under a
    /// restricted tool list — and the guidance for them; `explore` and `plan`
    /// get the read-only ones and the guidance for those.
    #[test]
    fn working_subagents_get_the_sessions_mcp_tools() {
        use crate::tools::builtin::subagent::spawn_with_profile;
        let tool =
            |name: &str| Loadable(Tool::new(name, "", serde_json::json!({"type": "object"})));
        let mut parent = Agent::new(std::env::temp_dir(), "sys");
        parent.register_dynamic_tool(Arc::new(Reading(tool("mcp__code__search"))));
        parent.register_dynamic_tool(Arc::new(tool("mcp__code__rename")));
        parent.register_tool(Arc::new(tool("mcp__small__ping")));
        parent.set_subagent_mcp_guidance(Some(crate::McpGuidance {
            all: "# Devscriptor code intelligence\nUse it.".into(),
            read_only: "# Devscriptor code intelligence\nRead with it.".into(),
        }));
        let mut profiles = ProfileRegistry::with_builtin_profiles();
        profiles.register(crate::Profile {
            name: "narrow".into(),
            allowed_tools: vec!["read_file".into()],
            mcp_tools: Some(McpTools::All),
            ..Default::default()
        });
        let ctx = parent.tool_ctx.clone();

        for name in ["coder", "default"] {
            let child = spawn_with_profile(&ctx, &profiles.resolve(name).unwrap(), None);
            let reg = child.registry.lock().unwrap();
            assert!(reg.is_dynamic("mcp__code__search"), "{name}");
            assert!(reg.is_dynamic("mcp__code__rename"), "{name}");
            assert!(reg.get("mcp__small__ping").is_some(), "{name}");
            assert!(!reg.is_dynamic("mcp__small__ping"), "{name}");
            assert!(child.context.system_prompt().contains("Use it."), "{name}");
        }
        for name in ["explore", "plan"] {
            let child = spawn_with_profile(&ctx, &profiles.resolve(name).unwrap(), None);
            let reg = child.registry.lock().unwrap();
            assert!(reg.is_dynamic("mcp__code__search"), "{name}");
            assert!(reg.get("mcp__code__rename").is_none(), "{name}");
            assert!(reg.get("mcp__small__ping").is_none(), "{name}");
            assert!(
                reg.get("select_tools").is_some(),
                "{name} loads the deferred"
            );
            let prompt = child.context.system_prompt();
            assert!(prompt.contains("Read with it."), "{name}");
            assert!(!prompt.contains("Use it."), "{name}");
        }

        let narrow = spawn_with_profile(&ctx, &profiles.resolve("narrow").unwrap(), None);
        let reg = narrow.registry.lock().unwrap();
        assert!(reg.get("read_file").is_some());
        assert!(reg.get("bash").is_none(), "the profile's list still holds");
        assert!(reg.is_dynamic("mcp__code__search"));
        assert!(
            reg.get("select_tools").is_some(),
            "deferred tools need select_tools"
        );
    }

    /// A coder or default sub-agent gets the session's skills through its own
    /// `skill` tool; explore and plan, whose tool lists leave it out, do not.
    /// A coder had no `skill` tool at all, and looked for a skill's file on
    /// disk, outside its workspace.
    #[test]
    fn working_subagents_get_the_sessions_skills() {
        use crate::tools::builtin::subagent::spawn_with_profile;
        let parent = Agent::new(std::env::temp_dir(), "sys").with_skill_registry(greet_skill());
        let profiles = ProfileRegistry::with_builtin_profiles();
        for name in ["coder", "default"] {
            let child =
                spawn_with_profile(&parent.tool_ctx, &profiles.resolve(name).unwrap(), None);
            let reg = child.registry.lock().unwrap();
            let skill = reg
                .get("skill")
                .unwrap_or_else(|| panic!("{name} has no skill tool"));
            assert!(skill.is_available(&child.tool_ctx), "{name}");
            assert!(
                skill.tool_definition().description.contains("greet"),
                "{name}: {}",
                skill.tool_definition().description
            );
        }
        for name in ["explore", "plan"] {
            let child =
                spawn_with_profile(&parent.tool_ctx, &profiles.resolve(name).unwrap(), None);
            assert!(
                child.registry.lock().unwrap().get("skill").is_none(),
                "{name}"
            );
        }
    }

    /// A sub-agent gets the session's prompt with its own role in the slot,
    /// and the guidance for its MCP tools after it; so does a sub-agent's
    /// own sub-agent. Without the session's prompt, sub-agents would know
    /// none of its rules, such as how its file tools are meant to be used.
    #[test]
    fn a_subagent_gets_the_sessions_prompt_with_its_role() {
        use crate::tools::builtin::subagent::spawn_with_profile;
        let mut parent = Agent::new(std::env::temp_dir(), "sys");
        let session =
            crate::prompt::SubagentPrompt::new("You are Rusta.\n", "\n# Tool use\n\nRead first.\n");
        parent.set_subagent_prompt(Some(session.clone()));
        parent.set_subagent_mcp_guidance(Some(crate::McpGuidance {
            all: "# Devscriptor code intelligence\nUse it.".into(),
            read_only: "# Devscriptor code intelligence\nRead with it.".into(),
        }));
        let profiles = ProfileRegistry::with_builtin_profiles();
        let coder = profiles.resolve("coder").unwrap();
        let role = coder.system_prompt.clone().unwrap();

        let child = spawn_with_profile(&parent.tool_ctx, &coder, None);
        assert_eq!(
            child.context.system_prompt(),
            format!(
                "{}\n\n# Devscriptor code intelligence\nUse it.\n",
                session.render(&role).trim_end()
            )
        );
        let grandchild = spawn_with_profile(&child.tool_ctx, &coder, None);
        assert_eq!(
            grandchild.context.system_prompt(),
            child.context.system_prompt()
        );
    }

    /// A sub-agent asks the user through its parent's hook, naming itself, and
    /// is held to its parent's rules.
    #[tokio::test]
    async fn a_subagent_asks_through_its_parent_under_its_rules() {
        use crate::permissions::{
            ApprovalRequest, ApprovalResponse, PolicyAction, PolicyCheck, PolicyRule, PolicyScope,
        };
        #[derive(Default)]
        struct Recording(std::sync::Mutex<Vec<(String, Option<String>)>>);
        #[async_trait]
        impl ApprovalHook for Recording {
            async fn request_approval(&self, request: ApprovalRequest) -> ApprovalResponse {
                self.0
                    .lock()
                    .unwrap()
                    .push((request.tool_name.clone(), request.requester.clone()));
                ApprovalResponse::AllowOnce
            }
        }
        let hook = Arc::new(Recording::default());
        let parent = Agent::new(std::env::temp_dir(), "sys")
            .with_permission_mode(PermissionMode::Default)
            .with_approval_hook(hook.clone())
            .with_permission_rules(vec![PolicyRule {
                name: Some("no-rm".into()),
                tool: Some("bash".into()),
                path: None,
                command: Some("^rm ".into()),
                action: PolicyAction::Deny,
                scope: PolicyScope::Once,
            }]);
        let mut child = Agent::spawn_child(&parent.tool_ctx, "sub", None);
        child.set_approval_requester(Some("sub-1".into()));

        assert_eq!(
            child.permissions.check_policy(
                "bash",
                &serde_json::json!({"command": "rm -rf build"}),
                &child.tool_ctx
            ),
            PolicyCheck::Deny
        );
        let answer = child
            .permissions
            .request_approval("run_command", "bash", &serde_json::json!({"command": "ls"}))
            .await;
        assert_eq!(answer, ApprovalResponse::AllowOnce);
        assert_eq!(
            hook.0.lock().unwrap().as_slice(),
            [("bash".to_string(), Some("sub-1".to_string()))]
        );
    }

    /// In swarm mode, a step that pairs `agent_swarm` with another call is
    /// refused whole — every call gets the refusal — and the turn
    /// goes on.
    #[tokio::test]
    async fn a_swarm_paired_with_another_call_is_refused_whole() {
        let llm = MockLlm::new(vec![StepResult {
            message: Message::assistant_tool_calls(vec![
                ToolCall::new("tc-1", "agent_swarm").with_arguments(r#"{"description": "d"}"#),
                ToolCall::new("tc-2", "read_file").with_arguments(r#"{"path": "x"}"#),
            ]),
            usage: TokenUsage::empty(),
            finish_reason: Some(FinishReason::ToolCalls),
            response_id: None,
            stop_reason: None,
        }]);
        let mut agent = Agent::new(std::env::temp_dir(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        agent.set_swarm_mode(Some(crate::swarm::SwarmTrigger::Manual));
        agent
            .prompt(&llm, "go", &CancellationToken::new())
            .await
            .unwrap();
        let results: Vec<String> = agent
            .history()
            .iter()
            .filter(|m| m.role == lm_bridge::message::Role::Tool)
            .map(Message::text_content)
            .collect();
        assert_eq!(results.len(), 2, "{results:?}");
        assert!(
            results
                .iter()
                .all(|r| r
                    .starts_with("agent_swarm must be the only tool call in a model response.")),
            "{results:?}"
        );
    }

    /// Only the user turns swarm mode on. Outside it,
    /// `agent_swarm` starts no sub-agent and says why, and the calls beside
    /// it run rather than the step being refused whole; a `/swarm <task>`
    /// ends with its prompt. In swarm mode the same call goes on past that.
    #[tokio::test]
    async fn a_swarm_runs_only_in_swarm_mode() {
        use crate::swarm::SwarmTrigger;
        use crate::tools::builtin::agent_swarm::SWARM_MODE_OFF;
        let swarm =
            || ToolCall::new("tc-1", "agent_swarm").with_arguments(r#"{"description": "d"}"#);
        let step = |calls: Vec<ToolCall>| StepResult {
            message: Message::assistant_tool_calls(calls),
            usage: TokenUsage::empty(),
            finish_reason: Some(FinishReason::ToolCalls),
            response_id: None,
            stop_reason: None,
        };
        let results = |agent: &Agent| -> Vec<String> {
            agent
                .history()
                .iter()
                .filter(|m| m.role == lm_bridge::message::Role::Tool)
                .map(Message::text_content)
                .collect()
        };
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("x"), "the file").unwrap();
        let cancel = CancellationToken::new();

        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let llm = MockLlm::new(vec![step(vec![
            swarm(),
            ToolCall::new("tc-2", "read_file").with_arguments(r#"{"path": "x"}"#),
        ])]);
        agent.prompt(&llm, "go", &cancel).await.unwrap();
        let off = results(&agent);
        assert_eq!(off.len(), 2, "{off:?}");
        assert_eq!(off[0], SWARM_MODE_OFF);
        assert!(off[1].contains("the file"), "read_file ran: {off:?}");

        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        agent.set_swarm_mode(Some(SwarmTrigger::Task));
        agent
            .prompt(&MockLlm::new(Vec::new()), "the task", &cancel)
            .await
            .unwrap();
        let llm = MockLlm::new(vec![step(vec![swarm()])]);
        agent.prompt(&llm, "go", &cancel).await.unwrap();
        assert_eq!(
            results(&agent),
            [SWARM_MODE_OFF],
            "the task's swarm mode ended"
        );

        agent.set_swarm_mode(Some(SwarmTrigger::Manual));
        let llm = MockLlm::new(vec![step(vec![swarm()])]);
        agent.prompt(&llm, "go", &cancel).await.unwrap();
        let on = results(&agent);
        assert_eq!(on.len(), 2, "{on:?}");
        assert_ne!(on[1], SWARM_MODE_OFF, "in swarm mode the call goes on");
    }

    /// Swarm mode is said once when it starts and once when it ends; a
    /// task's swarm mode ends with its prompt.
    #[tokio::test]
    async fn swarm_mode_is_said_when_it_starts_and_when_it_ends() {
        use crate::swarm::SwarmTrigger;
        let llm = MockLlm::new(Vec::new());
        let cancel = CancellationToken::new();
        let mut agent = Agent::new(std::env::temp_dir(), "sys");
        let said = |agent: &Agent, marker: &str| {
            agent
                .history()
                .iter()
                .map(|m| m.text_content().matches(marker).count())
                .sum::<usize>()
        };
        let entry = "You are now in \"agent swarm\" mode";
        let exit = "## Swarm Mode Ended";

        agent.set_swarm_mode(Some(SwarmTrigger::Manual));
        agent.prompt(&llm, "one", &cancel).await.unwrap();
        agent.prompt(&llm, "two", &cancel).await.unwrap();
        assert_eq!(said(&agent, entry), 1, "said once while it stays on");
        assert_eq!(agent.swarm_mode(), Some(SwarmTrigger::Manual));

        agent.set_swarm_mode(None);
        agent.prompt(&llm, "three", &cancel).await.unwrap();
        agent.prompt(&llm, "four", &cancel).await.unwrap();
        assert_eq!(said(&agent, exit), 1, "its end is said once");

        agent.set_swarm_mode(Some(SwarmTrigger::Task));
        agent.prompt(&llm, "the task", &cancel).await.unwrap();
        assert_eq!(said(&agent, entry), 2);
        assert_eq!(agent.swarm_mode(), None, "a task's swarm mode ends with it");
        agent.prompt(&llm, "after", &cancel).await.unwrap();
        assert_eq!(said(&agent, exit), 2);
    }

    #[tokio::test]
    async fn test_resume_marks_messages_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let records = Records::new(temp.path().join("records.jsonl"));

        let _agent = Agent::new(std::path::PathBuf::from("."), "sys")
            .with_resumed_records(records)
            .await
            .unwrap();

        // No messages to persist yet; should not create duplicate entries.
        let content = tokio::fs::read_to_string(temp.path().join("records.jsonl"))
            .await
            .unwrap_or_default();
        assert!(content.is_empty());
    }

    use std::time::Duration;

    /// Spawns a sub-agent that finishes after `delay`, on a manager rooted in
    /// `data_dir` so the test leaves nothing behind.
    fn spawn_slow_subagent(agent: &mut Agent, data_dir: &std::path::Path, delay: Duration) {
        let background = Arc::new(crate::background::BackgroundManager::with_data_dir(
            data_dir.to_path_buf(),
        ));
        agent.tool_ctx.background = Arc::clone(&background);
        background.spawn_agent(
            "explore",
            agent.tool_ctx.cwd.clone(),
            move |_cancel| async move {
                tokio::time::sleep(delay).await;
                Ok("subagent finding: 42".to_string())
            },
        );
    }

    /// Headless: the model stops, but a sub-agent is still running. The turn
    /// must hold, then hand the model what the sub-agent found.
    #[tokio::test]
    async fn drain_on_stop_waits_for_subagent_and_feeds_back_its_completion() {
        let temp = tempfile::tempdir().unwrap();
        let llm = MockLlm::new(Vec::new()); // every call returns Stop
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        agent.set_drain_agent_tasks_on_stop(true);
        spawn_slow_subagent(&mut agent, temp.path(), Duration::from_millis(150));

        let cancel = CancellationToken::new();
        agent.prompt(&llm, "delegate", &cancel).await.unwrap();

        assert_eq!(
            agent.tool_ctx.background.active_agent_task_count(),
            0,
            "the turn must not end while a sub-agent is still running"
        );
        let history = agent.history();
        assert!(
            history.iter().any(|m| m.text_content().contains("explore")),
            "the sub-agent's completion must reach the model, got: {:?}",
            history.iter().map(|m| m.text_content()).collect::<Vec<_>>()
        );
    }

    /// Interactive: the user is still at the keyboard and the next prompt picks
    /// completions up, so the turn must not block on sub-agents.
    #[tokio::test]
    async fn without_drain_the_turn_returns_while_the_subagent_runs() {
        let temp = tempfile::tempdir().unwrap();
        let llm = MockLlm::new(Vec::new());
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        // drain_agent_tasks_on_stop defaults to false.
        spawn_slow_subagent(&mut agent, temp.path(), Duration::from_secs(30));

        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        agent.prompt(&llm, "delegate", &cancel).await.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the turn must return immediately without draining"
        );
        assert_eq!(agent.tool_ctx.background.active_agent_task_count(), 1);
    }

    /// A detached shell command is meant to outlive the turn; only sub-agents
    /// hold it open.
    #[tokio::test]
    async fn drain_on_stop_ignores_shell_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let llm = MockLlm::new(Vec::new());
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        agent.set_drain_agent_tasks_on_stop(true);

        let background = Arc::new(crate::background::BackgroundManager::with_data_dir(
            temp.path().to_path_buf(),
        ));
        agent.tool_ctx.background = Arc::clone(&background);
        background.spawn(
            "sleep 30",
            temp.path().to_path_buf(),
            std::collections::HashMap::new(),
            None,
        );

        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        agent.prompt(&llm, "start a server", &cancel).await.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a background shell command must not hold the turn open"
        );
        background.stop(&background.list()[0]);
    }

    /// Ctrl-C during a drain must not hang.
    #[tokio::test]
    async fn drain_on_stop_honors_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let llm = MockLlm::new(Vec::new());
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        agent.set_drain_agent_tasks_on_stop(true);
        spawn_slow_subagent(&mut agent, temp.path(), Duration::from_secs(30));

        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let started = std::time::Instant::now();
        let err = agent.prompt(&llm, "delegate", &cancel).await.unwrap_err();
        assert!(matches!(err, AgentError::Interrupted), "got {err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A mock that counts how many times the loop asked the model.
    struct CountingLlm {
        first: std::sync::Mutex<Option<StepResult>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Llm for CountingLlm {
        fn model_name(&self) -> &str {
            "counting"
        }
        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(first) = self.first.lock().unwrap().take() {
                return Ok(first);
            }
            Ok(StepResult {
                message: Message::assistant_text(""),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }

        /// Compaction's summary request is not a loop step, so it is not
        /// counted.
        async fn summarize(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _max_output_tokens: Option<u32>,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            Ok(StepResult {
                message: Message::assistant_text("summary"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    fn step_with(finish_reason: Option<FinishReason>) -> StepResult {
        StepResult {
            message: Message::assistant_text(""),
            usage: TokenUsage::empty(),
            finish_reason,
            response_id: None,
            stop_reason: None,
        }
    }

    async fn calls_for(finish_reason: Option<FinishReason>) -> usize {
        let temp = tempfile::tempdir().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let llm = CountingLlm {
            first: std::sync::Mutex::new(Some(step_with(finish_reason))),
            calls: Arc::clone(&calls),
        };
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let cancel = CancellationToken::new();
        agent.prompt(&llm, "hi", &cancel).await.unwrap();
        calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A turn that is not a tool-call turn ends, whatever its finish reason:
    /// `length`, `sensitive`, and a reason Rusta cannot parse must not send
    /// the loop round again up to the turn cap.
    #[tokio::test]
    async fn a_non_tool_call_turn_never_spins_the_loop() {
        for fr in [
            Some(FinishReason::Stop),
            Some(FinishReason::Length),
            Some(FinishReason::ContentFilter),
            None, // an unrecognized reason from the provider
        ] {
            assert_eq!(
                calls_for(fr).await,
                1,
                "finish_reason {fr:?} must end the turn"
            );
        }
    }

    /// Answers once, noting whether the session's lock file existed while
    /// it was asked.
    struct LockWatchingLlm {
        lock: std::path::PathBuf,
        held_while_asked: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Llm for LockWatchingLlm {
        fn model_name(&self) -> &str {
            "lock-watching"
        }

        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            self.held_while_asked
                .store(self.lock.exists(), Ordering::SeqCst);
            Ok(StepResult {
                message: Message::assistant_text("done"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    fn locked_agent(dir: &std::path::Path, records: &std::path::Path) -> Agent {
        let locks = Arc::new(crate::session_lock::SessionLocks::new("rusta test"));
        Agent::new(dir.to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_records(Records::new(records))
            .with_session_lock(locks, "default")
    }

    /// Counts the records file's lines at each request, then answers from
    /// a script.
    struct RecordsPeekingLlm {
        records: std::path::PathBuf,
        seen: std::sync::Mutex<Vec<usize>>,
        steps: std::sync::Mutex<Vec<StepResult>>,
    }

    #[async_trait]
    impl Llm for RecordsPeekingLlm {
        fn model_name(&self) -> &str {
            "peeking"
        }

        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            let lines = std::fs::read_to_string(&self.records)
                .map(|text| text.lines().count())
                .unwrap_or(0);
            self.seen.lock().unwrap().push(lines);
            Ok(self.steps.lock().unwrap().remove(0))
        }
    }

    /// Each step is on disk before the next request: a turn killed mid-way
    /// (SIGKILL, a crash) keeps the steps it took. Only the turn's end wrote
    /// them, so a two-hour turn killed in its last minute left nothing.
    #[tokio::test]
    async fn each_step_is_recorded_before_the_next_request() {
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("default.jsonl");
        let call = |id: &str| StepResult {
            message: Message::assistant_tool_calls(vec![
                ToolCall::new(id, "list_dir").with_arguments("{}"),
            ]),
            usage: TokenUsage::empty(),
            finish_reason: Some(FinishReason::ToolCalls),
            response_id: None,
            stop_reason: None,
        };
        let llm = RecordsPeekingLlm {
            records: records.clone(),
            seen: std::sync::Mutex::new(Vec::new()),
            steps: std::sync::Mutex::new(vec![
                call("tc-1"),
                call("tc-2"),
                StepResult {
                    message: Message::assistant_text("done"),
                    usage: TokenUsage::empty(),
                    finish_reason: Some(FinishReason::Stop),
                    response_id: None,
                    stop_reason: None,
                },
            ]),
        };
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_records(Records::new(&records));
        agent
            .prompt(&llm, "look around", &CancellationToken::new())
            .await
            .unwrap();
        let seen = llm.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert!(
            seen[1] >= 3,
            "the prompt, the call and its result: {seen:?}"
        );
        assert!(seen[2] > seen[1], "the second step too: {seen:?}");
    }

    /// A turn holds the session's lock, beside its records file, while it
    /// runs, and lets it go when it ends.
    #[tokio::test]
    async fn a_turn_holds_the_session_lock_while_it_runs() {
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("default.jsonl");
        let lock = temp.path().join("default.lock");
        let llm = LockWatchingLlm {
            lock: lock.clone(),
            held_while_asked: std::sync::atomic::AtomicBool::new(false),
        };
        let mut agent = locked_agent(temp.path(), &records);
        agent
            .prompt(&llm, "hello", &CancellationToken::new())
            .await
            .unwrap();
        assert!(llm.held_while_asked.load(Ordering::SeqCst));
        assert!(!lock.exists(), "the lock goes with the turn");
    }

    /// A session another live process is writing is refused before anything
    /// is sent or written.
    #[tokio::test]
    async fn a_session_another_process_writes_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("default.jsonl");
        // Another host's lock is taken to be alive.
        std::fs::write(
            temp.path().join("default.lock"),
            serde_json::json!({
                "version": 1, "token": "t", "pid": 1, "hostname": "elsewhere.example",
                "started_at": 0, "client": "rusta repl"
            })
            .to_string(),
        )
        .unwrap();
        let llm = RecordingLlm {
            requests: std::sync::Mutex::new(Vec::new()),
            summaries: std::sync::Mutex::new(Vec::new()),
        };
        let mut agent = locked_agent(temp.path(), &records);
        let error = agent
            .prompt(&llm, "hello", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::SessionOccupied(_)), "{error}");
        assert!(
            error
                .to_string()
                .contains("in use by another Rusta process, rusta repl"),
            "{error}"
        );
        assert!(llm.requests.lock().unwrap().is_empty());
        assert!(!records.exists(), "nothing was written");
    }

    /// A fresh session under a file an earlier session wrote does not take
    /// the earlier one's history for another process's turns.
    #[tokio::test]
    async fn a_fresh_session_leaves_an_earlier_one_alone() {
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("default.jsonl");
        Records::new(&records)
            .append(&Message::user("an earlier session"))
            .await
            .unwrap();
        let llm = RecordingLlm {
            requests: std::sync::Mutex::new(Vec::new()),
            summaries: std::sync::Mutex::new(Vec::new()),
        };
        let mut agent = locked_agent(temp.path(), &records);
        agent
            .prompt(&llm, "fresh", &CancellationToken::new())
            .await
            .unwrap();
        let requests = llm.requests.lock().unwrap();
        assert!(
            !requests[0]
                .iter()
                .any(|m| m.text_content().contains("an earlier session")),
            "{:?}",
            requests[0]
        );
    }

    /// What another process wrote to the session since this one's last turn
    /// is in the history of the next.
    #[tokio::test]
    async fn another_processs_turn_is_reloaded_before_the_next() {
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("default.jsonl");
        let llm = RecordingLlm {
            requests: std::sync::Mutex::new(Vec::new()),
            summaries: std::sync::Mutex::new(Vec::new()),
        };
        let mut agent = locked_agent(temp.path(), &records);
        agent
            .prompt(&llm, "first", &CancellationToken::new())
            .await
            .unwrap();

        let theirs = Records::new(&records);
        theirs
            .append(&Message::user("a question from elsewhere"))
            .await
            .unwrap();
        theirs
            .append(&Message::assistant_text("an answer from elsewhere"))
            .await
            .unwrap();

        agent
            .prompt(&llm, "second", &CancellationToken::new())
            .await
            .unwrap();
        let requests = llm.requests.lock().unwrap();
        let texts: Vec<String> = requests[1].iter().map(Message::text_content).collect();
        let at = |text: &str| texts.iter().position(|t| t.contains(text));
        assert!(at("first") < at("a question from elsewhere"), "{texts:?}");
        assert!(at("an answer from elsewhere") < at("second"), "{texts:?}");
        // Each message once: the reload replaced the history.
        assert_eq!(texts.iter().filter(|t| t.contains("first")).count(), 1);
    }

    /// Records every chat request and answers it; writes a fixed summary.
    struct RecordingLlm {
        requests: std::sync::Mutex<Vec<Vec<Message>>>,
        /// Requests made to `summarize`, in order.
        summaries: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    impl Llm for RecordingLlm {
        fn model_name(&self) -> &str {
            "recording"
        }

        async fn chat(
            &self,
            messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            self.requests.lock().unwrap().push(messages);
            Ok(StepResult {
                message: Message::assistant_text("tests added"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }

        async fn summarize(
            &self,
            messages: Vec<Message>,
            _tools: Vec<Tool>,
            _max_output_tokens: Option<u32>,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            self.summaries.lock().unwrap().push(messages);
            Ok(StepResult {
                message: Message::assistant_text("Ported the lexer; tests remain."),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    /// Past 90 % of the window, a turn compacts before its request: the model
    /// then sees the task, the latest request, the summary and, last, the
    /// continuation anchor — and the front-end hears about both ends of it.
    #[tokio::test]
    async fn a_turn_compacts_proactively_before_its_request() {
        let temp = tempfile::tempdir().unwrap();
        let mut agent = Agent::new_with_budget(temp.path().to_path_buf(), "sys", 4_000)
            .with_permission_mode(PermissionMode::BypassPermissions);
        let mut history = vec![Message::user("the task: port the parser")];
        for i in 0..12 {
            let id = format!("tc{i}");
            history.push(Message::assistant_tool_calls(vec![ToolCall::new(
                &id, "bash",
            )]));
            history.push(Message::tool_result(&id, "log line\n".repeat(150)));
        }
        agent.resume(history);

        let notices = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&notices);
        let callbacks = LlmCallbacks {
            on_compaction: Some(Arc::new(
                move |notice: &crate::agent_loop::llm::CompactionNotice| {
                    seen.lock().unwrap().push(notice.clone());
                },
            )),
            ..Default::default()
        };
        let llm = RecordingLlm {
            requests: std::sync::Mutex::new(Vec::new()),
            summaries: std::sync::Mutex::new(Vec::new()),
        };
        let cancel = CancellationToken::new();
        let step = agent
            .prompt_with_callbacks(&llm, "now add tests", callbacks, &cancel)
            .await
            .unwrap();
        assert_eq!(step.message.text_content(), "tests added");

        let notices = notices.lock().unwrap().clone();
        assert!(
            matches!(
                notices.as_slice(),
                [
                    crate::agent_loop::llm::CompactionNotice::Started {
                        trigger: crate::agent_loop::llm::CompactionTrigger::Proactive,
                        ..
                    },
                    crate::agent_loop::llm::CompactionNotice::Summarized { .. },
                ]
            ),
            "{notices:?}"
        );

        // The one chat request went out compacted. Adjacent user messages are
        // merged on the wire, so the task, the latest request, the summary
        // and the anchor arrive as one user message, in that order. (A
        // trailing system message may follow: the empty-loadable-tools
        // announcement, given once per session.)
        let requests = llm.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(
            !request
                .iter()
                .any(|m| m.role == lm_bridge::message::Role::Tool)
        );
        let last = request
            .iter()
            .rev()
            .find(|m| m.role == lm_bridge::message::Role::User)
            .unwrap()
            .text_content();
        let order = [
            "the task: port the parser",
            "now add tests",
            "Ported the lexer; tests remain.",
            crate::context_compaction::COMPACTION_CONTINUATION_TEXT,
        ]
        .map(|part| {
            last.find(part)
                .unwrap_or_else(|| panic!("{part} missing: {last}"))
        });
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        assert!(last.ends_with(crate::context_compaction::COMPACTION_CONTINUATION_TEXT));
    }

    /// `/compact <instruction>`: the focus text reaches the summarizer as an
    /// "Optional user instruction" block on the handoff instruction.
    #[tokio::test]
    async fn compact_instruction_reaches_the_summarizer() {
        let temp = tempfile::tempdir().unwrap();
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys");
        agent.resume(vec![
            Message::user("port the parser"),
            Message::assistant_text("parser done"),
            Message::user("now the lexer"),
            Message::assistant_text("lexer done"),
        ]);
        let llm = RecordingLlm {
            requests: std::sync::Mutex::new(Vec::new()),
            summaries: std::sync::Mutex::new(Vec::new()),
        };
        let cancel = CancellationToken::new();
        agent
            .compact_context(&llm, &cancel, Some("keep the auth notes"))
            .await
            .unwrap();

        let summaries = llm.summaries.lock().unwrap();
        assert_eq!(summaries.len(), 1, "one summary request: {summaries:?}");
        let request = &summaries[0];
        let instruction = request.last().unwrap().text_content();
        assert!(
            instruction.ends_with("Optional user instruction:\nkeep the auth notes"),
            "the focus instruction ends the handoff instruction: {instruction}"
        );
    }

    /// Without an argument the summary request carries no instruction block.
    #[tokio::test]
    async fn compact_without_instruction_omits_the_block() {
        let temp = tempfile::tempdir().unwrap();
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys");
        agent.resume(vec![
            Message::user("port the parser"),
            Message::assistant_text("parser done"),
        ]);
        let llm = RecordingLlm {
            requests: std::sync::Mutex::new(Vec::new()),
            summaries: std::sync::Mutex::new(Vec::new()),
        };
        let cancel = CancellationToken::new();
        agent.compact_context(&llm, &cancel, None).await.unwrap();

        let summaries = llm.summaries.lock().unwrap();
        assert_eq!(summaries.len(), 1);
        let instruction = summaries[0].last().unwrap().text_content();
        assert!(
            !instruction.contains("Optional user instruction"),
            "{instruction}"
        );
    }

    /// `/undo <count>` at the agent: several turns go at once, and a count
    /// past what is undoable is an error that removes nothing.
    #[test]
    fn undo_turns_removes_several_turns_and_checks_first() {
        let temp = tempfile::tempdir().unwrap();
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys");
        agent.resume(vec![
            Message::user("first"),
            Message::assistant_text("answer one"),
            Message::user("second"),
            Message::assistant_text("answer two"),
        ]);

        let err = agent.undo_turns(3).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Nothing to undo: only 2 of 3 requested turn(s) available"
        );
        assert_eq!(agent.context.undoable_turns(), 2, "the error undid nothing");

        assert_eq!(agent.undo_turns(2).unwrap(), 4);
        assert_eq!(agent.context.undoable_turns(), 0);
    }

    /// Rejects the first request as too large, then answers; records what was
    /// sent.
    struct TooLargeOnceLlm {
        requests: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    impl Llm for TooLargeOnceLlm {
        fn model_name(&self) -> &str {
            "too-large-once"
        }

        async fn chat(
            &self,
            messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            let mut requests = self.requests.lock().unwrap();
            requests.push(messages);
            if requests.len() == 1 {
                return Err(lm_bridge::errors::LlmError::ApiRequestTooLarge(
                    "message size exceeds limit 2097152".into(),
                ));
            }
            Ok(StepResult {
                message: Message::assistant_text("seen"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    /// A request rejected as too large is retried without its oldest media,
    /// instead of failing the same way on every turn after.
    #[tokio::test]
    async fn a_too_large_request_drops_old_media_and_retries() {
        let temp = tempfile::tempdir().unwrap();
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let mut screenshot = Message::user("");
        screenshot.content = vec![
            lm_bridge::message::ContentPart::text(
                "<system>Image file: /shots/big.png\nimage/png, 9 bytes.</system>",
            ),
            lm_bridge::message::ContentPart::ImageUrl {
                image_url: lm_bridge::message::ImageUrl {
                    url: "data:image/png;base64,AAAAAAAA".into(),
                    id: None,
                },
            },
        ];
        agent.resume(vec![screenshot]);
        let llm = TooLargeOnceLlm {
            requests: std::sync::Mutex::new(Vec::new()),
        };
        let step = agent
            .prompt(&llm, "what was in it?", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(step.message.text_content(), "seen");
        let requests = llm.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let retried: String = requests[1].iter().map(Message::text_content).collect();
        assert!(
            retried.contains("[image omitted to fit the request size limit — /shots/big.png]"),
            "{retried}"
        );
    }

    /// Z.AI reports an overrun context on a successful response, so the error
    /// path never sees it. The loop must compact and try again rather than
    /// return the truncated reply.
    #[tokio::test]
    async fn provider_reported_context_overflow_compacts_and_retries() {
        let calls = calls_for(Some(FinishReason::ContextWindowExceeded)).await;
        assert_eq!(calls, 2, "must retry once after compacting");
    }

    /// A model that reserves output, as DeepSeek does, of the window it
    /// declares.
    struct ReservingLlm(Option<u64>);

    #[async_trait]
    impl Llm for ReservingLlm {
        fn model_name(&self) -> &str {
            "reserving"
        }

        fn output_reservation(&self) -> Option<u64> {
            self.0
        }

        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            unreachable!("the budget is read, no request is made")
        }
    }

    /// The context budget keeps the attached model's output reservation
    /// free: DeepSeek's 256K of its 1M window moves compaction to 786,432
    /// tokens, a model without one stays at 90 %, and a model switch or a new
    /// window re-derives it.
    #[test]
    fn the_context_budget_keeps_the_models_output_reservation_free() {
        let temp = tempfile::tempdir().unwrap();
        let threshold = |agent: &Agent| agent.context.budget().proactive_threshold();
        let mut agent = Agent::new_with_budget(temp.path().to_path_buf(), "sys", 1_048_576)
            .with_llm(Arc::new(ReservingLlm(Some(262_144))));
        assert_eq!(threshold(&agent), 786_432);

        agent.set_llm(Arc::new(ReservingLlm(None)));
        assert_eq!(threshold(&agent), 943_718, "back at 90 %");

        agent.set_llm(Arc::new(ReservingLlm(Some(262_144))));
        agent.set_context_budget(2_000_000);
        assert_eq!(threshold(&agent), 1_737_856);
    }

    /// **The two background builders must not clobber each other.** Both have to
    /// rebuild the manager — its limits are fixed at construction — so each must
    /// restate the other's value. Rebuilding from defaults meant whichever ran
    /// second silently discarded the first one's setting.
    #[test]
    fn background_limits_and_subagent_timeout_survive_each_other() {
        let temp = tempfile::tempdir().unwrap();
        let timeout = std::time::Duration::from_secs(42);

        for agent in [
            Agent::new(temp.path().to_path_buf(), "sys")
                .with_subagent_timeout(Some(timeout))
                .with_background_max_tasks(3),
            Agent::new(temp.path().to_path_buf(), "sys")
                .with_background_max_tasks(3)
                .with_subagent_timeout(Some(timeout)),
        ] {
            assert_eq!(agent.tool_ctx.background.agent_timeout(), Some(timeout));
            assert_eq!(agent.tool_ctx.background.max_running_tasks(), 3);
        }
    }

    /// Zero milliseconds removes the deadline for an overnight run, so `None`
    /// has to survive the rebuild too.
    #[test]
    fn an_unbounded_subagent_timeout_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_subagent_timeout(None)
            .with_background_max_tasks(2);
        assert_eq!(agent.tool_ctx.background.agent_timeout(), None);
        assert_eq!(agent.tool_ctx.background.max_running_tasks(), 2);
    }

    /// Main agent and sub-agent share one `wait_for` description; a
    /// sub-agent's guidance arrives at run time, from its timeout results and
    /// the repeat warning.
    #[test]
    fn a_subagent_shares_the_main_wait_for_description() {
        let temp = tempfile::tempdir().unwrap();
        let parent = Agent::new(temp.path().to_path_buf(), "sys");
        let child = Agent::spawn_child(&parent.tool_ctx, "sub", None);
        let description = |agent: &Agent| {
            agent
                .registry
                .lock()
                .unwrap()
                .get(crate::tools::builtin::wait_for::WAIT_FOR_TOOL_NAME)
                .expect("wait_for is registered")
                .tool_definition()
                .description
                .clone()
        };
        assert_eq!(description(&parent), description(&child));
    }

    /// Drives one `agent` fork to completion and returns the (system, tools
    /// JSON) of every model request in order: the caller's, the fork's first,
    /// the caller's next.
    async fn fork_prefix_requests(kimi: bool) -> Vec<(String, serde_json::Value)> {
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new(vec![
            StepResult {
                message: Message::assistant_tool_calls(vec![
                    ToolCall::new("tc-1", "agent")
                        .with_arguments(r#"{"prompt": "finish the task", "fork": true}"#),
                ]),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::ToolCalls),
                response_id: None,
                stop_reason: None,
            },
            StepResult {
                message: Message::assistant_text("fork done"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            },
            StepResult {
                message: Message::assistant_text("parent done"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            },
        ]);
        let mut parent = Agent::new(temp.path().to_path_buf(), "the caller's instructions")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_llm(Arc::clone(&llm) as Arc<dyn Llm>);
        parent.tool_ctx.background = Arc::new(crate::background::BackgroundManager::with_data_dir(
            temp.path().to_path_buf(),
        ));
        if kimi {
            // What `register_builtin_tools` puts on the main agent on Kimi.
            parent.register_tool(Arc::new(KimiWebSearchTool::new()));
        }
        parent
            .prompt(&*llm, "start", &CancellationToken::new())
            .await
            .unwrap();
        let requests = llm.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            3,
            "caller, fork, caller again: {requests:?}"
        );
        requests
    }

    /// A fork's first request repeats its caller's request prefix so it hits
    /// the prompt cache. On Kimi the session registers the
    /// `$web_search` provider builtin on the main agent only; the fork must
    /// receive it too, or its `tools` table diverges.
    #[tokio::test]
    async fn a_forks_first_request_repeats_its_callers_prefix_on_kimi() {
        let requests = fork_prefix_requests(true).await;
        let has_web_search = requests[0]
            .1
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "$web_search");
        assert!(
            has_web_search,
            "the caller offers $web_search: {:?}",
            requests[0].1
        );
        assert_eq!(requests[1].0, requests[0].0, "same system message");
        assert_eq!(requests[1].1, requests[0].1, "same tools JSON");
    }

    /// GLM takes no provider built-ins (Z.AI rejects the form); the fork's
    /// prefix must match its caller's exactly all the same.
    #[tokio::test]
    async fn a_forks_first_request_repeats_its_callers_prefix_on_glm() {
        let requests = fork_prefix_requests(false).await;
        let has_web_search = requests[0]
            .1
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "$web_search");
        assert!(
            !has_web_search,
            "GLM has no $web_search: {:?}",
            requests[0].1
        );
        assert_eq!(requests[1].0, requests[0].0, "same system message");
        assert_eq!(requests[1].1, requests[0].1, "same tools JSON");
    }

    /// A fork's tool table is its caller's, in the caller's order, deferred
    /// where the caller defers — including the session's MCP tools and the
    /// `$web_search` provider builtin.
    #[test]
    fn a_forks_tool_table_copies_its_callers() {
        use crate::tools::builtin::subagent::fork_agent;
        let tool = |name: &str, description: &str| {
            Arc::new(Loadable(Tool::new(
                name,
                description,
                serde_json::json!({"type": "object"}),
            )))
        };
        let temp = tempfile::tempdir().unwrap();
        let mut parent = Agent::new(temp.path().to_path_buf(), "sys");
        parent.register_tool(Arc::new(KimiWebSearchTool::new()));
        parent.register_dynamic_tool(tool("mcp__code__search", ""));
        parent.register_tool(tool("mcp__small__ping", ""));
        let caller_order: Vec<String> = parent
            .registry
            .lock()
            .unwrap()
            .definitions()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        let fork = fork_agent(&parent.tool_ctx, "sys".into(), None, None);
        let reg = fork.registry.lock().unwrap();
        let fork_order: Vec<String> = reg.definitions().iter().map(|t| t.name.clone()).collect();
        assert_eq!(fork_order, caller_order, "same tools in the caller's order");
        assert!(reg.get("$web_search").is_some());
        assert!(reg.is_dynamic("mcp__code__search"));
        assert!(!reg.is_dynamic("mcp__small__ping"));
        // `select_tools` runs against the fork's own registry, not the caller's.
        let caller_reg = parent.registry.lock().unwrap();
        assert!(
            !std::ptr::eq(
                reg.get("select_tools").unwrap().tool_definition() as *const Tool,
                caller_reg.get("select_tools").unwrap().tool_definition() as *const Tool,
            ),
            "select_tools is re-bound to the fork"
        );
        assert_eq!(
            reg.get("select_tools").unwrap().tool_definition(),
            caller_reg.get("select_tools").unwrap().tool_definition(),
            "the re-bound select_tools advertises the same definition"
        );
    }

    /// A fork shares its caller's `wait_for` instance, but not its tally:
    /// when both wait once in their first turn, neither is warned about
    /// repeated waits.
    #[tokio::test]
    async fn a_fork_and_its_caller_count_their_own_waits() {
        use crate::tools::builtin::subagent::fork_agent;
        let temp = tempfile::tempdir().unwrap();
        let parent = Agent::new(temp.path().to_path_buf(), "sys");
        let fork = fork_agent(&parent.tool_ctx, "sys".into(), None, None);
        let wait_for = |agent: &Agent| agent.registry.lock().unwrap().get("wait_for").unwrap();
        let shared = wait_for(&parent);
        assert!(
            Arc::ptr_eq(&shared, &wait_for(&fork)),
            "the fork keeps its caller's instance"
        );
        for agent in [&parent, &fork] {
            // Each agent's turn loop starts its first turn at 1.
            agent.tool_ctx.turn_id.store(1, Ordering::SeqCst);
            let text = shared
                .execute(serde_json::json!({ "timeout": 1 }), &agent.tool_ctx, "tc")
                .await
                .text_content();
            assert!(!text.contains("[wait_warning]"), "{text}");
        }
    }

    /// A registry holding one skill, `greet`.
    fn greet_skill() -> Arc<SkillRegistry> {
        let mut skills = SkillRegistry::new();
        skills.register(crate::skills::Skill {
            name: "greet".to_string(),
            description: None,
            skill_type: crate::skills::SkillType::User,
            when_to_use: None,
            disable_model_invocation: false,
            content: "Hello".to_string(),
        });
        Arc::new(skills)
    }

    /// The skills saved in the sidecar of the records at `records`.
    fn saved_skills(records: &std::path::Path) -> Vec<String> {
        let path = crate::records::sidecar_path(records, "skills");
        let skills = ActiveSkills::load(&path).unwrap();
        skills.activations.into_iter().map(|a| a.name).collect()
    }

    /// The session registers `skill` before it attaches its records, and a
    /// skill the model uses still reaches the session's skills sidecar.
    #[tokio::test]
    async fn a_skill_the_model_uses_reaches_the_sessions_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("session.jsonl");
        let agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_skill_registry(greet_skill())
            .with_records(Records::new(&records));
        let skill = agent.registry.lock().unwrap().get("skill").unwrap();

        let result = skill
            .execute(
                serde_json::json!({ "name": "greet" }),
                &agent.tool_ctx,
                "tc",
            )
            .await;

        assert!(!result.is_error, "{}", result.text_content());
        assert_eq!(saved_skills(&records), ["greet"]);
        let active = agent.active_skills();
        assert_eq!(active.lock().unwrap().activations.len(), 1);
    }

    /// A request offers only the tools that can work: `web_search` with a
    /// search service, `read_media` with a way to see media, `skill` with a
    /// skill the model may run. `/tools` lists the same, and `select_tools`
    /// says why it cannot load one.
    #[tokio::test]
    async fn a_request_offers_only_the_tools_that_can_work() {
        const NEEDY: [&str; 3] = ["web_search", "read_media", "skill"];
        let temp = tempfile::tempdir().unwrap();
        let offered = |llm: &ScriptedLlm| -> Vec<String> {
            let requests = llm.requests.lock().unwrap();
            let (_, tools) = requests.last().unwrap();
            tools
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_string())
                .collect()
        };

        // A text-only model, no search service, and no skill to run.
        let llm = ScriptedLlm::new(Vec::new());
        let mut bare = Agent::new(temp.path().to_path_buf(), "sys")
            .with_skill_registry(Arc::new(SkillRegistry::new()));
        bare.prompt(llm.as_ref(), "hi", &CancellationToken::new())
            .await
            .unwrap();
        let tools = offered(&llm);
        for name in NEEDY {
            assert!(!tools.iter().any(|tool| tool == name), "{name}: {tools:?}");
            assert!(!bare.list_tools().iter().any(|tool| tool == name), "{name}");
        }
        assert!(tools.iter().any(|tool| tool == "fetch"), "{tools:?}");
        let select = bare.registry.lock().unwrap().get("select_tools").unwrap();
        let answer = select
            .execute(
                serde_json::json!({ "names": ["web_search"] }),
                &bare.tool_ctx,
                "tc",
            )
            .await
            .text_content();
        assert!(
            answer.contains("web_search cannot work in this session"),
            "{answer}"
        );

        // Each with what it needs.
        let llm = ScriptedLlm::new(Vec::new());
        let vision: Arc<dyn Llm> = ScriptedLlm::new(Vec::new());
        let mut equipped = Agent::new(temp.path().to_path_buf(), "sys")
            .with_search_url("https://search.example/?q={query}")
            .with_vision_fallback(Arc::new(crate::vision_fallback::LlmVisionFallback::new(
                vision,
            )))
            .with_skill_registry(greet_skill());
        // Before the first turn has taken them up, too.
        assert!(
            equipped
                .list_tools()
                .iter()
                .any(|tool| tool == "read_media")
        );
        equipped
            .prompt(llm.as_ref(), "hi", &CancellationToken::new())
            .await
            .unwrap();
        let tools = offered(&llm);
        for name in NEEDY {
            assert!(tools.iter().any(|tool| tool == name), "{name}: {tools:?}");
            assert!(
                equipped.list_tools().iter().any(|tool| tool == name),
                "{name}"
            );
        }

        // A deferred tool that cannot work is not loaded either.
        let mut deferred = Agent::new(temp.path().to_path_buf(), "sys");
        deferred.register_dynamic_tool(Arc::new(crate::tools::builtin::SkillTool::default()));
        let disclosure = deferred.tool_ctx.tool_disclosure.clone().unwrap();
        disclosure.lock().unwrap().load(vec!["skill".to_string()]);
        let offered = crate::agent_loop::run_turn::offered_tools(
            &deferred.registry,
            &deferred.tool_ctx,
            &disclosure,
        );
        assert!(!offered.iter().any(|tool| tool.name == "skill"));
    }

    /// `/reload` replaces the skills and the profiles, and with them the
    /// `skill`, `agent` and `agent_swarm` tools, where those stand: the tool
    /// table keeps its order, and so the prompt cache, and `skill` lists the
    /// skills loaded now.
    #[test]
    fn reloaded_skills_and_profiles_keep_the_tool_order() {
        let temp = tempfile::tempdir().unwrap();
        let mut agent =
            Agent::new(temp.path().to_path_buf(), "sys").with_skill_registry(greet_skill());
        // As at startup, more tools come after `skill`.
        agent.register_tool(Arc::new(crate::tools::builtin::KimiWebSearchTool::new()));
        let order = |agent: &Agent| -> Vec<String> {
            let registry = agent.registry.lock().unwrap();
            registry
                .definitions()
                .iter()
                .map(|tool| tool.name.clone())
                .collect()
        };
        let before = order(&agent);

        let mut skills = SkillRegistry::new();
        skills.register(crate::skills::Skill {
            name: "review".to_string(),
            description: None,
            skill_type: crate::skills::SkillType::User,
            when_to_use: None,
            disable_model_invocation: false,
            content: "Review the diff.".to_string(),
        });
        agent.set_skill_registry(Arc::new(skills));
        agent.set_profile_registry(Arc::new(ProfileRegistry::with_builtin_profiles()));

        assert_eq!(order(&agent), before);
        let skill = agent.registry.lock().unwrap().get("skill").unwrap();
        let listed = &skill.tool_definition().description;
        assert!(
            listed.ends_with("Available skills:\n- review: Review the diff."),
            "{listed}"
        );
    }

    /// A todo list with items open is reminded about after five steps without
    /// a change, and again five steps after; a change restarts the count, and
    /// a list with nothing open, or a sub-agent's, is never reminded about.
    #[tokio::test]
    async fn an_open_todo_list_is_reminded_about_after_five_quiet_steps() {
        const MARKER: &str = "The todo list has not been updated in the last";
        let step = |name: &str, args: serde_json::Value| StepResult {
            message: Message::assistant_tool_calls(vec![
                ToolCall::new("tc", name).with_arguments(args.to_string()),
            ]),
            usage: TokenUsage::empty(),
            finish_reason: Some(FinishReason::ToolCalls),
            response_id: None,
            stop_reason: None,
        };
        let todo = |action: &str, extra: serde_json::Value| {
            let mut args = serde_json::json!({ "action": action });
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            step("todo_list", args)
        };
        // Each quiet step distinct, or the repeat breaker would end the turn.
        let quiet = |n: usize| step("glob", serde_json::json!({ "pattern": format!("*.{n}") }));
        // The requests before which a reminder was added, 1-based.
        async fn reminded(agent: &mut Agent, steps: Vec<StepResult>) -> (Vec<usize>, Vec<String>) {
            let llm = ScriptedLlm::new(steps);
            agent
                .prompt(llm.as_ref(), "work", &CancellationToken::new())
                .await
                .unwrap();
            let messages = llm.messages.lock().unwrap();
            let counts: Vec<usize> = messages
                .iter()
                .map(|request| {
                    request
                        .iter()
                        .map(|m| m.text_content().matches(MARKER).count())
                        .sum()
                })
                .collect();
            let at: Vec<usize> = (1..counts.len())
                .filter(|&i| counts[i] > counts[i - 1])
                .map(|i| i + 1)
                .collect();
            let texts = at
                .iter()
                .map(|&n| {
                    messages[n - 1]
                        .iter()
                        .rev()
                        .map(Message::text_content)
                        .find(|t| t.contains(MARKER))
                        .unwrap()
                })
                .collect();
            (at, texts)
        }

        let temp = tempfile::tempdir().unwrap();
        let mut steps = vec![todo("add", serde_json::json!({ "description": "a" }))];
        steps.extend((0..6).map(quiet));
        steps.push(todo("add", serde_json::json!({ "description": "b" })));
        steps.extend((6..11).map(quiet));
        steps.push(todo("done", serde_json::json!({ "id": 0 })));
        steps.push(todo("done", serde_json::json!({ "id": 1 })));
        steps.extend((11..17).map(quiet));
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        let (at, texts) = reminded(&mut agent, steps).await;
        // The second item comes in response 8, the five quiet steps after it
        // in responses 9 to 13.
        assert_eq!(
            at,
            [7, 14],
            "five quiet steps after the first item, five after the second; none once all are done"
        );
        assert!(
            texts[0].contains("in the last 5 steps (0/1 done)"),
            "{}",
            texts[0]
        );
        assert!(texts[1].contains("[ ] 0: a\n[ ] 1: b"), "{}", texts[1]);

        let quiet_turn: Vec<StepResult> =
            std::iter::once(todo("add", serde_json::json!({ "description": "a" })))
                .chain((0..12).map(quiet))
                .collect();
        let again = tempfile::tempdir().unwrap();
        let mut agent = Agent::new(again.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        assert_eq!(
            reminded(&mut agent, quiet_turn.clone()).await.0,
            [7, 12],
            "a reminder repeats five steps after the last"
        );

        let sub = tempfile::tempdir().unwrap();
        let mut subagent = Agent::new(sub.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        subagent.tool_ctx.depth = 1;
        assert!(
            reminded(&mut subagent, quiet_turn).await.0.is_empty(),
            "a sub-agent is never reminded"
        );
    }

    /// A new session's todos reach its sidecar, and resuming the session
    /// brings them back.
    #[tokio::test]
    async fn a_new_sessions_todos_survive_a_resume() {
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("session.jsonl");
        let agent =
            Agent::new(temp.path().to_path_buf(), "sys").with_records(Records::new(&records));
        let todo = agent.registry.lock().unwrap().get("todo_list").unwrap();
        let result = todo
            .execute(
                serde_json::json!({ "action": "add", "description": "write the tests" }),
                &agent.tool_ctx,
                "tc",
            )
            .await;
        assert!(!result.is_error, "{}", result.text_content());
        drop(agent);

        let resumed = Agent::new(temp.path().to_path_buf(), "sys")
            .with_resumed_records(Records::new(&records))
            .await
            .unwrap();

        let todos = resumed.tool_ctx.todo_list.lock().unwrap();
        assert_eq!(todos.list().len(), 1);
        assert_eq!(todos.list()[0].description, "write the tests");
    }

    /// A fork's `skill` call is recorded in the fork's own skills and
    /// sidecar, never in its caller's, and the re-bound tool advertises the
    /// same definition. The fork attaches its records once forked, as the
    /// `agent` tool does.
    #[tokio::test]
    async fn a_forks_skill_use_stays_out_of_its_callers_skills() {
        use crate::tools::builtin::subagent::fork_agent;
        let temp = tempfile::tempdir().unwrap();
        let parent_records = temp.path().join("parent.jsonl");
        let fork_records = temp.path().join("fork.jsonl");
        let parent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_skill_registry(greet_skill())
            .with_records(Records::new(&parent_records));
        let fork = fork_agent(&parent.tool_ctx, "sys".into(), None, None)
            .with_records(Records::new(&fork_records));
        let skill = |agent: &Agent| agent.registry.lock().unwrap().get("skill").unwrap();
        assert_eq!(
            skill(&fork).tool_definition(),
            skill(&parent).tool_definition()
        );

        let result = skill(&fork)
            .execute(serde_json::json!({ "name": "greet" }), &fork.tool_ctx, "tc")
            .await;

        assert!(!result.is_error, "{}", result.text_content());
        let used = |agent: &Agent| -> Vec<String> {
            let skills = agent.active_skills();
            let skills = skills.lock().unwrap();
            skills.activations.iter().map(|a| a.name.clone()).collect()
        };
        assert_eq!(used(&fork), vec!["greet".to_string()]);
        assert!(used(&parent).is_empty(), "the caller used no skill");
        assert_eq!(saved_skills(&fork_records), ["greet"]);
        assert!(saved_skills(&parent_records).is_empty());
    }

    /// The "nothing to load" notice goes only to an agent that can load
    /// tools: the session's agent gets it, `explore` and `plan`, which have
    /// no `select_tools`, never do.
    #[tokio::test]
    async fn only_an_agent_with_select_tools_hears_there_is_nothing_to_load() {
        use crate::tools::builtin::subagent::spawn_with_profile;
        async fn notified(agent: &mut Agent) -> bool {
            let llm = ScriptedLlm::new(vec![]);
            agent
                .prompt(llm.as_ref(), "hello", &CancellationToken::new())
                .await
                .unwrap();
            let messages = llm.messages.lock().unwrap();
            messages[0].iter().any(|m| {
                m.text_content()
                    .contains(crate::tool_disclosure::EMPTY_ANNOUNCEMENT_MARKER)
            })
        }

        let temp = tempfile::tempdir().unwrap();
        let mut main = Agent::new(temp.path().to_path_buf(), "sys");
        assert!(notified(&mut main).await, "the session's agent is told");

        let profiles = ProfileRegistry::with_builtin_profiles();
        let ctx = main.tool_ctx.clone();
        for name in ["explore", "plan"] {
            let mut agent = spawn_with_profile(&ctx, &profiles.resolve(name).unwrap(), None);
            assert!(
                agent.registry.lock().unwrap().get("select_tools").is_none(),
                "{name} has no select_tools"
            );
            assert!(!notified(&mut agent).await, "{name} is never told");
        }
    }

    /// Profile sub-agents get the provider built-ins too, and a profile's
    /// `allowed_tools` cut never removes them: they are the provider's own
    /// server-side tools, not tools the profile gates.
    #[test]
    fn profile_subagents_keep_the_provider_builtins() {
        use crate::tools::builtin::subagent::spawn_with_profile;
        let temp = tempfile::tempdir().unwrap();
        let mut parent = Agent::new(temp.path().to_path_buf(), "sys");
        parent.register_tool(Arc::new(KimiWebSearchTool::new()));
        let profiles = ProfileRegistry::with_builtin_profiles();
        let ctx = parent.tool_ctx.clone();

        // explore's allow list does not name the provider's built-in, yet it
        // survives the cut.
        let explore = spawn_with_profile(&ctx, &profiles.resolve("explore").unwrap(), None);
        assert!(
            explore
                .registry
                .lock()
                .unwrap()
                .get("$web_search")
                .is_some()
        );
    }

    /// The built-in profiles' tool lists reach the sub-agent's registry:
    /// `explore` is read-only, `plan` has no shell, and `coder`
    /// cannot spawn agents.
    #[test]
    fn builtin_profiles_cut_the_subagents_tool_table() {
        use crate::tools::builtin::subagent::spawn_with_profile;
        let temp = tempfile::tempdir().unwrap();
        let parent = Agent::new(temp.path().to_path_buf(), "sys");
        let profiles = ProfileRegistry::with_builtin_profiles();
        let ctx = parent.tool_ctx.clone();

        let explore = spawn_with_profile(&ctx, &profiles.resolve("explore").unwrap(), None);
        let reg = explore.registry.lock().unwrap();
        for kept in [
            "bash",
            "read_file",
            "read_media",
            "glob",
            "grep",
            "web_search",
            "fetch",
        ] {
            assert!(reg.get(kept).is_some(), "explore keeps {kept}");
        }
        for cut in [
            "write_file",
            "edit_file",
            "agent",
            "agent_swarm",
            "cron_create",
        ] {
            assert!(reg.get(cut).is_none(), "explore has no {cut}");
        }
        drop(reg);

        let plan = spawn_with_profile(&ctx, &profiles.resolve("plan").unwrap(), None);
        let reg = plan.registry.lock().unwrap();
        assert!(reg.get("bash").is_none(), "plan has no shell");
        assert!(reg.get("read_file").is_some());
        assert!(reg.get("fetch").is_some());
        drop(reg);

        let coder = spawn_with_profile(&ctx, &profiles.resolve("coder").unwrap(), None);
        let reg = coder.registry.lock().unwrap();
        assert!(reg.get("agent").is_none(), "coder cannot spawn agents");
        assert!(reg.get("agent_swarm").is_none());
        assert!(
            reg.get("ask_user").is_none(),
            "coder cannot question the user"
        );
        assert!(reg.get("ask_user_question").is_none());
        assert!(reg.get("create_goal").is_none());
        assert!(reg.get("bash").is_some(), "coder keeps the working tools");
        assert!(reg.get("write_file").is_some());
        drop(reg);

        // `default` keeps everything minus the main-agent-only tools; `agent`
        // itself stays — the depth bound gates nesting.
        let default = spawn_with_profile(&ctx, &profiles.resolve("default").unwrap(), None);
        let reg = default.registry.lock().unwrap();
        assert!(reg.get("agent").is_some());
        assert!(reg.get("agent_swarm").is_none());
        assert!(reg.get("ask_user").is_none());
        assert!(reg.get("ask_user_question").is_none());
        assert!(reg.get("create_goal").is_none());
        assert!(reg.get("set_goal_budget").is_none());
    }

    /// `explore`'s first message carries the git context inside a repository
    /// and nothing outside one, through the whole `agent` call.
    #[tokio::test]
    async fn explore_starts_with_git_context_only_in_a_repository() {
        use crate::agent_loop::types::ExecutableToolContext;

        struct Capture {
            users: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl Llm for Capture {
            fn model_name(&self) -> &str {
                "capture"
            }
            async fn chat(
                &self,
                messages: Vec<Message>,
                _tools: Vec<Tool>,
                _callbacks: LlmCallbacks,
                _cancel: &CancellationToken,
            ) -> Result<StepResult, lm_bridge::errors::LlmError> {
                self.users.lock().unwrap().extend(
                    messages
                        .iter()
                        .filter(|m| m.role == lm_bridge::message::Role::User)
                        .map(|m| m.text_content()),
                );
                Ok(StepResult {
                    message: Message::assistant_text("surveyed"),
                    usage: TokenUsage::empty(),
                    finish_reason: Some(FinishReason::Stop),
                    response_id: None,
                    stop_reason: None,
                })
            }
        }

        async fn run_explore(dir: std::path::PathBuf) -> Vec<String> {
            use crate::agent_loop::types::ExecutableTool;
            let llm = Arc::new(Capture {
                users: Arc::new(Mutex::new(Vec::new())),
            });
            let users = Arc::clone(&llm.users);
            let background = Arc::new(crate::background::BackgroundManager::with_data_dir(
                dir.join("data"),
            ));
            let ctx = ExecutableToolContext {
                llm: Some(llm),
                background,
                cwd: dir,
                ..Default::default()
            };
            crate::tools::builtin::AgentTool::default()
                .execute(
                    serde_json::json!({
                        "prompt": "survey the code",
                        "profile": "explore",
                        "agent_id": "sub-git",
                    }),
                    &ctx,
                    "tc",
                )
                .await;
            std::mem::take(&mut *users.lock().unwrap())
        }

        // Outside a repository: the task arrives bare.
        let temp = tempfile::tempdir().unwrap();
        let users = run_explore(temp.path().to_path_buf()).await;
        let task = users
            .iter()
            .find(|text| text.contains("survey the code"))
            .expect("the task reached the model");
        assert!(!task.contains("<git-context>"), "{task}");
        // Inside one: branch, dirty files and recent commits lead the task.
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
        };
        if git(&["--version"]).is_err() {
            eprintln!("git unavailable; skipping the repository half");
            return;
        }
        for args in [
            &["init", "-b", "main"][..],
            &["config", "user.email", "t@example.com"][..],
            &["config", "user.name", "T"][..],
        ] {
            assert!(git(args).unwrap().status.success(), "git {args:?}");
        }
        std::fs::write(repo.path().join("lib.rs"), "fn main() {}").unwrap();
        git(&["add", "."]).unwrap();
        git(&["commit", "-m", "bootstrap"]).unwrap();

        let users = run_explore(repo.path().to_path_buf()).await;
        // The projector folds the date reminder and the task into one user
        // message; the git context leads the task itself.
        let task = users
            .iter()
            .find(|text| text.contains("survey the code"))
            .expect("the task reached the model");
        assert!(task.contains("<git-context>"), "{task}");
        assert!(task.contains("Branch: main"), "{task}");
        assert!(task.contains("bootstrap"), "{task}");
        assert!(
            task.find("<git-context>").unwrap() < task.find("survey the code").unwrap(),
            "the context orients the agent before the task: {task}"
        );
    }

    /// An LLM that never answers: it waits for cancellation, the way a
    /// provider stream does when the user cuts a turn short.
    struct HangingLlm;

    #[async_trait]
    impl Llm for HangingLlm {
        fn model_name(&self) -> &str {
            "hanging"
        }
        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            cancel: &CancellationToken,
        ) -> Result<StepResult, lm_bridge::errors::LlmError> {
            cancel.cancelled().await;
            Err(lm_bridge::errors::LlmError::Cancelled)
        }
    }

    /// Cancel `agent`'s prompt as the user would: set the shared flag, then
    /// cancel the token. Without `user_initiated` the abort is programmatic.
    async fn cancel_like_user(agent: &Agent, cancel: &CancellationToken, user_initiated: bool) {
        let flag = agent.user_cancel_flag();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if user_initiated {
                flag.store(true, Ordering::SeqCst);
            }
            cancel.cancel();
        });
    }

    fn reminder_count(agent: &Agent) -> usize {
        agent
            .context
            .raw_messages()
            .iter()
            .filter(|m| crate::prompt::is_interruption_reminder(&m.text_content()))
            .count()
    }

    /// A turn the user cancelled (Ctrl+C / Esc — the front-end sets the flag
    /// before cancelling the token) is followed by the interruption reminder.
    /// Two cancels in a row leave one reminder per turn, never two for one.
    #[tokio::test]
    async fn a_user_cancelled_turn_gets_one_interruption_reminder() {
        let temp = tempfile::tempdir().unwrap();
        let llm = HangingLlm;
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys");

        let cancel = CancellationToken::new();
        cancel_like_user(&agent, &cancel, true).await;
        let err = agent.prompt(&llm, "first", &cancel).await.unwrap_err();
        assert!(err.is_cancellation(), "got {err:?}");
        assert_eq!(
            reminder_count(&agent),
            1,
            "one cancelled turn, one reminder"
        );
        assert!(
            agent
                .context
                .raw_messages()
                .last()
                .is_some_and(|m| { crate::prompt::is_interruption_reminder(&m.text_content()) }),
            "the reminder closes the turn"
        );

        // A second cancel of the same cut-short turn would not double it.
        agent.context.add_interruption_reminder();
        assert_eq!(
            reminder_count(&agent),
            1,
            "the last message already is the reminder"
        );

        // The next turn is cancelled too: its own reminder, separated by the
        // new user message — still exactly one per cancelled turn.
        let cancel = CancellationToken::new();
        cancel_like_user(&agent, &cancel, true).await;
        let err = agent.prompt(&llm, "second", &cancel).await.unwrap_err();
        assert!(err.is_cancellation(), "got {err:?}");
        assert_eq!(
            reminder_count(&agent),
            2,
            "each cancelled turn earns its own reminder"
        );
    }

    /// SIGTERM, limits and programmatic aborts cancel the token without the
    /// flag: no reminder follows them.
    #[tokio::test]
    async fn a_programmatic_abort_gets_no_interruption_reminder() {
        let temp = tempfile::tempdir().unwrap();
        let llm = HangingLlm;
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys");

        let cancel = CancellationToken::new();
        cancel_like_user(&agent, &cancel, false).await;
        let err = agent.prompt(&llm, "work", &cancel).await.unwrap_err();
        assert!(err.is_cancellation(), "got {err:?}");
        assert_eq!(
            reminder_count(&agent),
            0,
            "a cancel without the flag is not a user cancel"
        );
    }

    /// A headless max-turns stop ends the turn, not the run: no reminder.
    #[tokio::test]
    async fn a_max_turns_stop_gets_no_interruption_reminder() {
        let temp = tempfile::tempdir().unwrap();
        let llm = MockLlm::new(vec![StepResult {
            message: Message::assistant_text("out of steps"),
            usage: TokenUsage::empty(),
            finish_reason: Some(FinishReason::Stop),
            response_id: None,
            stop_reason: Some(crate::agent_loop::types::StopReason::MaxTurns),
        }]);
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys");

        let result = agent.prompt(&llm, "work", &CancellationToken::new()).await;
        assert!(result.is_ok(), "a stopped turn is not an error: {result:?}");
        assert_eq!(
            reminder_count(&agent),
            0,
            "running out of steps is not an interruption"
        );
    }

    /// `/undo` removes the interrupted turn together with its reminder: the
    /// reminder is an injection, not the start of a turn.
    #[tokio::test]
    async fn undo_removes_the_interruption_reminder_with_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let llm = HangingLlm;
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys");

        let cancel = CancellationToken::new();
        cancel_like_user(&agent, &cancel, true).await;
        let err = agent
            .prompt(&llm, "do the thing", &cancel)
            .await
            .unwrap_err();
        assert!(err.is_cancellation(), "got {err:?}");
        assert_eq!(reminder_count(&agent), 1);

        let removed = agent.undo_last_turn();
        assert!(removed > 0, "the interrupted turn is undone");
        assert_eq!(reminder_count(&agent), 0, "the reminder went with it");
        assert!(
            agent
                .context
                .raw_messages()
                .iter()
                .all(|m| !m.text_content().contains("do the thing")),
            "the interrupted prompt went with it"
        );
    }

    /// The reminder is written with the turn's records, so a resumed session
    /// still knows the previous turn was cut short.
    #[tokio::test]
    async fn the_interruption_reminder_survives_a_resume() {
        let temp = tempfile::tempdir().unwrap();
        let records = crate::records::Records::new(temp.path().join("session.jsonl"));
        {
            let llm = HangingLlm;
            let mut agent = Agent::new(temp.path().to_path_buf(), "sys").with_records(records);
            let cancel = CancellationToken::new();
            cancel_like_user(&agent, &cancel, true).await;
            let err = agent
                .prompt(&llm, "do the thing", &cancel)
                .await
                .unwrap_err();
            assert!(err.is_cancellation(), "got {err:?}");
            assert_eq!(reminder_count(&agent), 1);
        }

        let resumed = Agent::new(temp.path().to_path_buf(), "sys")
            .with_resumed_records(crate::records::Records::new(
                temp.path().join("session.jsonl"),
            ))
            .await
            .expect("the records replay");
        assert_eq!(
            reminder_count(&resumed),
            1,
            "the resumed session keeps the reminder with the turn"
        );
    }

    /// Hooks for a test, rooted in the temp dir so commands run there.
    fn test_hooks(
        dir: &std::path::Path,
        rules: Vec<crate::user_hooks::UserHookRule>,
    ) -> Arc<crate::user_hooks::UserHooks> {
        Arc::new(
            crate::user_hooks::UserHooks::new(
                rules,
                "session_test",
                "Hooks test",
                "rusta_cli",
                dir.to_path_buf(),
            )
            .unwrap(),
        )
    }

    /// `dangerous_command_guard = false` holds when the permission mode is
    /// set after it, as the CLI sets them: under `--yes`, a `git push` is
    /// asked about only while the guard is on.
    #[test]
    fn the_guard_switch_survives_a_later_permission_mode() {
        let temp = tempfile::tempdir().unwrap();
        let push = serde_json::json!({ "command": "git push origin HEAD:refs/heads/main" });
        let check = |agent: &Agent| {
            agent
                .permissions
                .check_policy("bash", &push, &agent.tool_ctx)
        };
        let off = Agent::new(temp.path().to_path_buf(), "sys")
            .with_dangerous_command_guard(false)
            .with_permission_mode(PermissionMode::BypassPermissions);
        assert_eq!(check(&off), crate::permissions::PolicyCheck::NoMatch);
        let on = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions);
        assert!(
            matches!(check(&on), crate::permissions::PolicyCheck::Prompt { .. }),
            "{:?}",
            check(&on)
        );
    }

    /// A title the session gets reaches the hooks' payloads.
    #[test]
    fn the_session_title_reaches_the_hooks() {
        let temp = tempfile::tempdir().unwrap();
        let hooks = test_hooks(temp.path(), Vec::new());
        let mut agent =
            Agent::new(temp.path().to_path_buf(), "sys").with_user_hooks(Some(Arc::clone(&hooks)));
        agent.set_session_title("Fix the login page");
        assert_eq!(agent.session_title(), "Fix the login page");
        assert_eq!(hooks.session_title(), "Fix the login page");
    }

    fn hook_rule(
        event: crate::user_hooks::UserHookEvent,
        command: &str,
    ) -> crate::user_hooks::UserHookRule {
        crate::user_hooks::UserHookRule {
            event,
            matcher: None,
            command: command.to_string(),
            timeout: Some(5),
        }
    }

    /// UserPromptSubmit stdout is folded into the prompt message, not added
    /// as a separate one.
    #[tokio::test]
    async fn user_prompt_submit_output_is_folded_into_the_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new(vec![]);
        let hooks = test_hooks(
            temp.path(),
            vec![hook_rule(
                crate::user_hooks::UserHookEvent::UserPromptSubmit,
                "echo 'current branch: main'",
            )],
        );
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_user_hooks(Some(hooks));

        agent
            .prompt(llm.as_ref(), "what changed?", &CancellationToken::new())
            .await
            .unwrap();

        let user_text: String = agent
            .history()
            .iter()
            .filter(|m| m.role == lm_bridge::message::Role::User)
            .map(Message::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            user_text.contains("what changed?\n\ncurrent branch: main"),
            "folded into one message: {user_text}"
        );
    }

    /// UserPromptSubmit fires for the user's own prompts only. TurnStarted
    /// fires for every turn and reports where its prompt came from; its
    /// matcher tests the origin kind.
    #[tokio::test]
    async fn turn_hooks_report_the_prompt_origin() {
        use crate::user_hooks::UserHookEvent::{TurnStarted, UserPromptSubmit};
        let temp = tempfile::tempdir().unwrap();
        let log = |name: &str| temp.path().join(name);
        let append = |path: &std::path::Path| format!("cat >> '{}'", path.display());
        let mut cron_only = hook_rule(TurnStarted, &append(&log("cron-turns")));
        cron_only.matcher = Some("^cron_job$".to_string());
        let hooks = test_hooks(
            temp.path(),
            vec![
                hook_rule(UserPromptSubmit, &append(&log("submitted"))),
                hook_rule(TurnStarted, &append(&log("started"))),
                cron_only,
            ],
        );
        let llm = ScriptedLlm::new(vec![]);
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_user_hooks(Some(hooks));

        for (origin, prompt) in [
            (PromptOrigin::User, "typed by the user"),
            (PromptOrigin::Cron, "<cron-fire jobId=\"0123abcd\">"),
            (PromptOrigin::Continuation, "continue the work"),
        ] {
            agent
                .prompt_from(
                    origin,
                    llm.as_ref(),
                    prompt,
                    LlmCallbacks::default(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
        }

        let payloads = |name: &str| -> Vec<serde_json::Value> {
            let text = std::fs::read_to_string(log(name)).unwrap_or_default();
            serde_json::Deserializer::from_str(&text)
                .into_iter()
                .map(Result::unwrap)
                .collect()
        };
        let submitted = payloads("submitted");
        assert_eq!(submitted.len(), 1, "{submitted:?}");
        assert_eq!(submitted[0]["prompt"], "typed by the user");
        let origins: Vec<(String, Option<String>)> = payloads("started")
            .iter()
            .map(|p| {
                (
                    p["origin_kind"].as_str().unwrap().to_string(),
                    p["origin_name"].as_str().map(str::to_string),
                )
            })
            .collect();
        assert_eq!(
            origins,
            vec![
                ("user".to_string(), None),
                ("cron_job".to_string(), None),
                (
                    "system_trigger".to_string(),
                    Some("headless_continuation".to_string())
                ),
            ]
        );
        let cron_turns = payloads("cron-turns");
        assert_eq!(cron_turns.len(), 1, "{cron_turns:?}");
        assert_eq!(cron_turns[0]["prompt"], "<cron-fire jobId=\"0123abcd\">");
    }

    /// A blocking UserPromptSubmit hook skips the model call entirely.
    #[tokio::test]
    async fn user_prompt_submit_block_skips_the_model_call() {
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new(vec![]);
        let hooks = test_hooks(
            temp.path(),
            vec![hook_rule(
                crate::user_hooks::UserHookEvent::UserPromptSubmit,
                "echo not allowed 1>&2; exit 2",
            )],
        );
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_user_hooks(Some(hooks));

        let err = agent
            .prompt(llm.as_ref(), "hello?", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, AgentError::HookBlocked(ref reason) if reason == "not allowed"),
            "got {err:?}"
        );
        assert!(
            llm.requests.lock().unwrap().is_empty(),
            "the model must never be asked"
        );
    }

    /// A blocking Stop hook appends its reason and sends the turn back to the
    /// model instead of ending it.
    #[tokio::test]
    async fn stop_hook_block_sends_the_turn_back_to_the_model() {
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new(vec![]);
        // Block once (no marker file), then allow: the second Stop ends the turn.
        let marker = temp.path().join("stop-marker");
        let command = format!(
            "[ -f '{}' ] || {{ touch '{}'; echo keep-going 1>&2; exit 2; }}",
            marker.display(),
            marker.display()
        );
        let hooks = test_hooks(
            temp.path(),
            vec![hook_rule(crate::user_hooks::UserHookEvent::Stop, &command)],
        );
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_user_hooks(Some(hooks));

        agent
            .prompt(llm.as_ref(), "finish", &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            llm.requests.lock().unwrap().len(),
            2,
            "the blocked Stop turned one turn into two model calls"
        );
        let history: String = agent
            .history()
            .iter()
            .map(Message::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            history.contains("keep-going"),
            "the block reason reached the model: {history}"
        );
    }

    /// Runs one prompt with a single Stop hook and returns how many model
    /// calls the turn made.
    async fn model_calls_with_stop_hook(command: &str) -> usize {
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new(vec![]);
        let hooks = test_hooks(
            temp.path(),
            vec![hook_rule(crate::user_hooks::UserHookEvent::Stop, command)],
        );
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_user_hooks(Some(hooks));
        agent
            .prompt(llm.as_ref(), "finish", &CancellationToken::new())
            .await
            .unwrap();
        llm.requests.lock().unwrap().len()
    }

    /// An allowing Stop hook ends the turn whatever it prints: its stdout is
    /// not context, and only a block acts. Otherwise a notification hook that
    /// echoes would send every turn back to the model until the step cap.
    #[tokio::test]
    async fn an_allowing_stop_hook_ends_the_turn_whatever_it_prints() {
        assert_eq!(model_calls_with_stop_hook("echo notified").await, 1);
    }

    /// A Stop hook continues a turn at most once: a hook that blocks every time
    /// costs one extra model call, not the whole step budget.
    #[tokio::test]
    async fn a_stop_hook_continues_a_turn_at_most_once() {
        assert_eq!(
            model_calls_with_stop_hook("echo again 1>&2; exit 2").await,
            2
        );
    }

    /// The Stop payload says whether the turn was already continued by a
    /// Stop hook; when the hook runs it never was.
    #[tokio::test]
    async fn the_stop_payload_carries_stop_hook_active() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload.json");
        let calls = model_calls_with_stop_hook(&format!("cat > '{}'", payload.display())).await;
        assert_eq!(calls, 1);
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&payload).unwrap()).unwrap();
        assert_eq!(payload["hook_event_name"], "Stop");
        assert_eq!(payload["stop_hook_active"], false);
    }

    /// A step that ends while steered input waits does not end the turn, so
    /// Stop does not run for it; it runs once, when the turn really ends.
    #[tokio::test]
    async fn steered_steps_do_not_run_the_stop_hook() {
        let temp = tempfile::tempdir().unwrap();
        let fired = temp.path().join("fired.log");
        let hooks = test_hooks(
            temp.path(),
            vec![hook_rule(
                crate::user_hooks::UserHookEvent::Stop,
                &format!("echo stop >> '{}'", fired.display()),
            )],
        );
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_user_hooks(Some(hooks));
        let llm =
            ScriptedLlm::new(vec![]).steering_on_first_call(agent.steer_channel(), "also this");

        agent
            .prompt(llm.as_ref(), "finish", &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            llm.requests.lock().unwrap().len(),
            2,
            "the steered input kept the turn going"
        );
        assert_eq!(
            std::fs::read_to_string(&fired).unwrap().lines().count(),
            1,
            "Stop ran once, at the turn's real end"
        );
    }

    /// Interrupt fires when the user cancels the turn, StopFailure when it
    /// fails on its own.
    #[tokio::test]
    async fn interrupt_and_stop_failure_hooks_fire() {
        let temp = tempfile::tempdir().unwrap();
        for (event, command) in [
            (
                crate::user_hooks::UserHookEvent::Interrupt,
                "echo interrupt >> \"$RUSTA_HOOK_EVENTS\"",
            ),
            (
                crate::user_hooks::UserHookEvent::StopFailure,
                "echo stop-failure >> \"$RUSTA_HOOK_EVENTS\"",
            ),
        ] {
            let file = temp.path().join("events.log");
            let command = command.replace("$RUSTA_HOOK_EVENTS", &file.to_string_lossy());
            let hooks = test_hooks(temp.path(), vec![hook_rule(event, &command)]);
            let llm = HangingLlm;
            let mut agent =
                Agent::new(temp.path().to_path_buf(), "sys").with_user_hooks(Some(hooks));
            let cancel = CancellationToken::new();
            // A user-shaped cancel: the flag the front-end sets for Ctrl+C/Esc,
            // just after the prompt starts (prompt_with_callbacks clears any
            // flag set earlier).
            cancel_like_user(&agent, &cancel, true).await;
            let err = agent.prompt(&llm, "do", &cancel).await.unwrap_err();
            assert!(err.is_cancellation());
        }
        let logged = std::fs::read_to_string(temp.path().join("events.log")).unwrap_or_default();
        assert!(
            logged.contains("interrupt"),
            "the user cancel fired Interrupt: {logged}"
        );
        assert!(
            !logged.contains("stop-failure"),
            "a cancellation is not a failure: {logged}"
        );

        // A genuine failure (the LLM errored) fires StopFailure instead.
        struct FailingLlm;
        #[async_trait]
        impl Llm for FailingLlm {
            fn model_name(&self) -> &str {
                "failing"
            }
            async fn chat(
                &self,
                _messages: Vec<Message>,
                _tools: Vec<Tool>,
                _callbacks: LlmCallbacks,
                _cancel: &CancellationToken,
            ) -> Result<StepResult, lm_bridge::errors::LlmError> {
                Err(lm_bridge::errors::LlmError::ApiConnection(
                    "down".to_string(),
                ))
            }
        }
        let file = temp.path().join("events.log");
        let _ = std::fs::remove_file(&file);
        let hooks = test_hooks(
            temp.path(),
            vec![hook_rule(
                crate::user_hooks::UserHookEvent::StopFailure,
                &format!("echo stop-failure >> '{}'", file.display()),
            )],
        );
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys").with_user_hooks(Some(hooks));
        let err = agent
            .prompt(&FailingLlm, "do", &CancellationToken::new())
            .await;
        assert!(err.is_err());
        let logged = std::fs::read_to_string(&file).unwrap_or_default();
        assert!(logged.contains("stop-failure"), "got {logged}");
    }

    fn plan_step(calls: Vec<ToolCall>) -> StepResult {
        StepResult {
            message: Message::assistant_tool_calls(calls),
            usage: TokenUsage::empty(),
            finish_reason: Some(FinishReason::ToolCalls),
            response_id: None,
            stop_reason: None,
        }
    }

    fn plan_text(text: &str) -> StepResult {
        StepResult {
            message: Message::assistant_text(text),
            usage: TokenUsage::empty(),
            finish_reason: Some(FinishReason::Stop),
            response_id: None,
            stop_reason: None,
        }
    }

    fn present_plan(id: &str) -> ToolCall {
        ToolCall::new(id, "exit_plan_mode")
            .with_arguments(r###"{"plan": "## Context\nFix the parser."}"###)
    }

    /// Answers each review from a script, and counts what it was asked.
    struct Reviewer(std::sync::Mutex<Vec<crate::permissions::ApprovalResponse>>);

    #[async_trait]
    impl ApprovalHook for Reviewer {
        async fn request_approval(
            &self,
            _request: crate::permissions::ApprovalRequest,
        ) -> crate::permissions::ApprovalResponse {
            self.0.lock().unwrap().remove(0)
        }
    }

    /// The tool results of `agent`'s history, in order.
    fn tool_results(agent: &Agent) -> Vec<String> {
        agent
            .history()
            .iter()
            .filter(|m| m.role == lm_bridge::message::Role::Tool)
            .map(Message::text_content)
            .collect()
    }

    /// The model hears of plan mode as the user switches it: in full when
    /// it turns on, briefly while it stays on, and once when it is off.
    #[tokio::test]
    async fn plan_mode_reminders_follow_the_users_switch() {
        use crate::plan_mode::{EXIT_REMINDER, FULL_REMINDER, SPARSE_REMINDER};
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new((0..4).map(|_| plan_text("ok")).collect());
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys").with_plan_mode(true);
        let cancel = CancellationToken::new();
        let count = |llm: &ScriptedLlm, turn: usize, reminder: &str| {
            llm.messages.lock().unwrap()[turn]
                .iter()
                .map(|m| m.text_content().matches(reminder).count())
                .sum::<usize>()
        };

        agent
            .prompt(llm.as_ref(), "plan a fix", &cancel)
            .await
            .unwrap();
        assert_eq!(count(&llm, 0, FULL_REMINDER), 1);
        agent
            .prompt(llm.as_ref(), "and then?", &cancel)
            .await
            .unwrap();
        assert_eq!(count(&llm, 1, FULL_REMINDER), 1, "said in full once");
        assert_eq!(count(&llm, 1, SPARSE_REMINDER), 1);
        assert!(!agent.toggle_plan_mode());
        agent
            .prompt(llm.as_ref(), "just do it", &cancel)
            .await
            .unwrap();
        assert_eq!(count(&llm, 2, EXIT_REMINDER), 1);
        agent.prompt(llm.as_ref(), "thanks", &cancel).await.unwrap();
        assert_eq!(count(&llm, 3, EXIT_REMINDER), 1, "said once");
        assert_eq!(count(&llm, 3, SPARSE_REMINDER), 1, "no reminder since");
    }

    /// A sub-agent is never in plan mode, so the plan reminders a fork's
    /// history carries from its parent never end a plan mode it was not in.
    #[tokio::test]
    async fn a_subagent_gets_no_plan_reminders() {
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new(vec![plan_text("ok")]);
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys");
        agent.tool_ctx.depth = 1;
        agent
            .context
            .add_message(crate::plan_mode::reminder_message(
                crate::plan_mode::FULL_REMINDER,
            ));
        agent
            .prompt(llm.as_ref(), "a task", &CancellationToken::new())
            .await
            .unwrap();
        let sent = llm.messages.lock().unwrap()[0]
            .iter()
            .map(Message::text_content)
            .collect::<String>();
        assert!(!sent.contains(crate::plan_mode::EXIT_REMINDER));
    }

    /// Every plan waits for the user's review, in bypass mode too: a
    /// rejected plan leaves plan mode on, an approved one ends it, and an
    /// earlier "yes to all" does not approve the next plan.
    #[tokio::test]
    async fn every_plan_waits_for_the_users_review() {
        use crate::permissions::ApprovalResponse::{AllowOnce, AllowSession, DenyOnce};
        let temp = tempfile::tempdir().unwrap();
        let llm = ScriptedLlm::new(vec![
            plan_step(vec![present_plan("p-1")]),
            plan_step(vec![present_plan("p-2")]),
            plan_text("carrying it out"),
            plan_step(vec![present_plan("p-3")]),
            plan_text("carrying it out"),
        ]);
        let reviewer = Arc::new(Reviewer(std::sync::Mutex::new(vec![
            DenyOnce,
            AllowSession,
            AllowOnce,
        ])));
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_approval_hook(reviewer.clone())
            .with_plan_mode(true);
        let cancel = CancellationToken::new();

        agent
            .prompt(llm.as_ref(), "plan a fix", &cancel)
            .await
            .unwrap();
        let results = tool_results(&agent);
        assert!(
            results[0].contains("The user rejected this plan"),
            "{results:?}"
        );
        assert!(
            results[1].contains("The user approved the plan"),
            "{results:?}"
        );
        assert!(!agent.plan_mode());

        assert!(agent.toggle_plan_mode());
        agent
            .prompt(llm.as_ref(), "plan the next", &cancel)
            .await
            .unwrap();
        assert!(
            reviewer.0.lock().unwrap().is_empty(),
            "the third plan was reviewed too"
        );
        assert!(!agent.plan_mode());
    }

    /// In a run with nobody to ask, a plan cannot be approved and an edit
    /// cannot be made in plan mode, and the model is told that nobody could
    /// approve them rather than that the user refused.
    #[tokio::test]
    async fn a_run_with_nobody_to_ask_keeps_plan_mode_on() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("notes.txt");
        let llm = ScriptedLlm::new(vec![
            plan_step(vec![
                present_plan("p-1"),
                ToolCall::new("w-1", "write_file").with_arguments(
                    serde_json::json!({ "path": target, "content": "x" }).to_string(),
                ),
            ]),
            plan_text("here is the plan"),
        ]);
        let mut agent = Agent::new(temp.path().to_path_buf(), "sys")
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_plan_mode(true);

        agent
            .prompt(llm.as_ref(), "plan a fix", &CancellationToken::new())
            .await
            .unwrap();
        let results = tool_results(&agent);
        assert!(
            results[0].contains("Nobody can review this plan in this run"),
            "{results:?}"
        );
        assert!(
            results[1].contains("write_file needs approval, but nobody can approve it"),
            "{results:?}"
        );
        assert!(agent.plan_mode());
        assert!(!target.exists());
    }
}
