use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Lines},
    process::{Child, ChildStderr, ChildStdout, Command},
    sync::{Notify, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

/// The stdout/stderr line readers handed over when a foreground command is
/// adopted into the background — already partially consumed, still live.
type AdoptedReaders = (Lines<BufReader<ChildStdout>>, Lines<BufReader<ChildStderr>>);

/// Owner ids for [`BackgroundManager`] handles; see
/// [`BackgroundManager::scoped`].
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

const DEFAULT_MAX_RUNNING_TASKS: usize = 5;
const DEFAULT_MEMORY_LINE_CAP: usize = 1_000;
const DEFAULT_MAX_TRACKED_TASKS: usize = 100;
/// How long a sub-agent may run before it is cancelled. Long enough that a
/// genuine multi-step delegation finishes, short enough that a wedged one does
/// not hang an unattended `-p` run forever.
const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
/// Maximum combined stdout/stderr bytes to capture for a background shell task.
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const OUTPUT_TRUNCATION_MARKER: &str = "[Output truncated after 16 MiB]";

/// Lifecycle status of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Stopped,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queued => write!(f, "queued"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

impl TaskStatus {
    /// Whether the task has finished, however it finished.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
    }
}

/// What kind of work a background task is running.
///
/// Only [`TaskKind::Agent`] tasks are waited for when a headless turn ends: a
/// sub-agent's findings are meant to come back to the model, whereas a detached
/// shell command (a dev server, a file watcher) is expected to outlive the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// A delegated sub-agent (`agent`, `agent_swarm`).
    Agent,
    /// A detached shell command.
    Shell,
}

/// A detached background command and its captured output.
pub struct BackgroundTask {
    pub id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub status: TaskStatus,
    pub kind: TaskKind,
    /// The handle that started it: the agent its completion is reported to.
    owner: u64,
    /// Whether its completion notice has been posted. A task is settled for
    /// [`BackgroundManager::wait_for`] only then, so a wait never reports a
    /// task whose notice would still arrive afterwards.
    notified: bool,
    stdout: VecDeque<String>,
    stderr: VecDeque<String>,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub exit_code: Option<i32>,
    pub created_at: DateTime<Utc>,
    /// Monotonic spawn order. `created_at` alone is not a total order: tasks
    /// spawned in a tight loop can share a timestamp on a coarse clock (Windows
    /// resolves to ~15 ms), and ties would then be broken by `HashMap` iteration
    /// order — i.e. arbitrarily.
    seq: u64,
    cancel: CancellationToken,
    memory_line_cap: usize,
    stdout_bytes: usize,
    stderr_bytes: usize,
    output_capped: bool,
}

/// The manager-owned settings every new task inherits.
///
/// Grouped because they belong to the [`BackgroundManager`], not to the task —
/// and because threading them individually put `BackgroundTask::new` over the
/// argument limit once spawn order joined them.
#[derive(Clone, Copy)]
struct TaskEnv<'a> {
    data_dir: &'a Path,
    memory_line_cap: usize,
    owner: u64,
}

impl BackgroundTask {
    fn new(
        id: String,
        command: String,
        cwd: PathBuf,
        kind: TaskKind,
        cancel: CancellationToken,
        env: TaskEnv<'_>,
        seq: u64,
    ) -> Self {
        let task_dir = env.data_dir.join("tasks").join(&id);
        std::fs::create_dir_all(&task_dir).ok();
        Self {
            id,
            command,
            cwd,
            status: TaskStatus::Queued,
            kind,
            owner: env.owner,
            notified: false,
            stdout: VecDeque::new(),
            stderr: VecDeque::new(),
            stdout_path: task_dir.join("stdout.txt"),
            stderr_path: task_dir.join("stderr.txt"),
            exit_code: None,
            created_at: Utc::now(),
            seq,
            cancel,
            memory_line_cap: env.memory_line_cap,
            stdout_bytes: 0,
            stderr_bytes: 0,
            output_capped: false,
        }
    }

    /// Return a short single-line summary suitable for `task_list`.
    pub fn summary(&self) -> String {
        format!(
            "{} [{}] {} (cwd: {})",
            self.id,
            self.status,
            self.command,
            self.cwd.display()
        )
    }
}

/// A task's completion notice, waiting for the agent that owns the task.
struct CompletionMarker {
    owner: u64,
    task_id: String,
    text: String,
}

/// A task a [`BackgroundManager::wait_for`] saw finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedTask {
    pub id: String,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    /// The command, or a sub-agent's label.
    pub command: String,
}

/// How a [`BackgroundManager::wait_for`] ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaitOutcome {
    /// Waited-on tasks that had finished when the wait ended, in start order.
    pub finished: Vec<FinishedTask>,
    /// Waited-on tasks still running, in start order.
    pub running: Vec<String>,
    /// Whether the wait ran out of time.
    pub timed_out: bool,
    /// Whether new user input ended the wait early. The tasks keep running;
    /// their completion still arrives via the usual notification.
    pub interrupted: bool,
}

/// Why a [`BackgroundManager::wait_for`] could not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    /// No task of this agent has the id.
    #[error("Task not found: {0}. Only tasks this agent started can be waited on.")]
    Unknown(String),
}

/// Manager for detached long-running shell commands and async agents.
///
/// One manager serves an agent and every sub-agent it spawns, each through
/// its own [`scoped`](Self::scoped) handle: tasks are shared for listing,
/// output and stopping, but a task's completion is reported only to the agent
/// that started it, and only that agent can wait on it.
#[derive(Clone)]
pub struct BackgroundManager {
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
    counter: Arc<AtomicU64>,
    data_dir: PathBuf,
    max_running_tasks: usize,
    memory_line_cap: usize,
    max_tracked_tasks: usize,
    semaphore: Arc<Mutex<Option<Arc<Semaphore>>>>,
    completion_markers: Arc<Mutex<Vec<CompletionMarker>>>,
    /// Signalled whenever a completion notice is posted.
    finished: Arc<Notify>,
    /// Deadline applied to sub-agents; `None` lets them run unbounded.
    agent_timeout: Option<Duration>,
    /// The session's user hooks, when configured: TaskStarted fires here when
    /// a task spawns and Notification when one reaches a terminal state.
    user_hooks: Arc<std::sync::Mutex<Option<Arc<crate::user_hooks::UserHooks>>>>,
    /// The agent this handle belongs to. Clones share it.
    owner: u64,
    /// In unit tests, the temporary directory standing in for the user's data
    /// directory, removed with the last handle on it.
    #[cfg(test)]
    scratch: Option<Arc<tempfile::TempDir>>,
}

