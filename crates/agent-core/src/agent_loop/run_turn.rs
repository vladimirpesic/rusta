use std::sync::{Arc, Mutex, atomic::Ordering};

use lm_bridge::{message::Message, provider::FinishReason, usage::TokenUsage};
use tokio_util::sync::CancellationToken;

use crate::{
    agent_loop::tool_call::run_tool_call_batch,
    agent_loop::tool_dedup::ToolCallDeduplicator,
    agent_loop::types::ExecutableToolContext,
    context::{CompactMode, CompactionInputs, ContextMemory},
    errors::AgentError,
    permissions::PermissionEngine,
    tool_disclosure::ToolDisclosureState,
    tools::{ToolRegistry, builtin::select_tools::SELECT_TOOLS_TOOL_NAME},
};

use super::{
    llm::{CompactionNotice, CompactionTrigger, Llm, LlmCallbacks},
    retry::{RetryPolicy, chat_with_retry},
    types::{ForkSource, LoopHooks, StepResult, StopReason, ToolEventSender},
};

/// Shared context passed to a single agent turn.
pub struct TurnContext<'a> {
    pub context: &'a mut crate::context::ContextMemory,
    pub registry: Arc<Mutex<ToolRegistry>>,
    pub tool_ctx: &'a ExecutableToolContext,
    pub permissions: &'a PermissionEngine,
    pub cancel: &'a CancellationToken,
    pub tool_event_sender: Option<ToolEventSender>,
    pub loop_hooks: LoopHooks,
    /// When the model stops while background sub-agents are still running, hold
    /// the turn open until they reach a terminal state and feed their results
    /// back in, rather than ending the run and abandoning their work.
    ///
    /// Set by the headless (`-p`) flow. In the REPL the user is still at the
    /// keyboard and the next prompt picks the completions up, so holding there
    /// would only block the prompt.
    pub drain_agent_tasks_on_stop: bool,
    /// Maximum model round-trips (steps) allowed in this turn before the loop
    /// gives up with [`AgentError::MaxTurnsExceeded`].
    pub max_turns: u32,
    /// How persistently a failing step is retried before its error surfaces.
    pub retry: RetryPolicy,
    /// Whether swarm mode is on. Only then is a step pairing `agent_swarm`
    /// with other calls refused whole; otherwise `agent_swarm` is refused
    /// alone, and the rest run.
    pub swarm_mode: bool,
}

/// Whether any call in this batch asks `agent` or `agent_swarm` to fork.
///
/// Matched on the tool name and argument rather than announced by the tool,
/// because the snapshot has to be taken by the loop — only it holds the
/// conversation — and it must be taken before the tool runs.
fn requests_fork(tool_calls: &[lm_bridge::message::ToolCall]) -> bool {
    tool_calls.iter().any(|tc| {
        (tc.name == "agent" || tc.name == "agent_swarm")
            && tc
                .arguments
                .as_deref()
                .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())
                .and_then(|v| v.get("fork").and_then(serde_json::Value::as_bool))
                .unwrap_or(false)
    })
}

/// Consecutive cut-off tool-call steps after which the turn gives up.
const MAX_TRUNCATED_TOOL_STEPS: u32 = 3;

/// What the model is told after its tool calls were cut off.
fn truncated_tool_calls_notice(cut: FinishReason, reason: &str) -> String {
    let cause = if cut == FinishReason::ContentFilter {
        "was stopped by the provider's content filter"
    } else {
        "hit the output-token limit"
    };
    format!(
        "<system-reminder>\nYour previous response {cause} (finish_reason: {reason}) while it \
         was still emitting tool calls, so none of those calls were run. Issue them again, \
         keeping each call's arguments smaller — for example, write a large file in several \
         edits instead of one.\n</system-reminder>"
    )
}

/// Recovery levels applied after successive context-overflow errors.
///
/// The provider's count is the one that matters, and it can exceed Rusta's
/// estimate, so the eviction rungs aim below both the budget and the current
/// estimate.
#[derive(Debug, Clone, Copy)]
enum OverflowRecovery {
    /// First overflow: full compaction — a summary, or protected eviction
    /// when no summary can be had.
    Compact,
    /// Second overflow: drop tool results down to 40 % of the budget (or 60 %
    /// of the current estimate, if lower).
    DropToolResults,
    /// Third overflow: protected eviction to 30 % of the budget (or half the
    /// current estimate, if lower).
    Evict,
}

impl OverflowRecovery {
    fn next(self) -> Option<Self> {
        match self {
            Self::Compact => Some(Self::DropToolResults),
            Self::DropToolResults => Some(Self::Evict),
            Self::Evict => None,
        }
    }
}

