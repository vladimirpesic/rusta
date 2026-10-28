use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use lm_bridge::{TokenUsage, message::ContentPart, tool::Tool};

use super::llm::Llm;
use crate::goal::GoalManager;
use crate::todo::TodoList;
use crate::tool_disclosure::ToolDisclosureState;
use crate::vision_fallback::VisionFallback;
use crate::web_reader_client::WebReaderClient;
use crate::web_search_client::WebSearchClient;
use crate::{background::BackgroundManager, cron::CronManager};

/// Lifecycle status of a single tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// An event emitted during the lifecycle of a tool call.
#[derive(Debug, Clone)]
pub enum ToolEvent {
    Call {
        id: String,
        name: String,
        args: String,
        status: ToolStatus,
    },
    Progress {
        id: String,
        stdout_chunk: Option<String>,
        stderr_chunk: Option<String>,
        status: ToolStatus,
    },
    Result {
        id: String,
        result: ExecutableToolResult,
        status: ToolStatus,
    },
}

/// Sender callback used to surface tool lifecycle events to the UI.
pub type ToolEventSender = Arc<dyn Fn(ToolEvent) + Send + Sync>;

/// Where the tool events of sub-agents go, each with the `agent_id` of the
/// sub-agent whose call it is: set on the session's agent by a front-end that
/// shows sub-agents' work (`--stream-json`), and passed down to theirs.
pub type SubagentToolEventSender = Arc<dyn Fn(&str, ToolEvent) + Send + Sync>;

pub type BeforeStepHook = Arc<dyn Fn(&mut Vec<lm_bridge::tool::Tool>) + Send + Sync>;
pub type AfterStepHook = Arc<dyn Fn(&StepResult) + Send + Sync>;
pub type ShouldContinueHook = Arc<dyn Fn(&StepResult) -> bool + Send + Sync>;

/// Optional hooks that observe or influence the agent turn loop.
#[derive(Clone, Default)]
pub struct LoopHooks {
    pub before_step: Option<BeforeStepHook>,
    pub after_step: Option<AfterStepHook>,
    pub should_continue_after_stop: Option<ShouldContinueHook>,
}

/// A single option inside a structured question.
#[derive(Debug, Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

/// A structured question that can be presented as multiple-choice.
#[derive(Debug, Clone)]
pub struct Question {
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

/// Answers to one or more structured questions.
#[derive(Debug, Clone)]
pub struct QuestionAnswers {
    /// Mapping from question text to the selected option labels.
    pub answers: std::collections::HashMap<String, Vec<String>>,
}

/// Async interaction channel between a tool and the user.
#[async_trait]
pub trait UserInteraction: Send + Sync {
    /// Ask the user a question and return their typed answer.
    async fn ask(&self, question: &str) -> Option<String>;

    /// Ask the user one or more structured multiple-choice questions.
    ///
    /// The default implementation falls back to plain-text prompting and parses
    /// comma-separated option labels.
    async fn ask_question(&self, questions: &[Question]) -> Option<QuestionAnswers> {
        let mut answers = std::collections::HashMap::new();
        for q in questions {
            let mut prompt = String::new();
            if let Some(header) = &q.header {
                prompt.push_str(&format!("[{header}] "));
            }
            prompt.push_str(&q.question);
            prompt.push('\n');
            for (i, opt) in q.options.iter().enumerate() {
                prompt.push_str(&format!("{}. {}", i + 1, opt.label));
                if let Some(desc) = &opt.description {
                    prompt.push_str(&format!(" — {desc}"));
                }
                prompt.push('\n');
            }
            if q.multi_select {
                prompt.push_str("Select one or more (comma-separated):");
            } else {
                prompt.push_str("Select one:");
            }
            let answer = self.ask(&prompt).await?;
            let selected: Vec<String> = answer
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    // Allow either the label or the 1-based index.
                    if let Ok(idx) = s.parse::<usize>() {
                        q.options
                            .get(idx.saturating_sub(1))
                            .map(|o| o.label.clone())
                            .unwrap_or_else(|| s.to_string())
                    } else {
                        s.to_string()
                    }
                })
                .collect();
            if !selected.is_empty() {
                answers.insert(q.question.clone(), selected);
            }
        }
        if answers.is_empty() {
            None
        } else {
            Some(QuestionAnswers { answers })
        }
    }
}

impl ToolEvent {
    /// Return the tool-call id associated with this event.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Call { id, .. } | Self::Progress { id, .. } | Self::Result { id, .. } => id,
        }
    }
}

