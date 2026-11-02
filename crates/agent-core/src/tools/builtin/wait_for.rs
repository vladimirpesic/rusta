//! `wait_for` — wait inside the turn for a background task to finish.
//!
//! The model blocks on a sub-agent or a background command whose result its
//! next step needs, instead of ending the turn or polling `task_output`. One
//! description serves the session's own agent and its sub-agents alike;
//! sub-agent guidance is delivered at run time (a sub-agent's timeout advice
//! and the repeat warning's advice variants).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};
use crate::background::{FinishedTask, WaitOutcome};

pub const WAIT_FOR_TOOL_NAME: &str = "wait_for";

/// The longest single wait, in seconds. A longer wait is a second call, which
/// also lets the model re-evaluate in between.
pub const WAIT_FOR_MAX_TIMEOUT_S: u64 = 90;

/// The tool's description, shared by every agent. Rusta's
/// `ask_user_question` is not a background task, so it is not among the tasks
/// it waits for.
const DESCRIPTION: &str = "Wait for background tasks to finish without ending the current turn.\n\
     \n\
     Only call this tool when you really have no other work to do and your next step cannot \
     proceed without the result of a running background task (a sub-agent or a background bash \
     command). The call suspends inside the current turn until the task finishes or the timeout \
     elapses, then returns the outcome so you can keep working in the same turn. While waiting, \
     no LLM requests are made, but the user is kept waiting too.\n\
     \n\
     When background tasks finish, they notify you automatically, so you do not need to busily \
     wait for them.\n\
     \n\
     Guidelines:\n\
     \n\
     - Before calling wait_for, think about what else you can do meanwhile: another part of the \
     task, verifying earlier work, or ending your turn with a progress update. If there is \
     anything, do that instead.\n\
     - Do not call wait_for right after dispatching work whose result you do not need yet.\n\
     - `timeout` is required, in seconds, capped at 90. Pick it from how long you expect the \
     task to take, not the maximum.\n\
     - A timeout is not an error: the result lists the tasks that are still running. Prefer \
     moving on to other work over calling wait_for again; repeated waits keep the user waiting.\n\
     - Without `task_id`, the wait ends as soon as any background task that was running at call \
     time finishes. Tasks started during the wait are not covered by it; their completion \
     arrives via the usual automatic notification.\n\
     - With `task_id`, the wait ends when that task finishes. An unknown `task_id` is an error; \
     a task that has already finished returns immediately.\n\
     - When no background tasks are running, wait_for returns immediately without waiting.\n\
     - When the wait ends because a task finished, the result also lists other tasks that \
     finished during the wait window, so failures surface with context.\n\
     - Waiting has no side effects on the waited tasks: wait_for never stops a task, and \
     interrupting the wait (for example, a new user message) leaves every task running.\n\
     - A finished task's result is delivered exactly once: tasks reported by wait_for do not \
     also produce an automatic completion notification.\n\
     - You can only wait for background tasks started by this agent; task IDs belonging to \
     other agents are unknown here.";

/// The repeat warning's advice for a sub-agent: ending its turn is its final
/// hand-off, so it must not wait on tasks it does not need, but must not hand
/// off without a result it truly cannot finish without.
const SUBAGENT_REPEAT_ADVICE: &str = "Stop waiting by reflex: repeated waits burn time your \
     caller is waiting on. Before calling wait_for again, do every part of your task that does \
     not depend on the running background task. Wait again only for a result you truly cannot \
     finish without — ending your turn is your final hand-off, so do not hand off without it, \
     but never wait for tasks you do not need.";

/// The repeat warning's advice while a goal is active (`repeatWaitAdvice`):
/// repeated waits stall the goal; there is almost always other goal work.
const GOAL_REPEAT_ADVICE: &str = "Stop waiting by reflex: repeated waits stall the goal. Before \
     calling wait_for again, do every piece of remaining goal work that does not depend on the \
     running background task — there is almost always some. Wait again only if nothing else can \
     proceed until it finishes; even then, wait_for beats polling with Bash sleep or ending the \
     turn only to be continued again.";

/// The repeat warning's advice for the session's own agent with no active
/// goal (`repeatWaitAdvice`).
const MAIN_REPEAT_ADVICE: &str = "Stop calling wait_for. Repeated waiting wastes the user's \
     time and is almost never the right move. Do not call it again in this turn unless the user \
     explicitly asked you to wait. Do something useful now — another part of the task, or \
     verifying earlier work — or end your turn with a progress update. Finished background tasks \
     notify you automatically, so you will not miss the result.";