/// The tool table one step offers: [`offered_tools`], within the model's cap
/// on tools per request. What does not fit is left out from the end — the
/// loaded dynamic tools first, then the static tools registered last — and
/// the change is logged once.
pub(crate) fn step_tool_table(
    registry: &Mutex<ToolRegistry>,
    tool_ctx: &ExecutableToolContext,
    tool_disclosure: &Mutex<ToolDisclosureState>,
) -> Vec<lm_bridge::tool::Tool> {
    let mut list = offered_tools(registry, tool_ctx, tool_disclosure);
    let cap = tool_ctx.llm.as_ref().and_then(|llm| llm.max_tools());
    let left_out: Vec<String> = match cap {
        Some(cap) if list.len() > cap => list.drain(cap..).map(|t| t.name).collect(),
        _ => Vec::new(),
    };
    if tool_disclosure.lock().unwrap().note_capped(&left_out) && !left_out.is_empty() {
        tracing::warn!(
            "the model takes at most {} tools per request; left out: {}",
            cap.unwrap_or_default(),
            left_out.join(", ")
        );
    }
    list
}

/// Every tool a step would offer, before any cap: the static tools that can
/// work (with goal-scoped ones only while a goal is active), `select_tools`
/// when dynamic tools exist, and the dynamic tools the model has loaded.
pub(crate) fn offered_tools(
    registry: &Mutex<ToolRegistry>,
    tool_ctx: &ExecutableToolContext,
    tool_disclosure: &Mutex<ToolDisclosureState>,
) -> Vec<lm_bridge::tool::Tool> {
    let reg = registry.lock().unwrap();
    let has_dynamic = !reg.dynamic_definition_names().is_empty();
    let goal_manager = tool_ctx.goal_manager.lock().unwrap();
    let mut list: Vec<lm_bridge::tool::Tool> = reg
        .all()
        .iter()
        .filter(|tool| tool.is_available(tool_ctx))
        .map(|tool| tool.tool_definition())
        .filter(|t| !reg.is_dynamic(&t.name))
        .filter(|t| t.name != SELECT_TOOLS_TOOL_NAME)
        .filter(|t| goal_manager.should_offer_tool(&t.name))
        .cloned()
        .collect();
    drop(goal_manager);
    if has_dynamic && let Some(select_tool) = reg.get(SELECT_TOOLS_TOOL_NAME) {
        list.push(select_tool.tool_definition().clone());
    }
    // A model that takes tools on a system message gets the loaded ones
    // there, so this list — the request's prefix — stays fixed.
    if tool_ctx
        .llm
        .as_ref()
        .is_some_and(|llm| llm.supports_dynamic_tools())
    {
        return list;
    }
    let loaded = tool_disclosure.lock().unwrap().loaded().clone();
    for name in loaded {
        if let Some(tool) = reg.get(&name)
            && reg.is_dynamic(&name)
            && tool.is_available(tool_ctx)
        {
            list.push(tool.tool_definition().clone());
        }
    }
    list
}

/// A fork's first request should carry its caller's exact prefix — system
/// prompt and tool table — so it hits the caller's prompt cache. A divergence
/// is logged at debug level. Checked once, at the fork's first step:
/// the baseline is taken here, so later steps and later turns never see it.
fn check_fork_prefix(
    tool_ctx: &ExecutableToolContext,
    context: &crate::context::ContextMemory,
    tools: &[lm_bridge::tool::Tool],
) {
    let Some(baseline) = tool_ctx.fork_prefix_baseline.lock().unwrap().take() else {
        return;
    };
    let system_matches = context.system_prompt() == baseline.system_prompt;
    let tools_match = tools == baseline.tools.as_slice();
    if system_matches && tools_match {
        return;
    }
    let baseline_names: Vec<&str> = baseline.tools.iter().map(|t| t.name.as_str()).collect();
    let offered_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    tracing::debug!(
        system_matches,
        tools_match,
        offered = ?offered_names,
        caller = ?baseline_names,
        "fork's first request prefix differs from its caller's; the fork misses the caller's prompt cache"
    );
}

/// One compaction run: who writes the summary, and who hears about it.
pub(crate) struct CompactionRun<'a> {
    pub llm: Option<&'a dyn Llm>,
    /// The step's tool table, declared on the summary request.
    pub tools: &'a [lm_bridge::tool::Tool],
    pub tool_ctx: &'a ExecutableToolContext,
    pub callbacks: &'a LlmCallbacks,
    pub cancel: &'a CancellationToken,
    pub trigger: CompactionTrigger,
    /// `/compact <instruction>`'s focus text for the summarizer; auto
    /// compactions pass `None`.
    pub instruction: Option<String>,
    /// Where eviction aims when no summary can be had.
    pub target: u64,
}