impl BackgroundManager {
    /// Create a manager with the default data directory and limits.
    ///
    /// In unit tests the data directory is a temporary one of the manager's
    /// own. On the user's, every test run left sub-agent records and task
    /// output behind, and tests overwrote each other's `tasks/task-1`.
    pub fn new() -> Self {
        #[cfg(test)]
        {
            let scratch = Arc::new(tempfile::tempdir().expect("a temporary data directory"));
            let mut manager = Self::with_data_dir(scratch.path().to_path_buf());
            manager.scratch = Some(scratch);
            manager
        }
        #[cfg(not(test))]
        {
            let data_dir = crate::user_dirs::data_dir()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
                .join("rusta");
            Self::with_data_dir(data_dir)
        }
    }

    /// A new manager — default limits, no tasks — for this one's data
    /// directory.
    #[must_use]
    pub fn for_same_data_dir(&self) -> Self {
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut manager = Self::with_data_dir(self.data_dir.clone());
        #[cfg(test)]
        {
            manager.scratch = self.scratch.clone();
        }
        *manager.user_hooks.lock().unwrap() = self.user_hooks.lock().unwrap().clone();
        manager
    }

    /// Attach the session's user hooks. Background tasks report their starts
    /// (TaskStarted) and terminal states (Notification) through them.
    pub fn set_user_hooks(&self, hooks: Arc<crate::user_hooks::UserHooks>) {
        *self.user_hooks.lock().unwrap() = Some(hooks);
    }