/// What a sub-agent is told when its wait times out. With one shared
/// description, this carries the sub-agent guidance the description does
/// not.
const SUBAGENT_TIMEOUT_ADVICE: &str = "If you need the result of a task still running, call \
     wait_for again: ending your turn is your hand-off, and a completion notice after it \
     reaches no one.";

/// One agent's `wait_for` calls in its current turn: how many, and how long
/// they waited in total. Reset whenever the turn id changes (see
/// [`ExecutableToolContext::turn_id`]).
///
/// Kept per agent, in [`ExecutableToolContext::wait_tally`], not in the tool:
/// a fork shares its caller's tool instances, but its waits are its own.
#[derive(Debug, Default)]
pub struct WaitTally {
    turn: u64,
    calls: u32,
    waited_ms: u64,
}

impl WaitTally {
    /// Count one call in `turn`, resetting the tally at a new turn.
    fn count_call(&mut self, turn: u64) {
        if self.turn != turn {
            *self = Self {
                turn,
                ..Self::default()
            };
        }
        self.calls += 1;
    }

    /// Add `waited_ms` to the tally for `turn`, if it is still the current
    /// one (a new turn while this call waited discards it).
    fn add_waited(&mut self, turn: u64, waited_ms: u64) {
        if self.turn == turn {
            self.waited_ms += waited_ms;
        }
    }
}

pub struct WaitForTool {
    tool: Tool,
}

impl WaitForTool {
    /// The tool as every agent gets it. Sub-agent and goal guidance is not in
    /// the description; it arrives at run time, in a sub-agent's timeout
    /// advice and in the repeat warning.
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                WAIT_FOR_TOOL_NAME,
                DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "timeout": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": WAIT_FOR_MAX_TIMEOUT_S,
                            "description": format!(
                                "Maximum time to wait, in seconds (1-{WAIT_FOR_MAX_TIMEOUT_S}). \
                                 Pick it from how long you expect the task to take, not the \
                                 maximum. A timeout is not an error: the tool returns the tasks \
                                 that are still running."
                            )
                        },
                        "task_id": {
                            "type": "string",
                            "description": "The background task ID to wait for. When omitted, \
                                           the wait ends as soon as any background task that was \
                                           running at call time finishes."
                        }
                    },
                    "required": ["timeout"],
                    "additionalProperties": false
                }),
            ),
        }
    }
}

impl Default for WaitForTool {
    fn default() -> Self {
        Self::new()
    }
}

/// The `[wait_warning]`: from the second `wait_for` call in a turn,
/// the result ends with the call count, the total time already waited, and
/// one of three advice texts — sub-agent, active goal, or main agent, in that
/// order of precedence.
fn repeat_warning(ctx: &ExecutableToolContext, turn: u64) -> Option<String> {
    let advice = if ctx.depth > 0 {
        SUBAGENT_REPEAT_ADVICE
    } else if ctx.goal_manager.lock().unwrap().active_goal().is_some() {
        GOAL_REPEAT_ADVICE
    } else {
        MAIN_REPEAT_ADVICE
    };
    let tally = ctx.wait_tally.lock().unwrap();
    if tally.turn != turn || tally.calls < 2 {
        return None;
    }
    let waited = format_wait_seconds(tally.waited_ms.saturating_add(500) / 1000);
    Some(format!(
        "[wait_warning]\nThis is wait_for call {} in this turn, and you have already spent \
         {waited} waiting. {advice}",
        tally.calls
    ))
}

/// `90s`, `2m`, `2m 05s`, `1h`, `1h 05m`.
fn format_wait_seconds(total_seconds: u64) -> String {
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes < 60 {
        return if seconds == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {seconds:02}s")
        };
    }
    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if remaining_minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {remaining_minutes:02}m")
    }
}