/// Compacts `context` ([`ContextMemory::compact`]), telling the front-end before
/// and after: summarizing costs a model call, and the history changes under
/// the model. The todo list is re-attached below the summary.
pub(crate) async fn compact_context(
    context: &mut ContextMemory,
    run: CompactionRun<'_>,
) -> Result<CompactionNotice, AgentError> {
    let notify = |notice: &CompactionNotice| {
        if let Some(on_compaction) = &run.callbacks.on_compaction {
            on_compaction(notice);
        }
    };
    notify(&CompactionNotice::Started {
        trigger: run.trigger,
        tokens: context.estimate_tokens(),
    });
    // PreCompact / PostCompact bracket the run; the trigger names the matcher
    // input, "manual" for an asked-for compaction and "auto" otherwise.
    let trigger_name = match run.trigger {
        CompactionTrigger::Manual => "manual",
        CompactionTrigger::Proactive | CompactionTrigger::Overflow => "auto",
    };
    if let Some(hooks) = &run.tool_ctx.user_hooks {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "trigger".to_string(),
            serde_json::Value::String(trigger_name.to_string()),
        );
        hooks
            .fire(
                crate::user_hooks::UserHookEvent::PreCompact,
                trigger_name,
                fields,
            )
            .await;
    }
    let todo = {
        let list = run.tool_ctx.todo_list.lock().unwrap();
        (!list.is_empty()).then(|| list.format())
    };
    let notice = context
        .compact(
            CompactionInputs {
                llm: run.llm,
                tools: run.tools,
                todo,
                instruction: run.instruction,
                cancel: run.cancel,
            },
            run.target,
        )
        .await?;
    tracing::info!("context compaction: {}", notice.describe());
    notify(&notice);
    if let Some(hooks) = &run.tool_ctx.user_hooks {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "trigger".to_string(),
            serde_json::Value::String(trigger_name.to_string()),
        );
        hooks
            .fire(
                crate::user_hooks::UserHookEvent::PostCompact,
                trigger_name,
                fields,
            )
            .await;
    }
    Ok(notice)
}

/// The reminder for a todo list that fell behind its work,
/// when one is due: for the session's own agent, while the list has items
/// open, after [`STALE_REMINDER_STEPS`] steps without a change to it and as
/// many since the last reminder.
///
/// [`STALE_REMINDER_STEPS`]: crate::todo::STALE_REMINDER_STEPS
fn stale_todo_reminder(
    context: &ContextMemory,
    registry: &Mutex<ToolRegistry>,
    tool_ctx: &ExecutableToolContext,
) -> Option<Message> {
    use crate::todo::{STALE_REMINDER_STEPS, changes_todo_list, is_stale_reminder};
    use crate::tools::builtin::todo_list::TODO_LIST_TOOL_NAME;

    if tool_ctx.depth > 0 || registry.lock().unwrap().get(TODO_LIST_TOOL_NAME).is_none() {
        return None;
    }
    let list = tool_ctx.todo_list.lock().unwrap();
    if list.open_count() == 0 {
        return None;
    }
    let steps = context.steps_since(changes_todo_list);
    (steps >= STALE_REMINDER_STEPS
        && context.steps_since(is_stale_reminder) >= STALE_REMINDER_STEPS)
        .then(|| list.stale_reminder(steps))
}

