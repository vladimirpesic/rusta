//! A headless run (`rusta -p`): its turns, the limits on them, and waiting for
//! the background agents and keep-alive work a turn leaves behind.

use tokio_util::sync::CancellationToken;

use crate::cli::{self, Cli};
use crate::{SESSION_OCCUPIED_EXIT_CODE, commands, stream};

/// A headless prompt, then the background work it left, as the headless
/// background mode says. Returns the exit code: 1 when a turn failed or the
/// prompt ran out of steps, 0 otherwise.
pub(crate) async fn run_headless(
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