/// A resource access pattern for a tool execution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolAccess {
    Read(PathBuf),
    Write(PathBuf),
    ReadWriteTree(PathBuf),
}

/// Conflict-aware access set for a single tool execution.
#[derive(Debug, Clone, Default)]
pub struct ToolAccesses {
    accesses: Vec<ToolAccess>,
}

impl ToolAccesses {
    pub fn new(accesses: Vec<ToolAccess>) -> Self {
        Self { accesses }
    }

    pub fn read_file(path: impl Into<PathBuf>) -> Self {
        Self {
            accesses: vec![ToolAccess::Read(path.into())],
        }
    }

    pub fn write_file(path: impl Into<PathBuf>) -> Self {
        Self {
            accesses: vec![ToolAccess::Write(path.into())],
        }
    }

    pub fn read_write_tree(path: impl Into<PathBuf>) -> Self {
        Self {
            accesses: vec![ToolAccess::ReadWriteTree(path.into())],
        }
    }

    pub fn none() -> Self {
        Self { accesses: vec![] }
    }

    /// An access pattern that conflicts with every other tool, used to force
    /// serialization (e.g. for `select_tools`).
    pub fn all() -> Self {
        Self {
            accesses: vec![ToolAccess::ReadWriteTree(std::path::PathBuf::from("/"))],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.accesses.is_empty()
    }

    /// Returns true if two access sets conflict.
    pub fn conflicts(left: &Self, right: &Self) -> bool {
        for a in &left.accesses {
            for b in &right.accesses {
                if access_conflict(a, b) || access_conflict(b, a) {
                    return true;
                }
            }
        }
        false
    }

    pub fn iter(&self) -> impl Iterator<Item = &ToolAccess> {
        self.accesses.iter()
    }
}

fn access_conflict(a: &ToolAccess, b: &ToolAccess) -> bool {
    let (p1, p2) = match (a, b) {
        (ToolAccess::Read(_), ToolAccess::Read(_)) => return false,
        (ToolAccess::Read(p1), ToolAccess::Write(p2))
        | (ToolAccess::Read(p1), ToolAccess::ReadWriteTree(p2))
        | (ToolAccess::Write(p1), ToolAccess::Read(p2))
        | (ToolAccess::Write(p1), ToolAccess::Write(p2))
        | (ToolAccess::Write(p1), ToolAccess::ReadWriteTree(p2))
        | (ToolAccess::ReadWriteTree(p1), ToolAccess::Read(p2))
        | (ToolAccess::ReadWriteTree(p1), ToolAccess::Write(p2))
        | (ToolAccess::ReadWriteTree(p1), ToolAccess::ReadWriteTree(p2)) => (p1, p2),
    };
    paths_overlap(p1, p2)
}

fn paths_overlap(a: &std::path::Path, b: &std::path::Path) -> bool {
    let a = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let b = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    a.starts_with(&b) || b.starts_with(&a)
}

/// Result of executing a tool.
#[derive(Debug, Clone)]
pub struct ExecutableToolResult {
    pub output: Vec<ContentPart>,
    pub is_error: bool,
    pub stop_turn: bool,
}

impl ExecutableToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            output: vec![ContentPart::text(text)],
            is_error: false,
            stop_turn: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            output: vec![ContentPart::text(text)],
            is_error: true,
            stop_turn: false,
        }
    }

    pub fn text_content(&self) -> String {
        self.output
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Context passed to a tool execution.
#[derive(Clone)]
pub struct ExecutableToolContext {
    pub cwd: PathBuf,
    pub allowed_paths: HashSet<PathBuf>,
    pub todo_list: Arc<Mutex<TodoList>>,
    pub tool_event_sender: Option<ToolEventSender>,
    /// Identifies the turn currently running: the turn loop bumps the counter
    /// at the start of every turn. Tools with per-turn state (like
    /// `wait_for`'s repeat-call tally) key off it; a fresh turn starts a fresh
    /// tally. Shared by clone, so every step of one turn sees the same value.
    pub turn_id: Arc<AtomicU64>,
    /// This agent's `wait_for` calls in the current turn, for the repeat
    /// warning. Fresh per agent: a fork shares its caller's tool instances,
    /// but its waits are its own.
    pub wait_tally: Arc<Mutex<crate::tools::builtin::wait_for::WaitTally>>,
    pub user_interaction: Option<Arc<dyn UserInteraction>>,
    pub search_url: Option<String>,
    pub plan_mode: Arc<AtomicBool>,
    /// The permission mode of the agent that owns this context. A spawned
    /// sub-agent copies it so it is gated exactly as its parent — never more
    /// permissively. The owning `Agent` keeps it current across runtime mode
    /// switches (`/yolo`, `/auto`, `/permission`).
    pub permission_mode: crate::permissions::PermissionMode,
    pub llm: Option<Arc<dyn Llm>>,
    pub vision_fallback: Option<Arc<dyn VisionFallback>>,
    pub web_search_client: Option<Arc<dyn WebSearchClient>>,
    pub web_reader_client: Option<Arc<dyn WebReaderClient>>,
    /// The prompt the running turn answers, as it was given — by the user,
    /// or by a trigger such as a scheduled task — before hooks or context
    /// were added to it: a search service reads the intent behind a query
    /// from it. `None` outside a turn.
    pub prompt: Option<Arc<str>>,
    pub background: Arc<BackgroundManager>,
    pub cron: Arc<CronManager>,
    pub goal_manager: Arc<Mutex<GoalManager>>,
    /// The session's user hooks (`[[hooks]]` in the user config), fired on turn,
    /// tool, session and task events. Inherited by sub-agents, so a delegated
    /// call is seen by the same rules as its parent's. See
    /// [`crate::user_hooks`].
    pub user_hooks: Option<Arc<crate::user_hooks::UserHooks>>,
    /// This agent's steering channel: messages the front-end injects into a
    /// running turn (Ctrl-S). The turn loop drains it into user messages before
    /// each model request, and `wait_for` selects on its wake source. Fresh per
    /// agent — never inherited: input steered while a sub-agent runs belongs to
    /// the session agent's turn, not the sub-agent's context.
    pub steer: Arc<crate::steer::SteerChannel>,
    pub tool_disclosure: Option<Arc<Mutex<ToolDisclosureState>>>,
    /// Files this agent has read, and how they looked. The write tools consult
    /// it before touching a file. Per-agent by construction: a sub-agent builds
    /// its own, because it does not share the parent's view of the workspace.
    /// See [`crate::stale_guard`].
    pub stale_guard: Arc<Mutex<crate::stale_guard::StaleGuard>>,
    /// The calling agent's conversation, for a sub-agent spawned with
    /// `fork: true`.
    ///
    /// Filled by the turn loop only for the batch that actually asked to fork,
    /// and cleared afterwards. Snapshotting unconditionally would clone the
    /// whole conversation on every step of every turn to serve a tool that is
    /// almost never called; a stale snapshot left behind would silently seed a
    /// later fork from the wrong point.
    pub fork_source: Arc<Mutex<Option<ForkSource>>>,
    /// For a fork, the caller's request prefix at fork time. The fork's first
    /// step compares its own prefix against it and logs at debug level when
    /// they differ — a prefix divergence costs the fork its caller's prompt
    /// cache. Taken by the first step, so only the first request is checked.
    pub fork_prefix_baseline: Arc<Mutex<Option<ForkPrefixBaseline>>>,
    /// How many sub-agent spawns deep this agent is. `0` is the session's own
    /// agent; a sub-agent it spawns runs at `1`, and so on.
    pub depth: usize,
    /// Tokens the session's sub-agents spent and no turn of the session's own
    /// agent has reported yet. Shared by the whole tree of agents: each
    /// sub-agent step adds its usage, and each turn of the session's agent
    /// takes what has gathered into its own ([`run_turn`](fn@crate::agent_loop::run_turn)).
    pub subagent_usage: Arc<Mutex<lm_bridge::usage::TokenUsage>>,
    /// Where this agent's sub-agents report their tool events, if anywhere.
    pub subagent_tool_events: Option<SubagentToolEventSender>,
    /// The deepest level still allowed to spawn. An agent at or beyond it has
    /// its `agent` and `agent_swarm` tools refuse.
    ///
    /// Sub-agents inherit the full built-in tool set, `agent` included, so
    /// without a bound a sub-agent can spawn sub-agents forever. That is not
    /// hypothetical: a tool call that keeps being denied reads to a model like a
    /// task to delegate, and each level multiplies rather than adds — the run
    /// burns tokens and wall-clock with nothing to show and no natural end.
    pub max_subagent_depth: usize,
    /// How tool results are bounded before they enter the model's context.
    /// Shared with sub-agents, so a delegated `bash` is capped like the
    /// parent's. See [`crate::tool_output`].
    pub tool_output: crate::tool_output::ToolOutputPolicy,
    /// Model requests one context compaction may make, retries and shrinks
    /// included. Sub-agents inherit it.
    pub compaction_max_attempts: u32,
    /// Files and directories tools may read, but not write, beyond the allowed
    /// roots: the session's records file and blob directory, which compaction
    /// summaries point the model at. See
    /// [`resolve_readable_path`](Self::resolve_readable_path).
    pub readable_paths: Vec<PathBuf>,
    /// Whether repeated tool calls earn reminders and, in the end, stop the
    /// turn (`repeat_breaker`). Sub-agents inherit it.
    pub repeat_breaker: bool,
    /// How long each `agent_swarm` member may run (`swarm_timeout_ms`);
    /// `None` leaves members unbounded. Sub-agents inherit it.
    pub swarm_timeout: Option<std::time::Duration>,
    /// How many `agent_swarm` members run at once (`swarm_max_concurrency`);
    /// `None` runs them all together. Sub-agents inherit it.
    pub swarm_max_concurrency: Option<usize>,
    /// Whether the built-in guards ask: about dangerous `bash` commands,
    /// sensitive-file writes and destructive git commands
    /// (`dangerous_command_guard`). Sub-agents inherit it.
    pub dangerous_command_guard: bool,
    /// Which instruction files (`AGENTS.md`) this agent has seen, and which
    /// it is to be reminded of. Per agent, from the files its own prompt
    /// carries: a sub-agent's carries the session's, or none without it.
    pub agents_md: Arc<Mutex<crate::agents_md::AgentsMdReminder>>,
    /// What a sub-agent inherits of this agent's approvals: the hook that asks
    /// the user and the configured policy rules. The owning `Agent` keeps it
    /// current as either is set.
    pub approval: crate::permissions::InheritedApproval,
    /// This agent's tool registry, from which a sub-agent whose profile takes
    /// MCP tools ([`crate::Profile::mcp_access`]) copies them.
    pub tool_registry: Option<Arc<Mutex<crate::tools::ToolRegistry>>>,
    /// System-prompt guidance for a sub-agent that gets MCP tools, such as
    /// Devscriptor's workflow, in the form for all of them and for the
    /// read-only ones; the session's own prompt carries the first.
    pub subagent_mcp_guidance: Option<Arc<crate::McpGuidance>>,
    /// The session's prompt as its sub-agents get it, with their role in its
    /// slot. `None` gives them their role and the environment section alone.
    pub subagent_prompt: Option<Arc<crate::prompt::SubagentPrompt>>,
    /// The session's skills, which a sub-agent's `skill` tool lists too.
    pub skill_registry: Option<Arc<crate::skills::SkillRegistry>>,
    /// The profile this agent runs under; `None` for the session's own agent.
    /// A fork keeps its caller's type, so `agent_swarm` checks a requested type
    /// against it.
    pub profile: Option<String>,
}

/// Default for [`ExecutableToolContext::swarm_timeout`]: two hours.
pub const DEFAULT_SWARM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

impl Default for ExecutableToolContext {
    /// A context rooted at the process's working directory, with fresh
    /// background, cron, todo and goal state and every optional collaborator
    /// unset.
    fn default() -> Self {
        let background = Arc::new(crate::background::BackgroundManager::new());
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            allowed_paths: HashSet::new(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            turn_id: Arc::new(AtomicU64::new(0)),
            wait_tally: Arc::default(),
            user_interaction: None,
            search_url: None,
            plan_mode: Arc::new(AtomicBool::new(false)),
            permission_mode: crate::permissions::PermissionMode::Default,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            prompt: None,
            cron: Arc::new(crate::cron::CronManager::new()),
            background,
            goal_manager: Arc::new(Mutex::new(GoalManager::new())),
            user_hooks: None,
            steer: Arc::new(crate::steer::SteerChannel::new()),
            tool_disclosure: None,
            stale_guard: Arc::default(),
            fork_source: Arc::default(),
            fork_prefix_baseline: Arc::default(),
            depth: 0,
            subagent_usage: Arc::default(),
            subagent_tool_events: None,
            max_subagent_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            tool_output: crate::tool_output::ToolOutputPolicy::default(),
            compaction_max_attempts: crate::context::DEFAULT_COMPACTION_MAX_ATTEMPTS,
            readable_paths: Vec::new(),
            repeat_breaker: true,
            swarm_timeout: Some(DEFAULT_SWARM_TIMEOUT),
            swarm_max_concurrency: None,
            dangerous_command_guard: true,
            agents_md: Arc::default(),
            approval: crate::permissions::InheritedApproval::default(),
            tool_registry: None,
            subagent_mcp_guidance: None,
            subagent_prompt: None,
            skill_registry: None,
            profile: None,
        }
    }
}

/// Default for [`ExecutableToolContext::max_subagent_depth`].
///
/// One level: the session's agent delegates, and its sub-agents do the work
/// themselves. A sub-agent that delegates again has, in practice, more often
/// been a loop than a plan; `max_subagent_depth` raises the bound where nesting
/// is wanted.
pub const DEFAULT_MAX_SUBAGENT_DEPTH: usize = 1;

/// What a forked sub-agent inherits from the agent that spawned it.
#[derive(Debug, Clone)]
pub struct ForkSource {
    /// The caller's system prompt. A fork is the caller continuing under a new
    /// budget, so it runs under the caller's instructions, not a profile's.
    pub system_prompt: String,
    /// The caller's conversation, as it stood when the fork was requested.
    pub history: Vec<lm_bridge::message::Message>,
}

/// The caller's request prefix a fresh fork's first request is checked
/// against: the system prompt and the offered tool table at fork time.
#[derive(Debug, Clone)]
pub struct ForkPrefixBaseline {
    /// The caller's system prompt, which the fork runs under too.
    pub system_prompt: String,
    /// The tool table the caller's step offered at fork time.
    pub tools: Vec<lm_bridge::tool::Tool>,
}

impl std::fmt::Debug for ExecutableToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutableToolContext")
            .field("depth", &self.depth)
            .field("cwd", &self.cwd)
            .field("allowed_paths", &self.allowed_paths)
            .field("todo_list", &self.todo_list)
            .field("tool_event_sender", &self.tool_event_sender.is_some())
            .field("turn_id", &self.turn_id.load(Ordering::SeqCst))
            .field("user_interaction", &self.user_interaction.is_some())
            .field("search_url", &self.search_url)
            .field("plan_mode", &self.plan_mode.load(Ordering::SeqCst))
            .field("permission_mode", &self.permission_mode)
            .field("llm", &self.llm.is_some())
            .field("vision_fallback", &self.vision_fallback.is_some())
            .field("web_search_client", &self.web_search_client.is_some())
            .field("web_reader_client", &self.web_reader_client.is_some())
            .field("background", &self.background.list().len())
            .field("cron", &self.cron.list().len())
            .field(
                "goal_manager",
                &self.goal_manager.lock().unwrap().active_goal().is_some(),
            )
            .field("user_hooks", &self.user_hooks.is_some())
            .field("tool_disclosure", &self.tool_disclosure.is_some())
            .field("approval", &self.approval)
            .field("profile", &self.profile)
            .finish()
    }
}