/// Run a single agent turn (possibly multiple tool-call steps).
pub async fn run_turn(
    llm: &dyn Llm,
    turn: TurnContext<'_>,
    prompt: Option<String>,
    callbacks: LlmCallbacks,
) -> Result<StepResult, AgentError> {
    let TurnContext {
        context,
        registry,
        tool_ctx,
        permissions,
        cancel,
        tool_event_sender,
        loop_hooks,
        drain_agent_tasks_on_stop,
        max_turns,
        retry,
        swarm_mode,
    } = turn;

    // A new turn: tools keying per-turn state (like `wait_for`'s repeat-call
    // tally) read this to tell one turn from the next.
    tool_ctx.turn_id.fetch_add(1, Ordering::SeqCst);

    let tool_disclosure = tool_ctx
        .tool_disclosure
        .as_ref()
        .cloned()
        .unwrap_or_else(|| Arc::new(Mutex::new(ToolDisclosureState::new())));

    if let Some(p) = prompt {
        context.add_user_message(p);
    }

    let mut recovery = Some(OverflowRecovery::Compact);
    // What every step of the turn used: the turn's result reports the whole
    // turn, which is what a front-end shows and a goal's token budget counts.
    let mut usage = TokenUsage::empty();
    // Repeated-call state spans the steps of this turn, and only this turn.
    let mut dedup = ToolCallDeduplicator::with_breaker(tool_ctx.repeat_breaker);
    // Consecutive steps whose tool calls were cut off before they completed.
    let mut truncated_tool_steps: u32 = 0;
    // Whether a Stop hook already sent this turn back to the model; a turn
    // is continued that way at most once.
    let mut stop_hook_used = false;
    // Whether this turn already asked for the answer after a reply of
    // reasoning alone; it asks once.
    let mut asked_for_an_answer = false;

    for turn in 0..max_turns {
        if cancel.is_cancelled() {
            return Err(AgentError::Interrupted);
        }

        // Steered input (Ctrl-S) lands before this request, once
        // the previous step's tool batch has finished. A message that
        // arrives while the model's final request is still in flight keeps
        // the turn alive below instead of waiting for a later turn.
        for message in tool_ctx.steer.drain() {
            context.add_user_message(message);
        }

        let mut tools = step_tool_table(&registry, tool_ctx, &tool_disclosure);
        if let Some(hook) = &loop_hooks.before_step {
            hook(&mut tools);
        }
        check_fork_prefix(tool_ctx, context, &tools);

        // Past 90 % of the window, compact before the request rather than wait
        // for the provider to reject it: summarize, or at worst evict with the
        // task statement and the latest request kept.
        if context.needs_proactive_compaction() {
            let target = context.compaction_target();
            compact_context(
                context,
                CompactionRun {
                    llm: Some(llm),
                    tools: &tools,
                    tool_ctx,
                    callbacks: &callbacks,
                    cancel,
                    trigger: CompactionTrigger::Proactive,
                    instruction: None,
                    target,
                },
            )
            .await?;
            // The announcements went with the old history.
            tool_disclosure.lock().unwrap().forget_announcements();
            tool_ctx.agents_md.lock().unwrap().forget_reminders();
        }

        // Announce any changes to the set of loadable dynamic tools, carrying
        // each tool's one-line description so the model can select the right
        // tool without guessing from its name alone.
        let dynamic_tools: std::collections::BTreeMap<String, String> = {
            let reg = registry.lock().unwrap();
            reg.dynamic_definitions()
                .into_iter()
                .map(|t| (t.name.clone(), t.description.clone()))
                .collect()
        };
        // An agent without `select_tools` (`explore`, `plan`) has nothing to
        // load tools with, so it is told nothing about loadable tools.
        let can_load_tools = registry
            .lock()
            .unwrap()
            .get(SELECT_TOOLS_TOOL_NAME)
            .is_some();
        let announcements = if can_load_tools {
            // Registry first, disclosure second: `offered_tools` and
            // `select_tools` lock them in that order.
            let fingerprint = crate::tool_disclosure::ToolDisclosureState::selection_fingerprint(
                &registry.lock().unwrap(),
            );
            let mut disclosure = tool_disclosure.lock().unwrap();
            // Registry changes are observed here even when the model only
            // calls select_tools at announcement boundaries.
            disclosure.reset_failures_if_selection_changed(&fingerprint);
            disclosure.take_announcement_messages(&dynamic_tools)
        } else {
            Vec::new()
        };
        for msg in announcements {
            context.add_message(msg);
        }
        // Tools `select_tools` loaded, for a model that takes them on a system
        // message.
        if llm.supports_dynamic_tools() {
            let names = tool_disclosure.lock().unwrap().take_undelivered();
            if !names.is_empty() {
                let definitions: Vec<lm_bridge::tool::Tool> = {
                    let reg = registry.lock().unwrap();
                    reg.dynamic_definitions()
                        .into_iter()
                        .filter(|tool| names.contains(&tool.name))
                        .cloned()
                        .collect()
                };
                if !definitions.is_empty() {
                    context.add_message(Message {
                        role: lm_bridge::message::Role::System,
                        content: Vec::new(),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        tools: definitions,
                        partial: false,
                    });
                }
            }
        }
        // Instruction files found under directories the last tools touched.
        let agents_md_reminder = tool_ctx.agents_md.lock().unwrap().take_reminder();
        if let Some(reminder) = agents_md_reminder {
            context.add_message(Message::user(reminder));
        }
        if let Some(reminder) = stale_todo_reminder(context, &registry, tool_ctx) {
            context.add_message(reminder);
        }

        let step = match chat_with_retry(
            llm,
            context.build_history(),
            tools.clone(),
            callbacks.clone(),
            cancel,
            retry,
        )
        .await
        {
            Ok(s) => s,
            Err(ref e) if e.is_tool_exchange_structure_error() => {
                tracing::warn!(
                    "Tool-exchange structural error from provider; retrying with orphan tool results removed"
                );
                chat_with_retry(
                    llm,
                    context.build_repaired_history(),
                    tools.clone(),
                    callbacks.clone(),
                    cancel,
                    retry,
                )
                .await?
            }
            // Too many bytes rather than too many tokens — inline media, in
            // practice. The history would be resent as it is and fail again,
            // so the oldest media goes.
            Err(lm_bridge::errors::LlmError::ApiRequestTooLarge(message)) => {
                let omitted = context.omit_oldest_media();
                if omitted == 0 {
                    return Err(AgentError::Llm(
                        lm_bridge::errors::LlmError::ApiRequestTooLarge(message),
                    ));
                }
                tracing::warn!(
                    "the provider rejected the request as too large ({message}); omitted \
                     {omitted} older media item(s) from the history and retrying"
                );
                continue;
            }
            Err(lm_bridge::errors::LlmError::ApiContextOverflow) => {
                recover_from_overflow(
                    &mut recovery,
                    context,
                    llm,
                    &tools,
                    tool_ctx,
                    &callbacks,
                    cancel,
                )
                .await?;
                tool_disclosure.lock().unwrap().forget_announcements();
                tool_ctx.agents_md.lock().unwrap().forget_reminders();
                continue;
            }
            Err(e) => return Err(AgentError::Llm(e)),
        };
        usage.add(&step.usage);
        report_subagent_usage(tool_ctx, &step.usage);

        if let Some(hook) = &loop_hooks.after_step {
            hook(&step);
        }

        // Z.AI reports an overrun context on a *successful* response, so it
        // never reaches the error path. Run the same recovery ladder. (A
        // generation the provider cut short never gets here: the stream
        // consumer fails it, and the retry layer re-issues the request.)
        if step.finish_reason == Some(FinishReason::ContextWindowExceeded) {
            tracing::warn!("Provider reported a context-window overrun; compacting");
            recover_from_overflow(
                &mut recovery,
                context,
                llm,
                &tools,
                tool_ctx,
                &callbacks,
                cancel,
            )
            .await?;
            tool_disclosure.lock().unwrap().forget_announcements();
            tool_ctx.agents_md.lock().unwrap().forget_reminders();
            continue;
        }

        // Tool calls cut off by the output limit or the content filter are
        // fragments: their arguments may parse and still be wrong — a
        // `write_file` whose content stops half-way is valid JSON. None of them
        // run. The model is told why and asked to re-issue smaller calls; a
        // model that keeps overrunning ends the turn instead of looping.
        if !step.tool_calls().is_empty()
            && let Some(cut @ (FinishReason::Length | FinishReason::ContentFilter)) =
                step.finish_reason
        {
            truncated_tool_steps += 1;
            let reason = if cut == FinishReason::Length {
                "length"
            } else {
                "content_filter"
            }
            .to_string();
            let mut spoken = step.message.clone();
            spoken.tool_calls.clear();
            context.add_message(spoken);
            if truncated_tool_steps >= MAX_TRUNCATED_TOOL_STEPS {
                return Err(AgentError::TruncatedToolCalls {
                    reason,
                    count: truncated_tool_steps,
                });
            }
            context.add_message(Message::user(truncated_tool_calls_notice(cut, &reason)));
            continue;
        }
        truncated_tool_steps = 0;

        // Record the assistant's turn — text, reasoning and tool calls together,
        // and before the branch, so it happens whether or not the model called
        // tools. Recording only the tool calls would throw away the model's own
        // words on every step: nothing to replay for Preserved Thinking,
        // nothing for a sub-agent to report back, and no final answer in the
        // records for a resumed session.
        context.add_message(step.message.clone());

        if step.tool_calls().is_empty() {
            // The turn is over unless the model asked to call tools, or a hook
            // asks us to keep going. An unrecognized reason ends the turn: the
            // old rule was `reason == Stop`, so anything else — `length`,
            // `sensitive`, or a reason Rusta could not parse — sent the loop
            // round again until it hit the turn cap.
            let expects_continuation = step
                .finish_reason
                .is_some_and(lm_bridge::provider::FinishReason::expects_continuation);
            let steered = !tool_ctx.steer.is_empty();
            let should_continue = steered
                || expects_continuation
                || loop_hooks
                    .should_continue_after_stop
                    .as_ref()
                    .map(|h| h(&step))
                    .unwrap_or(false);
            if !should_continue {
                // A reply of reasoning alone, finished normally, said nothing:
                // the model is asked for its answer, once a turn, rather than
                // the reply being resent as an empty one. A second one ends the
                // turn as it is.
                if step.message.is_thinking_only() {
                    if !asked_for_an_answer {
                        asked_for_an_answer = true;
                        context.add_message(Message::user(THINKING_ONLY_REMINDER));
                        continue;
                    }
                    tracing::warn!(
                        "the model answered with reasoning alone again; the turn ends without \
                         a visible answer"
                    );
                }
                // The model is done, but sub-agents it launched may not be. In
                // headless mode nothing else will ever collect them, so wait
                // and let the model react to what they found.
                if drain_agent_tasks_on_stop
                    && drain_background_agents(tool_ctx, context, cancel).await?
                {
                    continue;
                }
                // The turn really ends here, so Stop hooks run now: at most
                // once per turn, and never for a filtered answer. Only a block
                // keeps the turn going, its reason handed to the model; an
                // allowing hook's stdout is not context, or a hook that merely
                // notifies would send every turn back to the model until the
                // step cap.
                if !stop_hook_used
                    && step.finish_reason != Some(FinishReason::ContentFilter)
                    && let Some(hooks) = &tool_ctx.user_hooks
                {
                    let mut fields = serde_json::Map::new();
                    fields.insert(
                        "stop_hook_active".to_string(),
                        serde_json::Value::Bool(false),
                    );
                    let outcome = hooks
                        .fire(crate::user_hooks::UserHookEvent::Stop, "", fields)
                        .await;
                    if let Some(reason) = outcome.blocked {
                        stop_hook_used = true;
                        context.add_message(Message::user(reason));
                        continue;
                    }
                    if !outcome.stdout.trim().is_empty() {
                        tracing::debug!("ignoring an allowing Stop hook's stdout");
                    }
                }
                // Return the step whole: its message carries the model's final
                // answer, which is what the caller reports and what a sub-agent
                // hands back to its parent. We are inside the no-tool-calls
                // branch, so it already signals a finished turn. Its usage is
                // the turn's.
                return Ok(StepResult {
                    usage: with_subagent_usage(tool_ctx, usage),
                    ..step
                });
            }
            // The loop is going round again. Steered input speaks for itself —
            // the drained user message above is why — so only a stop a loop
            // hook continued, or an unrecognized finish reason, gets the note.
            if !steered {
                context.add_message(Message::system(step.finish_reason.map_or_else(
                    || "(continuing)".to_string(),
                    |fr| format!("(continuing after {fr:?})"),
                )));
            }
            continue;
        }

        // `agent_swarm` runs alone: a step that pairs it with other calls, or
        // makes several, is refused whole — in swarm mode, where it can run
        // at all.
        if swarm_mode
            && let Some(denial) = crate::swarm::step_denial(step.tool_calls(), tool_ctx.depth > 0)
        {
            for call in step.tool_calls() {
                let result = crate::agent_loop::types::ExecutableToolResult::error(denial.clone());
                if let Some(sender) = &tool_event_sender {
                    sender(crate::agent_loop::types::ToolEvent::Call {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        args: call.arguments.clone().unwrap_or_default(),
                        status: crate::agent_loop::types::ToolStatus::Failed,
                    });
                    sender(crate::agent_loop::types::ToolEvent::Result {
                        id: call.id.clone(),
                        result,
                        status: crate::agent_loop::types::ToolStatus::Failed,
                    });
                }
                context.add_message(Message::tool_result(call.id.clone(), denial.clone()));
            }
            continue;
        }

        // A fork inherits the conversation *including* the `agent` call asking
        // for it, which is why the snapshot is taken here — after the assistant
        // message is recorded, before the batch runs. Only when a fork was
        // actually requested: this clones the whole conversation, and doing that
        // on every step to serve a tool that is almost never called would be a
        // per-step cost paid for nothing.
        if requests_fork(step.tool_calls()) {
            *tool_ctx.fork_source.lock().unwrap() = Some(ForkSource {
                system_prompt: context.system_prompt().to_string(),
                history: context.fork_snapshot(),
            });
        }

        // Execute tools and append results.
        let tool_calls = step.message.tool_calls.clone();
        let batch = run_tool_call_batch(
            step.message.tool_calls.clone(),
            Arc::clone(&registry),
            tool_ctx,
            permissions,
            cancel,
            tool_event_sender.clone(),
            &mut dedup,
        )
        .await;
        // Cleared unconditionally: a snapshot left behind would silently seed a
        // later fork from a point the model never asked for.
        *tool_ctx.fork_source.lock().unwrap() = None;
        match batch {
            Ok(outcome) => {
                {
                    let mut agents_md = tool_ctx.agents_md.lock().unwrap();
                    for call in &tool_calls {
                        let args: serde_json::Value = call
                            .arguments
                            .as_deref()
                            .and_then(|raw| serde_json::from_str(raw).ok())
                            .unwrap_or_default();
                        agents_md.observe(
                            &call.name,
                            &args,
                            &tool_ctx.cwd,
                            !outcome.failed.contains(&call.id),
                        );
                    }
                }
                for msg in outcome.messages {
                    context.add_message_with_blob(msg).await?;
                }
                // The step is on disk before the next request: a process
                // killed mid-turn (SIGKILL, a crash,
                // the OOM killer) keeps the steps it took, which a write at the
                // turn's end alone would lose.
                context.persist().await?;
                if outcome.stop_turn {
                    // The repeated-call guard gave up on a model stuck in a
                    // loop. One text-only step hands the work back.
                    tracing::info!("Turn stopped by the repeat breaker at step {turn}");
                    return handoff(
                        llm,
                        context,
                        StopReason::RepeatBreaker,
                        &tools,
                        &callbacks,
                        cancel,
                        retry,
                    )
                    .await
                    .map(|result| with_turn_usage(tool_ctx, result, usage));
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Tool batch failed; synthesizing abandoned results for {} call(s)",
                    tool_calls.len()
                );
                context.close_abandoned_tool_calls(&tool_calls);
                return Err(e);
            }
        }
    }

    if max_turns == 0 {
        return Err(AgentError::MaxTurnsExceeded);
    }
    tracing::info!("Turn used all {max_turns} steps; asking for a handoff");
    let tools = step_tool_table(&registry, tool_ctx, &tool_disclosure);
    handoff(
        llm,
        context,
        StopReason::MaxTurns,
        &tools,
        &callbacks,
        cancel,
        retry,
    )
    .await
    .map(|result| with_turn_usage(tool_ctx, result, usage))
}