    /// Create a manager with a custom data directory.
    pub fn with_data_dir(data_dir: PathBuf) -> Self {
        std::fs::create_dir_all(&data_dir).ok();
        let semaphore = Arc::new(Mutex::new(Some(Arc::new(Semaphore::new(
            DEFAULT_MAX_RUNNING_TASKS,
        )))));
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            counter: Arc::new(AtomicU64::new(1)),
            data_dir,
            max_running_tasks: DEFAULT_MAX_RUNNING_TASKS,
            memory_line_cap: DEFAULT_MEMORY_LINE_CAP,
            max_tracked_tasks: DEFAULT_MAX_TRACKED_TASKS,
            semaphore,
            completion_markers: Arc::new(Mutex::new(Vec::new())),
            finished: Arc::new(Notify::new()),
            agent_timeout: Some(DEFAULT_AGENT_TIMEOUT),
            user_hooks: Arc::new(std::sync::Mutex::new(None)),
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            #[cfg(test)]
            scratch: None,
        }
    }

    /// A handle on the same tasks for another agent (a sub-agent). Tasks it
    /// starts report their completion to it, not to this handle's agent.
    #[must_use]
    pub fn scoped(&self) -> Self {
        Self {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            ..self.clone()
        }
    }

    /// The settings a task started through this handle inherits.
    fn task_env(&self) -> TaskEnv<'_> {
        TaskEnv {
            data_dir: &self.data_dir,
            memory_line_cap: self.memory_line_cap,
            owner: self.owner,
        }
    }

    /// Fire the TaskStarted event for a new background task.
    fn fire_task_started(&self, id: &str, description: &str, kind: &str) {
        let hooks = self.user_hooks.lock().unwrap().clone();
        let Some(hooks) = hooks else {
            return;
        };
        let mut fields = serde_json::Map::new();
        fields.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        fields.insert(
            "description".to_string(),
            serde_json::Value::String(description.to_string()),
        );
        fields.insert("detached".to_string(), serde_json::Value::Bool(true));
        hooks.fire_spawned(
            crate::user_hooks::UserHookEvent::TaskStarted,
            kind.to_string(),
            fields,
        );
    }

    /// Set how long a sub-agent may run before it is cancelled.
    ///
    /// `None` removes the deadline. Prefer a bound: a headless turn waits for
    /// agent tasks to settle, so an unbounded agent that wedges hangs the run.
    #[must_use]
    pub fn with_agent_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.agent_timeout = timeout;
        self
    }

    /// The configured sub-agent deadline.
    #[must_use]
    pub fn agent_timeout(&self) -> Option<Duration> {
        self.agent_timeout
    }

    /// Set the maximum number of tasks that may run concurrently.
    ///
    /// A value of `0` disables the limit. This should be called before any
    /// tasks are spawned; it does not migrate existing in-memory tasks.
    pub fn with_max_running_tasks(mut self, max: usize) -> Self {
        self.max_running_tasks = max;
        let sem = if max == 0 {
            None
        } else {
            Some(Arc::new(Semaphore::new(max)))
        };
        *self.semaphore.lock().unwrap() = sem;
        self
    }

    /// Set the number of output lines kept in memory per task.
    pub fn with_memory_line_cap(mut self, cap: usize) -> Self {
        self.memory_line_cap = cap;
        self
    }

    /// Set the maximum number of terminal tasks to keep in memory and on disk.
    ///
    /// Oldest completed/failed/stopped tasks are removed first. A value of `0`
    /// disables cleanup.
    pub fn with_max_tracked_tasks(mut self, max: usize) -> Self {
        self.max_tracked_tasks = max;
        self
    }

    /// Return the configured data directory.
    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    /// Return the configured concurrency limit.
    pub fn max_running_tasks(&self) -> usize {
        self.max_running_tasks
    }

    /// Drain this agent's background-task completion notifications that have
    /// accumulated since the last turn. Other agents' notices stay put.
    pub fn drain_completion_markers(&self) -> Vec<String> {
        self.take_markers(|_| true)
    }

    /// Number of this agent's pending background-task completion
    /// notifications.
    pub fn pending_completion_count(&self) -> usize {
        self.completion_markers
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.owner == self.owner)
            .count()
    }

    /// Remove and return this agent's notices that `wanted` accepts.
    fn take_markers(&self, wanted: impl Fn(&str) -> bool) -> Vec<String> {
        let mut markers = self.completion_markers.lock().unwrap();
        let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut *markers)
            .into_iter()
            .partition(|m| m.owner == self.owner && wanted(&m.task_id));
        *markers = kept;
        taken.into_iter().map(|m| m.text).collect()
    }

    /// Wait until a task finishes, or `timeout` passes.
    ///
    /// With `task_id`, waits for that task, which must be this agent's; an
    /// already-finished task returns at once. Without it, waits for any of
    /// this agent's tasks running at the call; with none running, returns at
    /// once. Waiting never stops a task, and dropping the future (a user
    /// interrupt) ends only the wait. Finished tasks' completion notices are
    /// taken, so each finish is reported once: here, not again at the next
    /// turn.
    ///
    /// A message steered into the running turn through `steer` while the
    /// wait is pending ends it early: the outcome is
    /// [`WaitOutcome::interrupted`] and the tasks keep running.
    pub async fn wait_for(
        &self,
        task_id: Option<&str>,
        timeout: Duration,
        steer: &crate::steer::SteerChannel,
    ) -> Result<WaitOutcome, WaitError> {
        let watched: Vec<String> = {
            let tasks = self.tasks.lock().unwrap();
            match task_id {
                Some(id) => match tasks.get(id) {
                    Some(task) if task.owner == self.owner => vec![id.to_string()],
                    _ => return Err(WaitError::Unknown(id.to_string())),
                },
                None => {
                    let mut running: Vec<&BackgroundTask> = tasks
                        .values()
                        .filter(|t| t.owner == self.owner && !t.notified)
                        .collect();
                    running.sort_by_key(|t| t.seq);
                    running.iter().map(|t| t.id.clone()).collect()
                }
            }
        };
        if watched.is_empty() {
            return Ok(WaitOutcome::default());
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let mut timed_out = false;
        let mut interrupted = false;
        loop {
            // Both registered before the checks below, so a notice posted or a
            // message steered in between is not missed.
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let steered = steer.notified();
            if self.any_settled(&watched) {
                break;
            }
            if !steer.is_empty() {
                interrupted = true;
                break;
            }
            tokio::select! {
                woken = tokio::time::timeout_at(deadline, notified) => {
                    if woken.is_err() {
                        timed_out = !self.any_settled(&watched);
                        break;
                    }
                    // A task finished, perhaps not a watched one: the checks
                    // above tell.
                }
                // A steered message ends the wait at the checks above. A wake
                // whose message the turn already took finds the channel empty
                // and waits on.
                () = steered => {}
            }
        }

        let mut outcome = WaitOutcome {
            timed_out,
            interrupted,
            ..WaitOutcome::default()
        };
        {
            let tasks = self.tasks.lock().unwrap();
            for id in &watched {
                match tasks.get(id) {
                    Some(task) if task.notified => outcome.finished.push(FinishedTask {
                        id: id.clone(),
                        status: task.status,
                        exit_code: task.exit_code,
                        command: task.command.clone(),
                    }),
                    Some(_) => outcome.running.push(id.clone()),
                    // Pruned after finishing: nothing left to report.
                    None => {}
                }
            }
        }
        let reported: Vec<&str> = outcome.finished.iter().map(|t| t.id.as_str()).collect();
        self.take_markers(|id| reported.contains(&id));
        Ok(outcome)
    }

    /// Whether any of `ids` has finished and been announced, or is gone.
    fn any_settled(&self, ids: &[String]) -> bool {
        let tasks = self.tasks.lock().unwrap();
        ids.iter()
            .any(|id| tasks.get(id).is_none_or(|task| task.notified))
    }

    /// Number of sub-agent tasks that have not yet reached a terminal state.
    ///
    /// Queued counts: a sub-agent still waiting on the concurrency semaphore
    /// has not run yet, and dropping it would silently discard its work.
    pub fn active_agent_task_count(&self) -> usize {
        self.tasks
            .lock()
            .unwrap()
            .values()
            .filter(|t| t.kind == TaskKind::Agent && !t.status.is_terminal())
            .count()
    }

    /// Spawn an arbitrary async task, a sub-agent working in `cwd`, under the
    /// manager's sub-agent deadline ([`agent_timeout`](Self::agent_timeout))
    /// and return its task id.
    pub fn spawn_agent<F, Fut>(&self, label: impl Into<String>, cwd: PathBuf, task: F) -> String
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
    {
        self.spawn_agent_with_timeout(label, cwd, self.agent_timeout, task)
    }

    /// Spawn an arbitrary async task, a sub-agent working in `cwd`, under
    /// `agent_timeout` — `None` for a task that bounds itself — and return
    /// its task id.
    pub fn spawn_agent_with_timeout<F, Fut>(
        &self,
        label: impl Into<String>,
        cwd: PathBuf,
        agent_timeout: Option<Duration>,
        task: F,
    ) -> String
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
    {
        let label = label.into();
        let seq = self.counter.fetch_add(1, Ordering::SeqCst);
        let id = format!("agent-{}-{}", label.replace(' ', "-"), seq);
        let cancel = CancellationToken::new();
        self.fire_task_started(&id, &label, "agent");

        {
            let mut tasks = self.tasks.lock().unwrap();
            tasks.insert(
                id.clone(),
                BackgroundTask::new(
                    id.clone(),
                    label.clone(),
                    cwd,
                    TaskKind::Agent,
                    cancel.clone(),
                    self.task_env(),
                    seq,
                ),
            );
        }

        let tasks = self.tasks.clone();
        let semaphore = self.semaphore.clone();
        let completion_markers = self.completion_markers.clone();
        let finished = self.finished.clone();
        let max_tracked_tasks = self.max_tracked_tasks;
        let user_hooks = self.user_hooks.lock().unwrap().clone();
        let id_spawn = id.clone();
        tokio::spawn(async move {
            let sem = {
                let guard = semaphore.lock().unwrap();
                guard.clone()
            };
            let _permit = if let Some(sem) = sem.as_ref() {
                Some(sem.acquire().await.expect("semaphore should not be closed"))
            } else {
                None
            };
            set_status(&tasks, &id_spawn, TaskStatus::Running);
            // Bound the sub-agent. A headless `-p` turn *waits* for agent tasks
            // (`drain_agent_tasks_on_stop`), and that drain has no deadline of its
            // own, so an agent that never settles — a model looping, a wedged
            // tool — hangs the whole run. Interactively that is a Ctrl-C away;
            // unattended (CI, cron) it hangs forever. Cancelling on the deadline
            // lets the agent's own cancellation path unwind and report.
            let outcome = match agent_timeout {
                Some(limit) => match tokio::time::timeout(limit, task(cancel.clone())).await {
                    Ok(result) => result,
                    Err(_) => {
                        cancel.cancel();
                        Err(format!(
                            "sub-agent exceeded its {} s time limit and was cancelled",
                            limit.as_secs()
                        ))
                    }
                },
                None => task(cancel.clone()).await,
            };
            match outcome {
                Ok(text) => {
                    let mut out = OutputFile::default();
                    for line in text.lines() {
                        append_stdout(&tasks, &id_spawn, line.to_string(), &mut out);
                    }
                    set_status(&tasks, &id_spawn, TaskStatus::Completed);
                }
                Err(err) => {
                    let mut out = OutputFile::default();
                    for line in err.lines() {
                        append_stderr(&tasks, &id_spawn, line.to_string(), &mut out);
                    }
                    set_status(&tasks, &id_spawn, TaskStatus::Failed);
                }
            }
            prune_terminal_tasks(&tasks, max_tracked_tasks);
            notify_completion(
                &tasks,
                &completion_markers,
                &finished,
                &id_spawn,
                user_hooks.as_ref(),
            );
        });

        id
    }

    /// Spawn a background shell command and return its task id.
    ///
    /// `timeout` bounds how long the task may run; `None` means the task runs
    /// until it exits or is stopped explicitly.
    pub fn spawn(
        &self,
        command: impl Into<String>,
        cwd: PathBuf,
        env: HashMap<String, String>,
        timeout: Option<Duration>,
    ) -> String {
        let command = command.into();
        let seq = self.counter.fetch_add(1, Ordering::SeqCst);
        let id = format!("task-{seq}");
        let cancel = CancellationToken::new();
        self.fire_task_started(&id, &command, "process");

        {
            let mut tasks = self.tasks.lock().unwrap();
            tasks.insert(
                id.clone(),
                BackgroundTask::new(
                    id.clone(),
                    command.clone(),
                    cwd.clone(),
                    TaskKind::Shell,
                    cancel.clone(),
                    self.task_env(),
                    seq,
                ),
            );
        }

        let tasks = self.tasks.clone();
        let semaphore = self.semaphore.clone();
        let completion_markers = self.completion_markers.clone();
        let finished = self.finished.clone();
        let max_tracked_tasks = self.max_tracked_tasks;
        let user_hooks = self.user_hooks.lock().unwrap().clone();
        let id_spawn = id.clone();
        tokio::spawn(async move {
            let sem = {
                let guard = semaphore.lock().unwrap();
                guard.clone()
            };
            let _permit = if let Some(sem) = sem.as_ref() {
                Some(sem.acquire().await.expect("semaphore should not be closed"))
            } else {
                None
            };
            set_status(&tasks, &id_spawn, TaskStatus::Running);
            run_task(
                id_spawn.clone(),
                command,
                cwd,
                env,
                cancel.clone(),
                timeout,
                max_tracked_tasks,
                tasks.clone(),
            )
            .await;
            notify_completion(
                &tasks,
                &completion_markers,
                &finished,
                &id_spawn,
                user_hooks.as_ref(),
            );
        });

        id
    }

    /// Adopt an already-running foreground command into the background.
    ///
    /// Used when a foreground `bash` command exceeds its timeout: rather than
    /// killing a process that may be mid-work (a slow build, a migration), the
    /// live child is handed here to keep running under a fresh, longer
    /// deadline. The process is *not* restarted, so its side effects never
    /// repeat.
    ///
    /// `child` must have had its `stdout`/`stderr` taken into `readers`;
    /// `seed_stdout`/`seed_stderr` carry the output already captured in the
    /// foreground so nothing is lost. Adopted tasks bypass the concurrency
    /// semaphore — the process is already running, so queuing it would only hide
    /// it, not stop it.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt_child(
        &self,
        command: impl Into<String>,
        cwd: PathBuf,
        child: Child,
        readers: AdoptedReaders,
        seed_stdout: String,
        seed_stderr: String,
        timeout: Option<Duration>,
    ) -> String {
        let command = command.into();
        let seq = self.counter.fetch_add(1, Ordering::SeqCst);
        let id = format!("task-{seq}");
        let cancel = CancellationToken::new();

        {
            let mut tasks = self.tasks.lock().unwrap();
            let mut task = BackgroundTask::new(
                id.clone(),
                command,
                cwd,
                TaskKind::Shell,
                cancel.clone(),
                self.task_env(),
                seq,
            );
            // Already running: skip the Queued state the semaphore path uses.
            task.status = TaskStatus::Running;
            tasks.insert(id.clone(), task);
        }

        let tasks = self.tasks.clone();
        let completion_markers = self.completion_markers.clone();
        let finished = self.finished.clone();
        let max_tracked_tasks = self.max_tracked_tasks;
        let user_hooks = self.user_hooks.lock().unwrap().clone();
        let id_spawn = id.clone();
        tokio::spawn(async move {
            run_adopted_task(
                id_spawn.clone(),
                child,
                readers,
                seed_stdout,
                seed_stderr,
                cancel,
                timeout,
                max_tracked_tasks,
                tasks.clone(),
            )
            .await;
            notify_completion(
                &tasks,
                &completion_markers,
                &finished,
                &id_spawn,
                user_hooks.as_ref(),
            );
        });

        id
    }

    /// List all known tasks.
    pub fn list(&self) -> Vec<String> {
        let tasks = self.tasks.lock().unwrap();
        tasks.values().map(|t| t.summary()).collect()
    }

    /// Return the current output for a task, if it exists.
    pub async fn output(&self, id: &str, offset: usize, limit: Option<usize>) -> Option<String> {
        let (stdout_path, stderr_path) = {
            let tasks = self.tasks.lock().unwrap();
            let task = tasks.get(id)?;
            (task.stdout_path.clone(), task.stderr_path.clone())
        };

        let stdout_text = tokio::fs::read_to_string(&stdout_path)
            .await
            .unwrap_or_default();
        let stderr_text = tokio::fs::read_to_string(&stderr_path)
            .await
            .unwrap_or_default();

        let stdout_lines: Vec<&str> = stdout_text.lines().collect();
        let stderr_lines: Vec<&str> = stderr_text.lines().collect();

        let mut lines = Vec::new();
        if !stdout_lines.is_empty() {
            lines.push("stdout:".to_string());
            for (i, line) in stdout_lines.iter().enumerate().skip(offset) {
                lines.push(format!("  {i}: {line}"));
            }
        }
        if !stderr_lines.is_empty() {
            lines.push("stderr:".to_string());
            let stderr_start = stdout_lines.len();
            for (i, line) in stderr_lines
                .iter()
                .enumerate()
                .skip(offset.saturating_sub(stderr_start))
            {
                lines.push(format!("  {}: {line}", stderr_start + i));
            }
        }

        if let Some(l) = limit {
            lines.truncate(l);
        }

        Some(lines.join("\n"))
    }

    /// Stop a running background task.
    pub fn stop(&self, id: &str) -> bool {
        let tasks = self.tasks.lock().unwrap();
        if let Some(task) = tasks.get(id) {
            task.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// Test helper: return the current status of a task.
    #[cfg(test)]
    pub fn status(&self, id: &str) -> Option<TaskStatus> {
        self.tasks.lock().unwrap().get(id).map(|t| t.status)
    }

    /// Test helper: return whether the captured stdout contains a substring.
    #[cfg(test)]
    pub fn stdout_contains(&self, id: &str, needle: &str) -> bool {
        self.tasks
            .lock()
            .unwrap()
            .get(id)
            .map(|t| t.stdout.iter().any(|l| l.contains(needle)))
            .unwrap_or(false)
    }

    /// Test helper: return whether the captured stderr contains a substring.
    #[cfg(test)]
    pub fn stderr_contains(&self, id: &str, needle: &str) -> bool {
        self.tasks
            .lock()
            .unwrap()
            .get(id)
            .map(|t| t.stderr.iter().any(|l| l.contains(needle)))
            .unwrap_or(false)
    }
}

impl Default for BackgroundManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    id: String,
    command: String,
    cwd: PathBuf,
    env: HashMap<String, String>,
    cancel: CancellationToken,
    timeout: Option<Duration>,
    max_tracked_tasks: usize,
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&command)
        .current_dir(&cwd)
        .kill_on_drop(true);
    crate::process_group::isolate(&mut cmd);
    // No input: Rusta's own is the terminal or the approval answers.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Safe defaults.
    cmd.env("NO_COLOR", "1");
    cmd.env("TERM", "dumb");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.envs(env);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            set_status(&tasks, &id, TaskStatus::Failed);
            append_stderr(
                &tasks,
                &id,
                format!("spawn failed: {e}"),
                &mut OutputFile::default(),
            );
            prune_terminal_tasks(&tasks, max_tracked_tasks);
            return;
        }
    };

    // Rusta going away takes the task and what it started along.
    let mut group = crate::process_group::KillOnDrop::new(&child);
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);

    let tasks_stdout = tasks.clone();
    let id_stdout = id.clone();
    let stdout_handle = tokio::spawn(async move {
        let mut lines = stdout_reader.lines();
        let mut out = OutputFile::default();
        while let Ok(Some(line)) = lines.next_line().await {
            append_stdout(&tasks_stdout, &id_stdout, line, &mut out);
        }
    });

    let tasks_stderr = tasks.clone();
    let id_stderr = id.clone();
    let stderr_handle = tokio::spawn(async move {
        let mut lines = stderr_reader.lines();
        let mut out = OutputFile::default();
        while let Ok(Some(line)) = lines.next_line().await {
            append_stderr(&tasks_stderr, &id_stderr, line, &mut out);
        }
    });

    let status = wait_or_stop(&mut child, &cancel, timeout, &tasks, &id).await;

    // The output is in before the task is marked finished, so whoever reads
    // the status (`task_output`, a waiter) reads all of it.
    drain_output(&tasks, &id, [stdout_handle, stderr_handle]).await;
    finalize_exit(&tasks, &id, status, &cancel).await;
    group.disarm();

    prune_terminal_tasks(&tasks, max_tracked_tasks);
}