impl ExecutableToolContext {
    /// Emit a tool lifecycle event if a sender is configured.
    pub fn emit_tool_event(&self, event: ToolEvent) {
        if let Some(sender) = &self.tool_event_sender {
            sender(event);
        }
    }

    /// Ask the user a question via the configured interaction channel.
    pub async fn ask_user(&self, question: &str) -> Option<String> {
        if let Some(interaction) = &self.user_interaction {
            interaction.ask(question).await
        } else {
            None
        }
    }

    /// Ask the user one or more structured multiple-choice questions.
    pub async fn ask_user_question(&self, questions: &[Question]) -> Option<QuestionAnswers> {
        if let Some(interaction) = &self.user_interaction {
            interaction.ask_question(questions).await
        } else {
            None
        }
    }

    /// Resolve `path` relative to `cwd` and verify it lies within an allowed root.
    ///
    /// Allowed roots are the agent cwd plus any paths registered via
    /// `--allow-path` / `allowed_paths`. Returns the canonical or resolved path
    /// on success.
    pub fn resolve_allowed_path(
        &self,
        path: &std::path::Path,
    ) -> Result<std::path::PathBuf, String> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };

        let canonical = canonicalize_or_resolve(&abs)
            .map_err(|e| format!("Failed to resolve {}: {e}", abs.display()))?;

        let roots = self.allowed_roots();
        for root in roots {
            if let Ok(root_canonical) = canonicalize_existing(&root)
                && canonical.starts_with(&root_canonical)
            {
                return Ok(canonical);
            }
        }

        Err(format!(
            "Path {} is outside allowed directories (cwd and --allow-path)",
            canonical.display()
        ))
    }

    /// Like [`resolve_allowed_path`](Self::resolve_allowed_path), but also
    /// accepts this run's spill files and the [`readable_paths`](Self::readable_paths).
    ///
    /// A tool result cut to fit the context points the model at a spill file
    /// holding the full output ([`crate::tool_output`]), and a compaction
    /// summary points it at the records file. Both live in the data directory,
    /// outside the allowed roots, so tools that read — never those that write —
    /// resolve paths through this method.
    pub fn resolve_readable_path(
        &self,
        path: &std::path::Path,
    ) -> Result<std::path::PathBuf, String> {
        let denied = match self.resolve_allowed_path(path) {
            Ok(resolved) => return Ok(resolved),
            Err(denied) => denied,
        };
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        let Ok(canonical) = canonicalize_or_resolve(&abs) else {
            return Err(denied);
        };
        let readable = std::iter::once(&self.tool_output.spill_dir).chain(&self.readable_paths);
        for root in readable {
            if let Ok(root) = canonicalize_existing(root)
                && canonical.starts_with(&root)
            {
                return Ok(canonical);
            }
        }
        Err(denied)
    }

    fn allowed_roots(&self) -> Vec<std::path::PathBuf> {
        let mut roots = vec![self.cwd.clone()];
        roots.extend(self.allowed_paths.iter().cloned());
        roots
    }
}