/// `result`, the turn's last step — a handoff — reporting the whole turn's
/// usage: the steps before it (`earlier`) and its own.
fn with_turn_usage(
    tool_ctx: &ExecutableToolContext,
    mut result: StepResult,
    earlier: TokenUsage,
) -> StepResult {
    report_subagent_usage(tool_ctx, &result.usage);
    result.usage.add(&earlier);
    result.usage = with_subagent_usage(tool_ctx, result.usage);
    result
}

/// A sub-agent's step counts toward what its session reports: its tokens are
/// added to the tree's shared tally. The session's own steps are its turn's.
fn report_subagent_usage(tool_ctx: &ExecutableToolContext, step: &TokenUsage) {
    if tool_ctx.depth > 0 {
        tool_ctx.subagent_usage.lock().unwrap().add(step);
    }
}

/// A turn's usage, for the session's own agent with the tokens its sub-agents
/// spent since its last turn reported them: a turn reports every model call
/// it caused, which is what `--stream-json`'s `usage` shows and a goal's
/// token budget counts. A sub-agent's turn reports its own.
fn with_subagent_usage(tool_ctx: &ExecutableToolContext, mut usage: TokenUsage) -> TokenUsage {
    if tool_ctx.depth == 0 {
        usage.add(&std::mem::take(
            &mut *tool_ctx.subagent_usage.lock().unwrap(),
        ));
    }
    usage
}