/// Manage a foreground command that was adopted into the background after its
/// timeout fired. The child is already running; we seed its captured output,
/// keep draining its live readers, and wait for it under a fresh deadline.
#[allow(clippy::too_many_arguments)]
async fn run_adopted_task(
    id: String,
    mut child: Child,
    readers: AdoptedReaders,
    seed_stdout: String,
    seed_stderr: String,
    cancel: CancellationToken,
    timeout: Option<Duration>,
    max_tracked_tasks: usize,
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
) {
    // Replay what the command already printed in the foreground.
    let mut out = OutputFile::default();
    for line in seed_stdout.lines() {
        append_stdout(&tasks, &id, line.to_string(), &mut out);
    }
    for line in seed_stderr.lines() {
        append_stderr(&tasks, &id, line.to_string(), &mut out);
    }

    let mut group = crate::process_group::KillOnDrop::new(&child);
    let (mut stdout_lines, mut stderr_lines) = readers;
    let tasks_stdout = tasks.clone();
    let id_stdout = id.clone();
    let stdout_handle = tokio::spawn(async move {
        let mut out = OutputFile::default();
        while let Ok(Some(line)) = stdout_lines.next_line().await {
            append_stdout(&tasks_stdout, &id_stdout, line, &mut out);
        }
    });

    let tasks_stderr = tasks.clone();
    let id_stderr = id.clone();
    let stderr_handle = tokio::spawn(async move {
        let mut out = OutputFile::default();
        while let Ok(Some(line)) = stderr_lines.next_line().await {
            append_stderr(&tasks_stderr, &id_stderr, line, &mut out);
        }
    });

    let status = wait_or_stop(&mut child, &cancel, timeout, &tasks, &id).await;

    // As in `run_task`: the output is in before the task is marked finished.
    drain_output(&tasks, &id, [stdout_handle, stderr_handle]).await;
    finalize_exit(&tasks, &id, status, &cancel).await;
    group.disarm();

    prune_terminal_tasks(&tasks, max_tracked_tasks);
}