/// `path` made canonical, or, when it does not exist yet, where it would be.
///
/// A path that does not exist resolves through its nearest ancestor that
/// does: that ancestor is made canonical — symlinks and all — and the missing
/// components go back on after it. So a file in a directory that does not
/// exist yet, which `write_file` creates, resolves to where it will land
/// rather than failing.
///
/// A `..` among the missing components is refused. It cannot be resolved
/// against a directory that is not there, and appending it as written would
/// let `new/../../outside` pass a check that the result starts with an
/// allowed root.
fn canonicalize_or_resolve(path: &std::path::Path) -> Result<std::path::PathBuf, std::io::Error> {
    if path.exists() {
        return path.canonicalize();
    }
    let invalid =
        |message: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, message.to_string());
    let mut missing = Vec::new();
    let mut ancestor = path;
    let base = loop {
        let name = ancestor.file_name().ok_or_else(|| {
            invalid("`..` after a directory that does not exist cannot be resolved")
        })?;
        missing.push(name);
        let Some(parent) = ancestor.parent() else {
            return Err(invalid("invalid path"));
        };
        if parent.as_os_str().is_empty() {
            break std::env::current_dir()?;
        }
        if parent.exists() {
            break parent.canonicalize()?;
        }
        ancestor = parent;
    };
    Ok(missing
        .iter()
        .rev()
        .fold(base, |resolved, name| resolved.join(name)))
}