/// The finished tasks, then what is still running, then each finished
/// task's output — which is what the model waited for. A sub-agent's timeout
/// comes with [`SUBAGENT_TIMEOUT_ADVICE`].
async fn render(outcome: &WaitOutcome, timeout_s: u64, ctx: &ExecutableToolContext) -> String {
    if outcome.finished.is_empty() && outcome.running.is_empty() {
        return "No background tasks are running.".to_string();
    }
    let mut lines = Vec::new();
    if !outcome.finished.is_empty() {
        let finished: Vec<String> = outcome.finished.iter().map(describe).collect();
        lines.push(format!("Finished: {}", finished.join(", ")));
    }
    if !outcome.running.is_empty() {
        lines.push(format!("Still running: {}", outcome.running.join(", ")));
    }
    if outcome.timed_out {
        lines.push(format!("Timed out after {timeout_s} s."));
        if ctx.depth > 0 {
            lines.push(SUBAGENT_TIMEOUT_ADVICE.to_string());
        }
    }
    if outcome.interrupted {
        // The interrupted-by-message result: the steered message itself is a
        // user message in the context by the time this result renders.
        lines.push(
            "[interrupted] New input ended this wait early. Read the new input before deciding \
             what to do next. Background tasks have not been stopped; completion still arrives \
             via automatic notification."
                .to_string(),
        );
    }
    for task in &outcome.finished {
        let output = ctx
            .background
            .output(&task.id, 0, None)
            .await
            .filter(|out| !out.trim().is_empty())
            .unwrap_or_else(|| "(no output)".to_string());
        lines.push(String::new());
        lines.push(format!("[{}] {}", task.id, task.command));
        lines.push(output);
    }
    lines.join("\n")
}

/// `task-3 (failed, exit 2)`.
fn describe(task: &FinishedTask) -> String {
    match task.exit_code {
        Some(code) if code != 0 => format!("{} ({}, exit {code})", task.id, task.status),
        _ => format!("{} ({})", task.id, task.status),
    }
}