/// What a model that answered with reasoning alone is told, once a turn.
pub(crate) const THINKING_ONLY_REMINDER: &str = "<system-reminder>\nYour previous response had \
     no visible output. Continue and produce a user-visible response.\n</system-reminder>";

/// The reminder that asks a stopped model for its handoff.
pub(crate) fn handoff_reminder(reason: StopReason) -> String {
    format!(
        "<system-reminder>\nYou were stopped ({}). Do not call tools. Summarize what you \
         completed, what is blocked, and the next step.\n</system-reminder>",
        reason.describe()
    )
}

/// One text-only step after the turn was stopped: the model is told why and
/// asked to hand the work back, and any tool call it still makes is dropped.
///
/// The step declares the turn's own tools rather than none, so the request
/// keeps the conversation's prefix — and the provider's prompt cache — and
/// an endpoint that expects the tools of a history's tool calls to be
/// declared gets them. Returns the handoff with `stop_reason` set, or
/// [`AgentError::NoFinalMessage`] when the model wrote nothing.
async fn handoff(
    llm: &dyn Llm,
    context: &mut ContextMemory,
    reason: StopReason,
    tools: &[lm_bridge::tool::Tool],
    callbacks: &LlmCallbacks,
    cancel: &CancellationToken,
    retry: RetryPolicy,
) -> Result<StepResult, AgentError> {
    context.add_message(Message::user(handoff_reminder(reason)));
    let ask = |context: &ContextMemory| {
        chat_with_retry(
            llm,
            context.build_history(),
            tools.to_vec(),
            callbacks.clone(),
            cancel,
            retry,
        )
    };
    let mut step = ask(context).await.map_err(AgentError::Llm)?;
    // Reasoning alone is no handoff: the model is asked for it once more.
    if step.message.is_thinking_only() {
        context.add_message(step.message.clone());
        context.add_message(Message::user(THINKING_ONLY_REMINDER));
        let earlier = step.usage;
        step = ask(context).await.map_err(AgentError::Llm)?;
        step.usage.add(&earlier);
    }
    let mut message = step.message;
    if !message.tool_calls.is_empty() {
        tracing::warn!(
            "the handoff step called {} tool(s) after being told not to; dropped",
            message.tool_calls.len()
        );
        message.tool_calls.clear();
    }
    if message.text_content().trim().is_empty() {
        return Err(AgentError::NoFinalMessage(reason));
    }
    context.add_message(message.clone());
    Ok(StepResult {
        message,
        usage: step.usage,
        finish_reason: Some(FinishReason::Stop),
        response_id: step.response_id,
        stop_reason: Some(reason),
    })
}