fn canonicalize_existing(path: &std::path::Path) -> Result<std::path::PathBuf, std::io::Error> {
    if path.exists() {
        path.canonicalize()
    } else {
        Ok(path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn ctx_with_allowed(allowed: Vec<std::path::PathBuf>) -> ExecutableToolContext {
        let background = Arc::new(crate::background::BackgroundManager::new());
        ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::env::current_dir().unwrap(),
            allowed_paths: allowed.into_iter().collect::<HashSet<_>>(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            user_interaction: None,
            search_url: None,
            plan_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: crate::permissions::PermissionMode::Default,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            background: background.clone(),
            goal_manager: std::sync::Arc::new(std::sync::Mutex::new(
                crate::goal::GoalManager::new(),
            )),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            cron: Arc::new(crate::cron::CronManager::new()),
            ..Default::default()
        }
    }

    #[test]
    fn test_resolve_allows_cwd_relative_path() {
        let ctx = ctx_with_allowed(Vec::new());
        let result = ctx.resolve_allowed_path(std::path::Path::new("Cargo.toml"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_resolve_denies_path_outside_cwd() {
        let ctx = ctx_with_allowed(Vec::new());
        let result = ctx.resolve_allowed_path(std::path::Path::new("/etc/passwd"));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_allows_explicit_allowed_path() {
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("notes.txt");
        std::fs::write(&file, "notes").unwrap();
        let ctx = ctx_with_allowed(vec![outside.path().to_path_buf()]);
        assert!(ctx.resolve_allowed_path(&file).is_ok());
    }

    #[test]
    fn test_resolve_denies_traversal_above_cwd() {
        let ctx = ctx_with_allowed(Vec::new());
        let result = ctx.resolve_allowed_path(std::path::Path::new("../Cargo.toml"));
        assert!(result.is_err());
    }

    /// A file in directories that do not exist yet — which `write_file`
    /// creates — resolves to where it will land inside the workspace.
    #[test]
    fn a_file_in_a_new_directory_resolves_inside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let ctx = ExecutableToolContext {
            cwd: workspace.path().to_path_buf(),
            ..Default::default()
        };
        let resolved = ctx
            .resolve_allowed_path(std::path::Path::new("docs/guides/ONBOARDING.md"))
            .unwrap();
        assert_eq!(
            resolved,
            workspace
                .path()
                .canonicalize()
                .unwrap()
                .join("docs/guides/ONBOARDING.md")
        );
    }

    /// `..` past a directory that does not exist cannot be resolved, and
    /// must not slip out of the workspace as written.
    #[test]
    fn a_new_directory_cannot_be_used_to_climb_out_of_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let ctx = ExecutableToolContext {
            cwd: workspace.path().to_path_buf(),
            ..Default::default()
        };
        for escape in ["new/../../outside.txt", "new/deeper/../../../outside.txt"] {
            assert!(
                ctx.resolve_allowed_path(std::path::Path::new(escape))
                    .is_err(),
                "{escape} must be refused"
            );
        }
        let outside = ctx.resolve_allowed_path(&workspace.path().join("../elsewhere/new/file.txt"));
        assert!(
            outside.is_err(),
            "a new directory outside the workspace stays outside"
        );
    }

    /// Spill files are readable although they sit outside the allowed roots;
    /// the rest of the filesystem stays off limits, and writes do not get the
    /// exemption.
    #[test]
    fn spill_files_are_readable_but_not_writable() {
        let workspace = tempfile::tempdir().unwrap();
        let spill = tempfile::tempdir().unwrap();
        let spill_file = spill.path().join("call_1.txt");
        std::fs::write(&spill_file, "full output").unwrap();
        let ctx = ExecutableToolContext {
            cwd: workspace.path().to_path_buf(),
            tool_output: crate::tool_output::ToolOutputPolicy::new(100, spill.path()),
            ..Default::default()
        };

        let resolved = ctx.resolve_readable_path(&spill_file).unwrap();
        assert_eq!(resolved, spill_file.canonicalize().unwrap());
        assert!(ctx.resolve_allowed_path(&spill_file).is_err());
        assert!(
            ctx.resolve_readable_path(std::path::Path::new("/etc/passwd"))
                .is_err()
        );
    }

    /// The records file and blob directory are readable once named; a sibling
    /// of the records file is not.
    #[test]
    fn readable_paths_cover_exactly_what_they_name() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let records = data.path().join("records.jsonl");
        let blobs = data.path().join("blobs");
        std::fs::write(&records, "{}").unwrap();
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("01ABC"), "blob").unwrap();
        std::fs::write(data.path().join("records.jsonl.goals.json"), "{}").unwrap();
        let ctx = ExecutableToolContext {
            cwd: workspace.path().to_path_buf(),
            readable_paths: vec![records.clone(), blobs.clone()],
            ..Default::default()
        };

        assert!(ctx.resolve_readable_path(&records).is_ok());
        assert!(ctx.resolve_readable_path(&blobs.join("01ABC")).is_ok());
        assert!(
            ctx.resolve_readable_path(&data.path().join("records.jsonl.goals.json"))
                .is_err()
        );
        assert!(ctx.resolve_allowed_path(&records).is_err());
    }
}

/// A tool that can be executed by the agent loop.
#[async_trait]
pub trait ExecutableTool: Send + Sync {
    fn tool_definition(&self) -> &Tool;

    /// This tool for a fork of its agent: `None`, the default, shares this
    /// instance; a tool that holds its agent's own state returns a copy with
    /// the same definition, bound to the fork's ([`ForkBinding`]).
    ///
    /// A fork's tool table is its caller's, in the caller's order and with the
    /// caller's definitions, so its requests repeat the caller's prefix and hit
    /// its prompt cache. Sharing suits every tool whose state lives in the
    /// [`ExecutableToolContext`], which each agent has its own of.
    fn rebind_for_fork(&self, _binding: &ForkBinding) -> Option<Arc<dyn ExecutableTool>> {
        None
    }

    /// Whether this tool can do its work in `ctx`. One that cannot, such as
    /// `web_search` without a search service, is left out of the tool table
    /// instead of being offered only to fail. The default is `true`. It is
    /// asked with the tool registry locked, so it must not lock the registry
    /// itself.
    fn is_available(&self, _ctx: &ExecutableToolContext) -> bool {
        true
    }

    /// `input` under the names this tool's schema gives its arguments, for a
    /// tool whose arguments models often name otherwise. Asked before the
    /// call is checked against the schema, so that an argument the schema
    /// requires is not refused for its name; the call then runs, is approved
    /// and is scheduled with what this returns. The default leaves `input`
    /// as it is.
    fn normalize_arguments(&self, input: serde_json::Value) -> serde_json::Value {
        input
    }

    /// Accesses this tool will perform; used for scheduling.
    fn accesses(&self, input: &serde_json::Value) -> ToolAccesses;

    /// Approval rule ID for this invocation.
    fn approval_rule(&self, input: &serde_json::Value) -> &str;

    /// Whether every call only reads: it changes no file and no other state.
    /// A read-only sub-agent (`explore`, `plan`) gets the session's MCP tools
    /// that are; an MCP tool is one when its server declares `readOnlyHint`.
    /// The default is `false`.
    fn is_read_only(&self) -> bool {
        false
    }

    /// A call-specific refusal checked after argument validation but before the
    /// approval gate and execution. Most tools never refuse here; the cron
    /// tools use it for their plan-mode veto and their pre-approval validation,
    /// so the user is never asked about a call that cannot run.
    fn preflight(
        &self,
        _input: &serde_json::Value,
        _ctx: &ExecutableToolContext,
    ) -> Result<(), String> {
        Ok(())
    }

    /// What the model is told when the call needed approval and did not get
    /// it: refused by the user (`asked`), or with nobody to ask. `None` uses
    /// the general refusal.
    fn refusal(&self, _asked: bool) -> Option<&'static str> {
        None
    }

    /// Execute the tool.
    ///
    /// `tool_call_id` is the provider-assigned id for this invocation and should
    /// be used when emitting lifecycle events so callers can correlate output.
    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        tool_call_id: &str,
    ) -> ExecutableToolResult;
}