/// Wait for a task's output `readers` to reach the end of their pipes, once
/// its command has exited. A pipe that a grandchild still holds open never
/// ends, so the wait gives up after two seconds in which no output was
/// stored — but not while the readers are still storing a command's last
/// lines, however long that takes.
async fn drain_output(
    tasks: &Arc<Mutex<HashMap<String, BackgroundTask>>>,
    id: &str,
    readers: [JoinHandle<()>; 2],
) {
    let stored = || {
        tasks
            .lock()
            .unwrap()
            .get(id)
            .map_or(0, |task| task.stdout_bytes + task.stderr_bytes)
    };
    for mut reader in readers {
        loop {
            let before = stored();
            match tokio::time::timeout(Duration::from_secs(2), &mut reader).await {
                Ok(_) => break,
                Err(_) if stored() == before => break,
                Err(_) => {}
            }
        }
    }
}

/// Wait for a task's command to exit; when the task is stopped or runs out of
/// time first, stop the command and what it started.
async fn wait_or_stop(
    child: &mut Child,
    cancel: &CancellationToken,
    timeout: Option<Duration>,
    tasks: &Arc<Mutex<HashMap<String, BackgroundTask>>>,
    id: &str,
) -> std::io::Result<std::process::ExitStatus> {
    let deadline = async {
        match timeout {
            Some(t) => tokio::time::sleep(t).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        status = child.wait() => return status,
        () = cancel.cancelled() => {}
        () = deadline => append_stderr(
            tasks,
            id,
            "task timed out".to_string(),
            &mut OutputFile::default(),
        ),
    }
    crate::process_group::stop(child, crate::process_group::STOP_GRACE).await
}

/// Record a task's terminal status and exit code from the `wait()` result,
/// both at once: a reader never sees the one without the other.
async fn finalize_exit(
    tasks: &Arc<Mutex<HashMap<String, BackgroundTask>>>,
    id: &str,
    status: std::io::Result<std::process::ExitStatus>,
    cancel: &CancellationToken,
) {
    let (outcome, code) = match status {
        Ok(status) if cancel.is_cancelled() => (TaskStatus::Stopped, status.code()),
        Ok(status) if status.success() => (TaskStatus::Completed, status.code()),
        Ok(status) => (TaskStatus::Failed, status.code()),
        Err(e) => {
            append_stderr(
                tasks,
                id,
                format!("wait failed: {e}"),
                &mut OutputFile::default(),
            );
            (TaskStatus::Failed, None)
        }
    };
    let mut tasks = tasks.lock().unwrap();
    if let Some(task) = tasks.get_mut(id) {
        task.status = outcome;
        if code.is_some() {
            task.exit_code = code;
        }
    }
}

fn set_status(tasks: &Arc<Mutex<HashMap<String, BackgroundTask>>>, id: &str, status: TaskStatus) {
    let mut tasks = tasks.lock().unwrap();
    if let Some(task) = tasks.get_mut(id) {
        task.status = status;
    }
}

/// The output file a task's lines are appended to, kept open from one line to
/// the next. Each line is written straight to the file: a small append to a
/// local file, cheaper than handing it to the blocking pool, and in the file
/// at once for `read_task_output`. Opening the file for every line made a
/// command's output arrive long after the command ended.
#[derive(Default)]
struct OutputFile(Option<(PathBuf, std::fs::File)>);

impl OutputFile {
    /// Append `line` and a newline to the file at `path`, opening it unless it
    /// is the one open.
    fn append(&mut self, path: &Path, line: &str) {
        if self.0.as_ref().is_none_or(|(open, _)| open != path) {
            self.0 = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
                .map(|file| (path.to_path_buf(), file));
        }
        if let Some((_, file)) = &mut self.0 {
            use std::io::Write as _;
            let _ = file.write_all(format!("{line}\n").as_bytes());
        }
    }
}

fn append_line(
    tasks: &Arc<Mutex<HashMap<String, BackgroundTask>>>,
    id: &str,
    line: String,
    is_stdout: bool,
    out: &mut OutputFile,
) {
    let (path, cap, capped) = {
        let mut tasks = tasks.lock().unwrap();
        let Some(task) = tasks.get_mut(id) else {
            return;
        };
        if task.output_capped {
            return;
        }
        let line_bytes = line.len().saturating_add(1);
        let total = task.stdout_bytes.saturating_add(task.stderr_bytes);
        if total.saturating_add(line_bytes) > MAX_OUTPUT_BYTES {
            task.output_capped = true;
            if is_stdout {
                (task.stdout_path.clone(), task.memory_line_cap, true)
            } else {
                (task.stderr_path.clone(), task.memory_line_cap, true)
            }
        } else {
            if is_stdout {
                task.stdout_bytes += line_bytes;
                (task.stdout_path.clone(), task.memory_line_cap, false)
            } else {
                task.stderr_bytes += line_bytes;
                (task.stderr_path.clone(), task.memory_line_cap, false)
            }
        }
    };

    let line = if capped {
        OUTPUT_TRUNCATION_MARKER.to_string()
    } else {
        line
    };
    out.append(&path, &line);
    let mut tasks = tasks.lock().unwrap();
    if let Some(task) = tasks.get_mut(id) {
        let lines = if is_stdout {
            &mut task.stdout
        } else {
            &mut task.stderr
        };
        lines.push_back(line);
        while lines.len() > cap {
            lines.pop_front();
        }
    }
}

fn append_stdout(
    tasks: &Arc<Mutex<HashMap<String, BackgroundTask>>>,
    id: &str,
    line: String,
    out: &mut OutputFile,
) {
    append_line(tasks, id, line, true, out);
}

fn append_stderr(
    tasks: &Arc<Mutex<HashMap<String, BackgroundTask>>>,
    id: &str,
    line: String,
    out: &mut OutputFile,
) {
    append_line(tasks, id, line, false, out);
}

/// Post a finished task's completion notice for the agent that started it,
/// mark the task announced, and wake any [`BackgroundManager::wait_for`].
fn notify_completion(
    tasks: &Arc<Mutex<HashMap<String, BackgroundTask>>>,
    completion_markers: &Arc<Mutex<Vec<CompletionMarker>>>,
    finished: &Notify,
    id: &str,
    hooks: Option<&Arc<crate::user_hooks::UserHooks>>,
) {
    let notification = {
        // The notice is posted and the task marked under the tasks lock, so a
        // waiter that sees `notified` also finds the notice to take.
        let mut tasks = tasks.lock().unwrap();
        if let Some(task) = tasks.get_mut(id) {
            let status = task.status;
            tracing::info!("[background] task {id} finished with status {status}");
            let severity = match status {
                TaskStatus::Completed => "info",
                TaskStatus::Failed => "error",
                TaskStatus::Stopped => "warning",
                _ => "info",
            };
            let text = format!(
                r#"<notification category="task" source_kind="background_task" severity="{severity}" title="Background task {id} {status}"><body><![CDATA[{}]]></body></notification>"#,
                task.command
            );
            completion_markers.lock().unwrap().push(CompletionMarker {
                owner: task.owner,
                task_id: id.to_string(),
                text,
            });
            task.notified = true;
            status
        } else {
            return;
        }
    };
    // The Notification hook carries the same change a completion marker does.
    if let Some(hooks) = hooks {
        let notification_type = match notification {
            TaskStatus::Completed => "task.completed",
            TaskStatus::Failed => "task.failed",
            TaskStatus::Stopped => "task.stopped",
            _ => return,
        };
        let message = {
            let tasks = tasks.lock().unwrap();
            tasks.get(id).map(|task| task.command.clone())
        };
        let mut fields = serde_json::Map::new();
        fields.insert(
            "notification_type".to_string(),
            serde_json::Value::String(notification_type.to_string()),
        );
        fields.insert(
            "task_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        if let Some(message) = message {
            fields.insert("message".to_string(), serde_json::Value::String(message));
        }
        hooks.fire_spawned(
            crate::user_hooks::UserHookEvent::Notification,
            notification_type.to_string(),
            fields,
        );
    }
    finished.notify_waiters();
}

fn prune_terminal_tasks(
    tasks: &Arc<Mutex<HashMap<String, BackgroundTask>>>,
    max_tracked_tasks: usize,
) {
    if max_tracked_tasks == 0 {
        return;
    }

    let ids_and_dirs: Vec<(String, PathBuf)> = {
        let tasks = tasks.lock().unwrap();
        let mut terminal: Vec<&BackgroundTask> =
            tasks.values().filter(|t| t.status.is_terminal()).collect();
        // The budget is over *terminal* tasks, which is what the cap documents
        // and the only thing eviction can act on — a running task is not
        // evictable, so counting it toward the budget only forces the eviction
        // of a finished task that was still under the cap. Sizing `to_remove`
        // from `tasks.len()` did exactly that: with four tasks spawned and one
        // finished, it computed "remove 2", found one terminal task, and evicted
        // it — so which tasks survived depended on the order they *completed*
        // rather than the order they were created.
        if terminal.len() <= max_tracked_tasks {
            return;
        }
        // `created_at` is not a total order on a coarse clock; `seq` breaks ties
        // by spawn order so eviction never depends on `HashMap` iteration order.
        terminal.sort_by_key(|t| (t.created_at, t.seq));
        let to_remove = terminal.len().saturating_sub(max_tracked_tasks);
        terminal
            .into_iter()
            .take(to_remove)
            .map(|t| {
                let dir = t
                    .stdout_path
                    .parent()
                    .unwrap_or(Path::new(""))
                    .to_path_buf();
                (t.id.clone(), dir)
            })
            .collect()
    };

    {
        let mut tasks = tasks.lock().unwrap();
        for (id, _) in &ids_and_dirs {
            tasks.remove(id);
        }
    }

    for (_, dir) in ids_and_dirs {
        if !dir.as_os_str().is_empty() {
            tokio::spawn(async move {
                let _ = tokio::fs::remove_dir_all(dir).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_manager() -> (BackgroundManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let manager = BackgroundManager::with_data_dir(dir.path().to_path_buf());
        (manager, dir)
    }

    /// Wait for `done`, failing the test after 30 s. A fixed budget of a few
    /// seconds failed on a loaded machine, where even `echo` can take that
    /// long to be scheduled.
    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn settled(manager: &BackgroundManager, id: &str) -> bool {
        manager.status(id).is_some_and(TaskStatus::is_terminal)
    }

    #[tokio::test]
    async fn test_background_task_runs_and_captures_output() {
        let (manager, _dir) = temp_manager();
        let id = manager.spawn(
            "echo hello && echo world >&2",
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        );

        wait_until("the task to finish", || settled(&manager, &id)).await;

        assert!(matches!(manager.status(&id), Some(TaskStatus::Completed)));
        assert!(manager.stdout_contains(&id, "hello"));
        assert!(manager.stderr_contains(&id, "world"));

        let output = manager.output(&id, 0, None).await.unwrap();
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
    }

    /// A sub-agent that never settles must be cancelled at its deadline rather
    /// than running forever. A headless turn waits for agent tasks to finish, so
    /// without this an unattended `-p` run hangs indefinitely.
    #[tokio::test]
    async fn agent_that_never_settles_is_cancelled_at_its_deadline() {
        let (manager, _dir) = temp_manager();
        let manager = manager.with_agent_timeout(Some(Duration::from_millis(150)));

        let id = manager.spawn_agent("wedged", PathBuf::from("/work/app"), |cancel| async move {
            // Ignore cancellation the way a wedged agent would: just never finish.
            let _ = cancel;
            std::future::pending::<()>().await;
            Ok("unreachable".to_string())
        });

        wait_until("the deadline", || settled(&manager, &id)).await;

        assert_eq!(
            manager.status(&id),
            Some(TaskStatus::Failed),
            "a wedged sub-agent must terminate at its deadline"
        );
        assert!(
            manager.stderr_contains(&id, "time limit"),
            "the timeout must be reported so the model learns why it stopped"
        );
    }

    /// The deadline is opt-out, and the default is set.
    #[tokio::test]
    async fn agent_timeout_defaults_on_and_can_be_disabled() {
        let (manager, _dir) = temp_manager();
        assert_eq!(manager.agent_timeout(), Some(DEFAULT_AGENT_TIMEOUT));
        assert_eq!(manager.with_agent_timeout(None).agent_timeout(), None);
    }

    #[tokio::test]
    async fn test_spawn_agent_runs_and_records_output() {
        let (manager, _dir) = temp_manager();
        let id = manager.spawn_agent("test-agent", PathBuf::from("/work/app"), |_cancel| async {
            Ok("agent result line 1\nagent result line 2".to_string())
        });

        wait_until("the agent to finish", || settled(&manager, &id)).await;

        assert!(matches!(manager.status(&id), Some(TaskStatus::Completed)));
        assert!(manager.stdout_contains(&id, "agent result line 1"));
        assert!(manager.stdout_contains(&id, "agent result line 2"));
        // The sub-agent's directory, not Rusta's own.
        let summary = manager.tasks.lock().unwrap()[&id].summary();
        assert!(summary.ends_with("(cwd: /work/app)"), "{summary}");
    }

    #[tokio::test]
    async fn test_background_task_can_be_stopped() {
        let (manager, _dir) = temp_manager();
        let id = manager.spawn(
            "sleep 30",
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        );

        // Give the task a moment to start.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(manager.stop(&id));

        wait_until("the task to stop", || settled(&manager, &id)).await;

        assert!(matches!(manager.status(&id), Some(TaskStatus::Stopped)));
    }

    #[tokio::test]
    async fn test_output_persists_to_disk() {
        let (manager, dir) = temp_manager();
        let id = manager.spawn(
            "echo persisted",
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        );

        wait_until("the task to finish", || settled(&manager, &id)).await;

        // Drop manager and read sidecars directly from disk.
        let stdout_path = dir.path().join("tasks").join(&id).join("stdout.txt");
        assert!(stdout_path.exists());
        let content = tokio::fs::read_to_string(&stdout_path).await.unwrap();
        assert!(content.contains("persisted"));
    }

    #[tokio::test]
    async fn test_max_running_tasks_queues() {
        let (manager, _dir) = temp_manager();
        let manager = manager.with_max_running_tasks(1);
        // First task holds the single permit.
        let id1 = manager.spawn(
            "sleep 5",
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        );
        wait_until("the first task to start", || {
            manager.status(&id1) == Some(TaskStatus::Running)
        })
        .await;

        let id2 = manager.spawn(
            "echo queued",
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(manager.status(&id2), Some(TaskStatus::Queued));

        manager.stop(&id1);
        wait_until("the queued task to run", || settled(&manager, &id2)).await;
        assert_eq!(manager.status(&id2), Some(TaskStatus::Completed));
    }

    /// Stopping a task stops what its command started too.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stopped_task_takes_what_it_started_along() {
        use crate::process_group::testing::{can_see_processes, gone, pid_in};
        if !can_see_processes() {
            return;
        }
        let (manager, dir) = temp_manager();
        let file = dir.path().join("pid");
        let id = manager.spawn(
            format!("sleep 300 & echo $! > {}; wait", file.display()),
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        );
        let grandchild = pid_in(&file).await;

        assert!(manager.stop(&id));
        wait_until("the task to stop", || settled(&manager, &id)).await;

        assert_eq!(manager.status(&id), Some(TaskStatus::Stopped));
        assert!(
            gone(grandchild).await,
            "the sleep outlived its stopped task"
        );
    }

    /// A task is marked finished only once its output is in: the last of
    /// many lines a command prints just before it exits is there when the
    /// status says the task completed, with its exit code.
    #[tokio::test]
    async fn a_finished_task_has_all_its_output() {
        let (manager, _dir) = temp_manager();
        let id = manager.spawn(
            "seq 1 20000",
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        );

        wait_until("the task to finish", || settled(&manager, &id)).await;

        assert_eq!(manager.status(&id), Some(TaskStatus::Completed));
        assert!(manager.stdout_contains(&id, "20000"));
        assert_eq!(manager.tasks.lock().unwrap()[&id].exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_completion_marker_is_drained() {
        let (manager, _dir) = temp_manager();
        let id = manager.spawn(
            "echo done",
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        );

        // The notice, not the status: a task is marked completed before its
        // notice is posted.
        wait_until("the completion notice", || {
            manager.pending_completion_count() == 1
        })
        .await;

        assert_eq!(manager.pending_completion_count(), 1);
        let markers = manager.drain_completion_markers();
        assert_eq!(markers.len(), 1);
        assert!(markers[0].contains(&format!("task {id} completed")));
        assert!(markers[0].contains("echo done"));
        assert!(manager.drain_completion_markers().is_empty());
    }

    #[tokio::test]
    async fn test_memory_ring_buffer_caps() {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            BackgroundManager::with_data_dir(dir.path().to_path_buf()).with_memory_line_cap(2);
        let id = manager.spawn(
            "echo a && echo b && echo c",
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        );

        wait_until("the task to finish", || settled(&manager, &id)).await;

        // In-memory ring buffer only keeps the last two lines, but disk has all three.
        assert!(!manager.stdout_contains(&id, "a"));
        assert!(manager.stdout_contains(&id, "b"));
        assert!(manager.stdout_contains(&id, "c"));

        let output = manager.output(&id, 0, None).await.unwrap();
        assert!(output.contains("a"));
        assert!(output.contains("b"));
        assert!(output.contains("c"));
    }

    #[tokio::test]
    async fn test_terminal_tasks_are_cleaned_up_when_limit_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            BackgroundManager::with_data_dir(dir.path().to_path_buf()).with_max_tracked_tasks(2);
        let ids: Vec<String> = (0..4)
            .map(|i| {
                manager.spawn(
                    format!("echo task{i}"),
                    std::env::current_dir().unwrap(),
                    HashMap::new(),
                    None,
                )
            })
            .collect();

        // Wait for each task to *settle* — terminal, or already evicted. Waiting
        // only for `Completed` spun the full budget on tasks the cleanup had
        // already removed, whose status is `None` and never becomes `Completed`.
        for id in &ids {
            wait_until("the task to settle", || {
                manager.status(id).is_none_or(TaskStatus::is_terminal)
            })
            .await;
        }

        // Only the two most recent terminal tasks should remain.
        assert!(manager.status(&ids[2]).is_some());
        assert!(manager.status(&ids[3]).is_some());
        assert!(manager.status(&ids[0]).is_none());
        assert!(manager.status(&ids[1]).is_none());

        // Their on-disk task directories are removed too, by a background
        // deletion, so wait for it rather than race it: under load it can still
        // be pending when the tasks have already left the table.
        for id in &ids[..2] {
            let task_dir = dir.path().join("tasks").join(id);
            wait_until(&format!("{} to be removed", task_dir.display()), || {
                !task_dir.exists()
            })
            .await;
        }
    }

    /// **The cap is a budget over terminal tasks, not over all of them.**
    ///
    /// Sizing the eviction from the total task count would make a
    /// still-running task consume budget it could never release: with three
    /// spawned, a cap of two, and one finished, it would compute "remove 1",
    /// find the single terminal task, and evict it — despite it being the only
    /// finished task and well under the cap. Which tasks survived would then
    /// depend on the order they *completed*, which under load is not the order
    /// they were spawned.
    #[tokio::test]
    async fn a_finished_task_is_kept_while_others_are_still_running() {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            BackgroundManager::with_data_dir(dir.path().to_path_buf()).with_max_tracked_tasks(2);
        let cwd = std::env::current_dir().unwrap();

        let slow = manager.spawn("sleep 30".to_string(), cwd.clone(), HashMap::new(), None);
        let slower = manager.spawn("sleep 30".to_string(), cwd.clone(), HashMap::new(), None);
        let quick = manager.spawn("echo done".to_string(), cwd, HashMap::new(), None);

        for _ in 0..100 {
            if manager.status(&quick).map(TaskStatus::is_terminal) == Some(true) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            manager.status(&quick),
            Some(TaskStatus::Completed),
            "the only finished task is under the cap of 2 and must be retained"
        );
        assert!(
            manager.status(&slow).is_some(),
            "a running task is not evictable"
        );
        assert!(manager.status(&slower).is_some());

        manager.stop(&slow);
        manager.stop(&slower);
    }
}