/// Applies the next step of the context-overflow recovery ladder, or fails when
/// every step has been exhausted.
async fn recover_from_overflow(
    recovery: &mut Option<OverflowRecovery>,
    context: &mut ContextMemory,
    llm: &dyn Llm,
    tools: &[lm_bridge::tool::Tool],
    tool_ctx: &ExecutableToolContext,
    callbacks: &LlmCallbacks,
    cancel: &CancellationToken,
) -> Result<(), AgentError> {
    let Some(step) = *recovery else {
        return Err(AgentError::Llm(
            lm_bridge::errors::LlmError::ApiContextOverflow,
        ));
    };
    let share = |of: u64, tenths: u64| of.saturating_mul(tenths).saturating_div(10);
    let budget = context.max_tokens();
    let estimate = context.estimate_tokens();
    match step {
        OverflowRecovery::Compact => {
            let target = context.compaction_target().min(share(estimate, 5));
            compact_context(
                context,
                CompactionRun {
                    llm: Some(llm),
                    tools,
                    tool_ctx,
                    callbacks,
                    cancel,
                    trigger: CompactionTrigger::Overflow,
                    instruction: None,
                    target,
                },
            )
            .await?;
        }
        OverflowRecovery::DropToolResults => {
            let target = share(budget, 4).min(share(estimate, 6));
            let dropped = context.compact_to(target, CompactMode::ToolResultsOnly);
            tracing::warn!(
                "context overflow persisted after compaction; dropped {} tool results",
                dropped.len()
            );
        }
        OverflowRecovery::Evict => {
            let target = share(budget, 3).min(share(estimate, 5));
            let notice = context.evict_with_notice(
                target,
                "the provider still reported a context overrun".to_string(),
            );
            tracing::warn!("context overflow recovery: {}", notice.describe());
            if let Some(on_compaction) = &callbacks.on_compaction {
                on_compaction(&notice);
            }
        }
    }
    *recovery = step.next();
    Ok(())
}