/// The state of a fork that [`ExecutableTool::rebind_for_fork`] binds a
/// tool to.
pub struct ForkBinding {
    /// The fork's tool registry. Weak, as the registry owns its tools.
    pub registry: Weak<Mutex<crate::tools::ToolRegistry>>,
    /// Which deferred tools the fork has loaded.
    pub disclosure: Arc<Mutex<ToolDisclosureState>>,
    /// The skills the fork has used.
    pub active_skills: Arc<Mutex<crate::skills::ActiveSkills>>,
}

/// Outcome of one provider step.
#[derive(Debug, Clone)]
pub struct StepResult {
    /// The assistant turn the model produced: its text, its reasoning, and its
    /// tool calls.
    ///
    /// Carrying the whole message rather than only the tool calls is what lets
    /// the turn loop record what the model actually said. Without it the
    /// assistant's text and reasoning reach the terminal through streaming
    /// callbacks and are then discarded, so nothing survives into the next
    /// request: Preserved Thinking has no reasoning to replay, a sub-agent has
    /// no answer to report back, and a resumed session has no final answer.
    pub message: lm_bridge::message::Message,
    /// Tokens the step used. On the result of a whole turn ([`run_turn`],
    /// [`crate::Agent::prompt`]) it is every step of the turn, summed.
    ///
    /// [`run_turn`]: fn@crate::agent_loop::run_turn
    pub usage: TokenUsage,
    pub finish_reason: Option<lm_bridge::provider::FinishReason>,
    /// Provider response id, useful for observability and wire-log correlation.
    pub response_id: Option<String>,
    /// Why the turn was stopped before the model finished on its own; `None`
    /// when it finished. A stopped turn's message is its handoff: what was
    /// done, what is blocked, and the next step.
    pub stop_reason: Option<StopReason>,
}

/// Why a turn was stopped before the model finished on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The repeated-call guard stopped a model stuck calling the same tool.
    RepeatBreaker,
    /// The turn used all the steps it may take (`max_turns`).
    MaxTurns,
    /// The turn was cancelled.
    Cancelled,
    /// A sub-agent ran past its time limit and was cancelled.
    TimedOut,
}

impl StopReason {
    /// The machine name: `repeat_breaker`, `max_turns`, `cancelled`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepeatBreaker => "repeat_breaker",
            Self::MaxTurns => "max_turns",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    /// What happened, for the model and the user.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::RepeatBreaker => "it kept repeating the same tool call",
            Self::MaxTurns => "it used all the steps one turn may take",
            Self::Cancelled => "it was cancelled",
            Self::TimedOut => "it ran out of time",
        }
    }
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl StepResult {
    /// The tool calls in this step, if any.
    ///
    /// A view onto [`Self::message`] rather than a second copy, so the two
    /// cannot drift apart.
    #[must_use]
    pub fn tool_calls(&self) -> &[lm_bridge::message::ToolCall] {
        &self.message.tool_calls
    }

    /// The assistant's text for this step, empty when it only called tools.
    #[must_use]
    pub fn text(&self) -> String {
        self.message.text_content()
    }
}