#[async_trait]
impl ExecutableTool for WaitForTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
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
        let timeout_s = match input.get("timeout").and_then(serde_json::Value::as_u64) {
            Some(t) if (1..=WAIT_FOR_MAX_TIMEOUT_S).contains(&t) => t,
            _ => {
                return ExecutableToolResult::error(format!(
                    "timeout is required: a whole number of seconds from 1 to \
                     {WAIT_FOR_MAX_TIMEOUT_S}."
                ));
            }
        };
        let task_id = match input.get("task_id") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(id)) if !id.trim().is_empty() => Some(id.trim()),
            Some(_) => {
                return ExecutableToolResult::error("task_id must be a task id string.");
            }
        };
        let turn = ctx.turn_id.load(Ordering::SeqCst);
        ctx.wait_tally.lock().unwrap().count_call(turn);
        let started = Instant::now();
        let outcome = ctx
            .background
            .wait_for(task_id, Duration::from_secs(timeout_s), &ctx.steer)
            .await;
        ctx.wait_tally
            .lock()
            .unwrap()
            .add_waited(turn, started.elapsed().as_millis() as u64);
        match outcome {
            Ok(outcome) => {
                let mut text = render(&outcome, timeout_s, ctx).await;
                if let Some(warning) = repeat_warning(ctx, turn) {
                    text.push_str("\n\n");
                    text.push_str(&warning);
                }
                ExecutableToolResult::text(text)
            }
            Err(e) => {
                let mut text = e.to_string();
                if let Some(warning) = repeat_warning(ctx, turn) {
                    text.push_str("\n\n");
                    text.push_str(&warning);
                }
                ExecutableToolResult::error(text)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::background::BackgroundManager;

    fn ctx_with(background: Arc<BackgroundManager>) -> ExecutableToolContext {
        ExecutableToolContext {
            cron: Arc::new(crate::cron::CronManager::new()),
            background,
            ..Default::default()
        }
    }

    fn manager() -> (Arc<BackgroundManager>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(BackgroundManager::with_data_dir(dir.path().to_path_buf()));
        (manager, dir)
    }

    fn spawn(manager: &BackgroundManager, command: &str) -> String {
        manager.spawn(
            command,
            std::env::current_dir().unwrap(),
            HashMap::new(),
            None,
        )
    }

    async fn run(ctx: &ExecutableToolContext, input: serde_json::Value) -> ExecutableToolResult {
        WaitForTool::new().execute(input, ctx, "tc").await
    }

    #[tokio::test]
    async fn waiting_returns_the_finished_tasks_output() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let id = spawn(&manager, "sleep 0.2 && echo finished-work");

        let result = run(&ctx, json!({ "timeout": 30, "task_id": id })).await;

        let text = result.text_content();
        assert!(!result.is_error, "{text}");
        assert!(
            text.contains(&format!("Finished: {id} (completed)")),
            "{text}"
        );
        assert!(text.contains("finished-work"), "{text}");
        assert_eq!(
            manager.pending_completion_count(),
            0,
            "a task reported by wait_for is not notified again"
        );
    }

    /// Without a task id, the first of the running tasks to finish ends the
    /// wait; the others are listed as still running.
    #[tokio::test]
    async fn waiting_for_any_task_ends_at_the_first_finish() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let quick = spawn(&manager, "sleep 0.2");
        let slow = spawn(&manager, "sleep 30");

        let result = run(&ctx, json!({ "timeout": 30 })).await;

        let text = result.text_content();
        assert!(
            text.contains(&format!("Finished: {quick} (completed)")),
            "{text}"
        );
        assert!(text.contains(&format!("Still running: {slow}")), "{text}");
        assert!(!text.contains("Timed out"), "{text}");
        manager.stop(&slow);
    }

    #[tokio::test]
    async fn a_timeout_is_not_an_error() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let slow = spawn(&manager, "sleep 30");

        let result = run(&ctx, json!({ "timeout": 1 })).await;

        let text = result.text_content();
        assert!(!result.is_error, "{text}");
        assert!(text.contains(&format!("Still running: {slow}")), "{text}");
        assert!(text.contains("Timed out after 1 s."), "{text}");
        assert!(
            manager.list().iter().any(|t| t.contains("[running]")),
            "waiting never stops a task"
        );
        manager.stop(&slow);
    }

    /// Every agent gets the same description; a sub-agent learns that ending
    /// its turn is its hand-off from its timeout results, not the description.
    #[tokio::test]
    async fn a_subagent_is_told_its_turn_is_the_hand_off_on_a_timeout() {
        // A sub-agent (depth > 0) gets the hand-off advice after a timeout.
        {
            let (manager, _dir) = manager();
            let mut ctx = ctx_with(manager.clone());
            ctx.depth = 1;
            let slow = spawn(&manager, "sleep 30");

            let text = WaitForTool::new()
                .execute(json!({ "timeout": 1, "task_id": slow }), &ctx, "tc")
                .await
                .text_content();
            assert!(text.contains("Timed out after 1 s."), "{text}");
            assert!(text.ends_with(SUBAGENT_TIMEOUT_ADVICE), "{text}");
            manager.stop(&slow);
        }

        // The session's own agent gets a plain timeout.
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let slow = spawn(&manager, "sleep 30");
        let text = WaitForTool::new()
            .execute(json!({ "timeout": 1, "task_id": slow }), &ctx, "tc")
            .await
            .text_content();
        assert!(
            text.ends_with("Timed out after 1 s."),
            "the main agent's timeout carries no sub-agent advice: {text}"
        );
        manager.stop(&slow);
    }

    #[tokio::test]
    async fn nothing_running_returns_at_once() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager);
        let result = run(&ctx, json!({ "timeout": 90 })).await;
        assert!(!result.is_error);
        assert_eq!(result.text_content(), "No background tasks are running.");
    }

    #[tokio::test]
    async fn bad_arguments_and_unknown_ids_are_errors() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager);
        for input in [json!({}), json!({ "timeout": 0 }), json!({ "timeout": 91 })] {
            let result = run(&ctx, input.clone()).await;
            assert!(result.is_error, "{input}");
            assert!(result.text_content().contains("timeout is required"));
        }
        let result = run(&ctx, json!({ "timeout": 5, "task_id": "task-999" })).await;
        assert!(result.is_error);
        assert!(result.text_content().contains("Task not found: task-999"));
    }

    /// The schema caps a single wait at 90 seconds.
    #[test]
    fn the_schema_caps_timeout_at_90_seconds() {
        let tool = WaitForTool::new();
        let parameters = &tool.tool_definition().parameters;
        assert_eq!(
            parameters["properties"]["timeout"]["maximum"],
            serde_json::json!(90)
        );
        assert_eq!(
            parameters["properties"]["timeout"]["minimum"],
            serde_json::json!(1)
        );
        assert!(
            parameters["required"]
                .as_array()
                .expect("required is a list")
                .contains(&serde_json::json!("timeout"))
        );
    }

    /// The first `wait_for` call of a turn carries no repeat warning.
    #[tokio::test]
    async fn the_first_call_of_a_turn_has_no_repeat_warning() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let slow = spawn(&manager, "sleep 30");

        let text = WaitForTool::new()
            .execute(json!({ "timeout": 1, "task_id": slow }), &ctx, "tc")
            .await
            .text_content();

        assert!(text.contains("Timed out after 1 s."), "{text}");
        assert!(!text.contains("[wait_warning]"), "{text}");
        manager.stop(&slow);
    }

    /// From the second call in a turn, the result ends with the
    /// `[wait_warning]`: the call count and the total time already waited.
    #[tokio::test]
    async fn the_second_call_warns_with_the_total_waited() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let slow = spawn(&manager, "sleep 30");
        let tool = WaitForTool::new();
        let input = json!({ "timeout": 1, "task_id": slow });

        let first = tool.execute(input.clone(), &ctx, "tc").await.text_content();
        assert!(!first.contains("[wait_warning]"), "{first}");

        let second = tool.execute(input, &ctx, "tc").await.text_content();
        // The waited total is whatever the first call actually waited (about a
        // second; scheduling can overrun), reported in `1s`-style units.
        assert!(
            second.contains(
                "[wait_warning]\nThis is wait_for call 2 in this turn, and you have already \
                 spent "
            ) && second.contains("s waiting."),
            "{second}"
        );
        assert!(
            second.contains(MAIN_REPEAT_ADVICE),
            "a main agent with no active goal gets the main advice: {second}"
        );
        manager.stop(&slow);
    }

    /// A new turn starts a fresh tally: the first call of the next turn
    /// carries no warning again.
    #[tokio::test]
    async fn a_new_turn_resets_the_tally() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let slow = spawn(&manager, "sleep 30");
        let tool = WaitForTool::new();
        let input = json!({ "timeout": 1, "task_id": slow });

        let first = tool.execute(input.clone(), &ctx, "tc").await.text_content();
        assert!(!first.contains("[wait_warning]"), "{first}");
        let second = tool.execute(input.clone(), &ctx, "tc").await.text_content();
        assert!(second.contains("[wait_warning]"), "{second}");

        // The turn loop bumps this between turns; the test plays that part.
        ctx.turn_id.fetch_add(1, Ordering::SeqCst);
        let third = tool.execute(input, &ctx, "tc").await.text_content();
        assert!(third.contains("Timed out after 1 s."), "{third}");
        assert!(!third.contains("[wait_warning]"), "{third}");
        manager.stop(&slow);
    }

    /// The repeat warning's advice has three variants: sub-agent first, then an
    /// active goal, then the main agent. Calls with nothing running return at
    /// once, so the tally alone decides.
    #[tokio::test]
    async fn the_repeat_warning_picks_the_advice_variant() {
        async fn warning_text(ctx: &ExecutableToolContext) -> String {
            let tool = WaitForTool::new();
            let _ = tool.execute(json!({ "timeout": 1 }), ctx, "tc").await;
            tool.execute(json!({ "timeout": 1 }), ctx, "tc")
                .await
                .text_content()
        }

        // A sub-agent (depth > 0) gets the sub-agent advice even with an
        // active goal: being a sub-agent is checked first.
        let sub_ctx = ExecutableToolContext {
            depth: 1,
            ..Default::default()
        };
        sub_ctx
            .goal_manager
            .lock()
            .unwrap()
            .create("objective", None, false)
            .unwrap();
        let text = warning_text(&sub_ctx).await;
        assert!(text.contains(SUBAGENT_REPEAT_ADVICE), "{text}");

        // The session's own agent with an active goal gets the goal advice.
        let goal_ctx = ExecutableToolContext::default();
        goal_ctx
            .goal_manager
            .lock()
            .unwrap()
            .create("objective", None, false)
            .unwrap();
        let text = warning_text(&goal_ctx).await;
        assert!(text.contains(GOAL_REPEAT_ADVICE), "{text}");

        // No goal: the main-agent advice.
        let text = warning_text(&ExecutableToolContext::default()).await;
        assert!(text.contains(MAIN_REPEAT_ADVICE), "{text}");
    }

    /// A sub-agent's handle neither sees its parent's tasks for waiting nor
    /// drains their notices.
    #[tokio::test]
    async fn an_agent_waits_only_on_its_own_tasks() {
        let (parent, _dir) = manager();
        let child = Arc::new(parent.scoped());
        let parents_task = spawn(&parent, "echo parent");

        let result = run(
            &ctx_with(child.clone()),
            json!({ "timeout": 5, "task_id": parents_task }),
        )
        .await;
        assert!(result.is_error, "{}", result.text_content());

        let result = run(&ctx_with(parent.clone()), json!({ "timeout": 30 })).await;
        assert!(!result.is_error);
        assert!(child.drain_completion_markers().is_empty());
    }

    /// A task that finished and was already announced returns at once, and
    /// is reported again only here.
    #[tokio::test]
    async fn an_already_finished_task_returns_at_once() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let id = spawn(&manager, "echo early");
        for _ in 0..100 {
            if manager.pending_completion_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let delivered = manager.drain_completion_markers();
        assert_eq!(delivered.len(), 1);

        let started = std::time::Instant::now();
        let result = run(&ctx, json!({ "timeout": 60, "task_id": id })).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            result.text_content().contains("early"),
            "{}",
            result.text_content()
        );
    }

    /// A message steered into the running turn ends the wait early: the result
    /// is the interrupted-by-message one, the task is not stopped, and the wait
    /// returns at once instead of holding out for the timeout.
    #[tokio::test]
    async fn a_steered_message_ends_the_wait_early() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let slow = spawn(&manager, "sleep 30");
        let steer = ctx.steer.clone();

        let wait = tokio::spawn({
            let ctx = ctx.clone();
            let slow = slow.clone();
            async move {
                WaitForTool::new()
                    .execute(json!({ "timeout": 90, "task_id": slow }), &ctx, "tc")
                    .await
            }
        });
        // Give the wait a moment to start, then steer as a Ctrl-S would.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let steered_at = std::time::Instant::now();
        steer.steer("actually, do this instead");
        let result = wait.await.unwrap();

        assert!(
            steered_at.elapsed() < Duration::from_secs(5),
            "the wait ended at the steer, not at the timeout"
        );
        let text = result.text_content();
        assert!(!result.is_error, "{text}");
        assert!(text.contains("[interrupted]"), "{text}");
        assert!(text.contains("New input ended this wait early."), "{text}");
        assert!(
            text.contains(&format!("Still running: {slow}")),
            "the task was not stopped: {text}"
        );
        manager.stop(&slow);
    }

    /// Input steered before the wait starts (queued while the model's last
    /// request was in flight) ends the wait at once: the channel is not
    /// empty when the wait registers, so no sleeping happens at all.
    #[tokio::test]
    async fn input_steered_before_the_wait_starts_ends_it_at_once() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let slow = spawn(&manager, "sleep 30");
        ctx.steer.steer("already waiting for you");

        let started = std::time::Instant::now();
        let result = run(&ctx, json!({ "timeout": 90, "task_id": slow })).await;

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no waiting happened"
        );
        assert!(result.text_content().contains("[interrupted]"));
        manager.stop(&slow);
    }

    /// A message the turn already drained into its context does not end a
    /// later wait: the steer left no signal behind.
    #[tokio::test]
    async fn a_steer_the_turn_already_took_does_not_end_a_later_wait() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let slow = spawn(&manager, "sleep 30");
        ctx.steer.steer("read before this step");
        ctx.steer.drain();

        let text = run(&ctx, json!({ "timeout": 1, "task_id": slow }))
            .await
            .text_content();

        assert!(text.contains("Timed out after 1 s."), "{text}");
        assert!(!text.contains("[interrupted]"), "{text}");
        manager.stop(&slow);
    }

    /// A task the wait does not watch finishing mid-wait neither ends the
    /// wait nor reads as its timeout: the wait holds out for the watched one.
    #[tokio::test]
    async fn another_task_finishing_does_not_end_the_wait() {
        let (manager, _dir) = manager();
        let ctx = ctx_with(manager.clone());
        let slow = spawn(&manager, "sleep 30");
        let _quick = spawn(&manager, "sleep 0.3");

        let started = std::time::Instant::now();
        let text = run(&ctx, json!({ "timeout": 2, "task_id": slow }))
            .await
            .text_content();

        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "the wait ended early, after {:?}: {text}",
            started.elapsed()
        );
        assert!(text.contains("Timed out after 2 s."), "{text}");
        assert!(text.contains(&format!("Still running: {slow}")), "{text}");
        manager.stop(&slow);
    }
}