/// Waits for every running background sub-agent to reach a terminal state, then
/// feeds their completion notices into the context.
///
/// Returns whether anything was drained — i.e. whether the caller should run
/// another step so the model can act on the results.
///
/// Sub-agents are re-enumerated on each pass, so a sub-agent that fans out into
/// further sub-agents is waited on too. The wait is bounded only by each task's
/// own timeout and by `cancel`: a deadline here would abandon exactly the
/// long-running work that is most expensive to lose.
async fn drain_background_agents(
    tool_ctx: &ExecutableToolContext,
    context: &mut crate::context::ContextMemory,
    cancel: &CancellationToken,
) -> Result<bool, AgentError> {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

    let background = &tool_ctx.background;
    if background.active_agent_task_count() == 0 {
        // Nothing running. Any completions that landed after the last flush
        // still deserve to reach the model.
        return Ok(flush_completion_markers(tool_ctx, context));
    }

    tracing::info!(
        "Holding turn for {} background sub-agent(s)",
        background.active_agent_task_count()
    );

    while background.active_agent_task_count() > 0 {
        if cancel.is_cancelled() {
            return Err(AgentError::Interrupted);
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = cancel.cancelled() => return Err(AgentError::Interrupted),
        }
    }

    Ok(flush_completion_markers(tool_ctx, context))
}

/// Moves pending background completion notices into the context as user
/// messages. Returns whether any were moved.
fn flush_completion_markers(
    tool_ctx: &ExecutableToolContext,
    context: &mut crate::context::ContextMemory,
) -> bool {
    let markers = tool_ctx.background.drain_completion_markers();
    let drained = !markers.is_empty();
    for marker in markers {
        context.add_message(lm_bridge::message::Message::user(marker));
    }
    drained
}
