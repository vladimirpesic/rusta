use std::{
    collections::{BTreeMap, HashSet},
    pin::Pin,
    sync::Arc,
    sync::Mutex,
};

use futures::Future;
use lm_bridge::message::{Message, Role, ToolCall};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// A pinned future that executes a single tool call and returns its ID and result.
type ToolFuture = Pin<Box<dyn Future<Output = (String, ExecutableToolResult)> + Send>>;

use crate::{
    agent_loop::tool_dedup::{CallSlot, ToolCallDeduplicator},
    agent_loop::types::{
        ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses, ToolEvent,
        ToolEventSender, ToolStatus,
    },
    errors::AgentError,
    permissions::{ApprovalResponse, PermissionEngine, PolicyCheck},
    tools::ToolRegistry,
};

use super::tool_scheduler::ToolScheduler;

/// What a batch of tool calls produced.
pub struct ToolBatchOutcome {
    /// One `Role::Tool` message per call, in the order the provider issued them.
    pub messages: Vec<Message>,
    /// A tool asked the turn to end (see [`ExecutableToolResult::stop_turn`]).
    pub stop_turn: bool,
    /// The ids of the calls whose result is an error.
    pub failed: HashSet<String>,
}

/// A same-step duplicate: it never executes, and inherits the original's result.
struct DuplicateCall {
    slot: CallSlot,
    tool_call_id: String,
    name: String,
    args: Value,
}

/// Validates, approves, and executes a batch of tool calls.
///
/// `dedup` carries repeated-call state across the steps of one turn: it
/// collapses calls repeated within this batch, and escalates a reminder onto
/// results whose `(tool, args)` pair keeps recurring across batches.
pub async fn run_tool_call_batch(
    tool_calls: Vec<ToolCall>,
    registry: Arc<Mutex<ToolRegistry>>,
    ctx: &ExecutableToolContext,
    permissions: &PermissionEngine,
    cancel: &CancellationToken,
    tool_event_sender: Option<ToolEventSender>,
    dedup: &mut ToolCallDeduplicator,
) -> Result<ToolBatchOutcome, AgentError> {
    let scheduler = ToolScheduler::new();

    // Prepare all calls while holding the registry lock, then drop it before
    // any await so the resulting future remains `Send`.
    let prepared: Vec<PreparedToolCall> = {
        let reg = registry.lock().unwrap();
        tool_calls
            .into_iter()
            .map(|tc| prepare_tool_call(tc, &reg))
            .collect()
    };

    dedup.begin_step();

    let mut tasks: Vec<(ToolAccesses, ToolFuture)> = Vec::with_capacity(prepared.len());
    // Parallel to `tasks`: which slot each future fills, and its tool name.
    let mut task_slots: Vec<(CallSlot, String)> = Vec::with_capacity(prepared.len());
    let mut duplicates: Vec<DuplicateCall> = Vec::new();

    for prepared in prepared {
        let tool_call_id = prepared.id;
        let name = prepared.name;
        let args = prepared.args;
        let slot = dedup.register(&name, &args);
        if !slot.is_original() {
            duplicates.push(DuplicateCall {
                slot,
                tool_call_id,
                name,
                args,
            });
            continue;
        }
        // Every path below pushes exactly one task, so registering the slot
        // here keeps `task_slots` aligned with `tasks`.
        task_slots.push((slot, name.clone()));

        if let Some(reason) = prepared.missing_reason {
            let result = ExecutableToolResult::error(&reason);
            fire_tool_lifecycle_hook(ctx, &name, &args, &result);
            if let Some(sender) = &tool_event_sender {
                sender(ToolEvent::Call {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    args: serde_json::to_string(&args).unwrap_or_default(),
                    status: ToolStatus::Failed,
                });
                sender(ToolEvent::Result {
                    id: tool_call_id.clone(),
                    result: result.clone(),
                    status: ToolStatus::Failed,
                });
            }
            tasks.push((
                prepared.accesses,
                Box::pin(async move { (tool_call_id, result) }),
            ));
            continue;
        }

        let tool = prepared.tool.expect("missing tool handled above");

        if let Err(err) = validate_arguments(tool.tool_definition(), &args) {
            let reason = format!("Invalid arguments for {name}: {err}");
            let result = ExecutableToolResult::error(&reason);
            fire_tool_lifecycle_hook(ctx, &name, &args, &result);
            if let Some(sender) = &tool_event_sender {
                sender(ToolEvent::Call {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    args: serde_json::to_string(&args).unwrap_or_default(),
                    status: ToolStatus::Failed,
                });
                sender(ToolEvent::Result {
                    id: tool_call_id.clone(),
                    result: result.clone(),
                    status: ToolStatus::Failed,
                });
            }
            tasks.push((
                prepared.accesses,
                Box::pin(async move { (tool_call_id, result) }),
            ));
            continue;
        }

        let accesses = tool.accesses(&args);

        // A call-specific refusal (the cron tools' plan-mode veto and their
        // pre-approval validation) lands before the approval gate: asking the
        // user about a call the tool will not run would be noise.
        if let Err(reason) = tool.preflight(&args, ctx) {
            let result = ExecutableToolResult::error(reason);
            fire_tool_lifecycle_hook(ctx, &name, &args, &result);
            if let Some(sender) = &tool_event_sender {
                sender(ToolEvent::Call {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    args: serde_json::to_string(&args).unwrap_or_default(),
                    status: ToolStatus::Failed,
                });
                sender(ToolEvent::Result {
                    id: tool_call_id.clone(),
                    result: result.clone(),
                    status: ToolStatus::Failed,
                });
            }
            tasks.push((accesses, Box::pin(async move { (tool_call_id, result) })));
            continue;
        }

        // User hooks run before the permission check (PreToolUse):
        // a block refuses the call with the hook's reason, exactly like a
        // denial, and an allowing hook's stdout joins the call's context.
        let mut hook_block: Option<String> = None;
        let mut hook_context = String::new();
        if let Some(hooks) = &ctx.user_hooks {
            let mut fields = serde_json::Map::new();
            fields.insert("tool_name".to_string(), Value::String(name.clone()));
            fields.insert("tool_input".to_string(), args.clone());
            let outcome = hooks
                .fire(crate::user_hooks::UserHookEvent::PreToolUse, &name, fields)
                .await;
            hook_block = outcome.blocked;
            hook_context = outcome.stdout;
        }

        let denied = if let Some(reason) = hook_block {
            // The hook's block is final: the user is not asked to approve a
            // call that will not run, no permission hook fires for it, and no
            // answer is recorded as a session rule.
            Some(format!("Blocked by hook: {reason}"))
        } else {
            // Evaluate user-configured policy rules before the generic heuristics.
            match permissions.check_policy(&name, &args, ctx) {
                PolicyCheck::Allow => None,
                PolicyCheck::Deny => Some(format!(
                    "Permission denied for {name}. Do not retry the exact same call; diagnose and adjust your approach."
                )),
                PolicyCheck::Prompt { rule_key, scope } => {
                    fire_permission_hook(ctx, &name, &args, true, None);
                    let response = permissions.request_approval(&rule_key, &name, &args).await;
                    fire_permission_hook(ctx, &name, &args, false, Some(&response));
                    permissions.record_policy_response(response, scope, &rule_key);
                    match response {
                        ApprovalResponse::AllowOnce | ApprovalResponse::AllowSession => None,
                        ApprovalResponse::DenyOnce | ApprovalResponse::DenySession => {
                            Some(refusal(tool.as_ref(), &name, permissions.can_ask()))
                        }
                        ApprovalResponse::Cancel => return Err(AgentError::Interrupted),
                    }
                }
                PolicyCheck::NoMatch => {
                    let approval = tool.approval_rule(&args);
                    if approval != "silent" && !permissions.is_allowed(approval, &args, ctx) {
                        fire_permission_hook(ctx, &name, &args, true, None);
                        let response = permissions.request_approval(approval, &name, &args).await;
                        fire_permission_hook(ctx, &name, &args, false, Some(&response));
                        match response {
                            ApprovalResponse::AllowOnce | ApprovalResponse::AllowSession => None,
                            ApprovalResponse::DenyOnce | ApprovalResponse::DenySession => {
                                Some(refusal(tool.as_ref(), &name, permissions.can_ask()))
                            }
                            ApprovalResponse::Cancel => return Err(AgentError::Interrupted),
                        }
                    } else {
                        None
                    }
                }
            }
        };

        if let Some(reason) = denied {
            let result = ExecutableToolResult::error(&reason);
            fire_tool_lifecycle_hook(ctx, &name, &args, &result);
            if let Some(sender) = &tool_event_sender {
                sender(ToolEvent::Call {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    args: serde_json::to_string(&args).unwrap_or_default(),
                    status: ToolStatus::Failed,
                });
                sender(ToolEvent::Result {
                    id: tool_call_id.clone(),
                    result: result.clone(),
                    status: ToolStatus::Failed,
                });
            }
            tasks.push((accesses, Box::pin(async move { (tool_call_id, result) })));
            continue;
        }

        let args_str = serde_json::to_string(&args).unwrap_or_default();
        if let Some(sender) = &tool_event_sender {
            sender(ToolEvent::Call {
                id: tool_call_id.clone(),
                name: name.clone(),
                args: args_str.clone(),
                status: ToolStatus::Pending,
            });
        }

        let mut ctx = ctx.clone();
        ctx.tool_event_sender = tool_event_sender.clone();
        let cancel = cancel.clone();
        let args_for_hooks = args.clone();
        let future: ToolFuture = Box::pin(async move {
            ctx.emit_tool_event(ToolEvent::Progress {
                id: tool_call_id.clone(),
                stdout_chunk: None,
                stderr_chunk: None,
                status: ToolStatus::Running,
            });
            let result = tokio::select! {
                result = tool.execute(args, &ctx, &tool_call_id) => result,
                _ = cancel.cancelled() => ExecutableToolResult::error("Tool execution cancelled"),
            };
            fire_tool_lifecycle_hook(&ctx, &name, &args_for_hooks, &result);
            let mut result = result;
            if !hook_context.is_empty() {
                result.output.push(lm_bridge::message::ContentPart::Text {
                    text: format!("\n\n<hooks>\n{}\n</hooks>", hook_context.trim()),
                });
            }
            // Bounded before the reminder is appended (the dedup pass below),
            // so a repeated-call reminder always survives the cut.
            let result = crate::tool_output::bound_tool_result(
                result,
                &name,
                &tool_call_id,
                &ctx.tool_output,
            )
            .await;
            let status = if result.is_error {
                ToolStatus::Failed
            } else {
                ToolStatus::Completed
            };
            ctx.emit_tool_event(ToolEvent::Result {
                id: tool_call_id.clone(),
                result: result.clone(),
                status,
            });
            (tool_call_id, result)
        });
        tasks.push((accesses, future));
    }

    let results = scheduler.run_batch(tasks).await;

    // Attach any repeated-call reminder, then index results by slot so the
    // messages come back in the order the provider issued the calls, not the
    // order the scheduler happened to finish them in.
    let mut by_slot: BTreeMap<usize, (String, ExecutableToolResult)> = BTreeMap::new();
    for ((slot, name), (tool_call_id, result)) in task_slots.iter().zip(results) {
        let result = dedup.finalize(*slot, name, result);
        by_slot.insert(slot.index, (tool_call_id, result));
    }

    // Duplicates inherit their original's finalized result — reminder included.
    for dup in duplicates {
        let original = dup
            .slot
            .duplicate_of
            .and_then(|index| by_slot.get(&index))
            .map(|(_, result)| result.clone());
        let result = original.unwrap_or_else(|| {
            // The original was never scheduled, which cannot happen: a slot is
            // only marked duplicate after its original was registered.
            ExecutableToolResult::error("Tool call deduplicated but original result was lost")
        });
        // The finalize hook already saw this exact `(name, args)` pair on the
        // original, and its result — reminder included — is what we inherit.
        // Re-running it here would let a redacting hook strip the reminder from
        // the duplicate only.
        emit_dedup_events(&tool_event_sender, &dup, &result);
        by_slot.insert(dup.slot.index, (dup.tool_call_id, result));
    }

    dedup.end_step();

    let mut stop_turn = false;
    let mut failed = HashSet::new();
    let messages = by_slot
        .into_values()
        .map(|(tool_call_id, result)| {
            stop_turn |= result.stop_turn;
            if result.is_error {
                failed.insert(tool_call_id.clone());
            }
            Message {
                role: Role::Tool,
                content: result.output,
                tool_calls: Vec::new(),
                tool_call_id: Some(tool_call_id),
                tools: Vec::new(),
                partial: false,
            }
        })
        .collect();

    Ok(ToolBatchOutcome {
        messages,
        stop_turn,
        failed,
    })
}

/// Mirrors the original call's events onto the duplicate's id, so a UI pairing
/// `Call` with `Result` by id does not strand the duplicate as forever-pending.
fn emit_dedup_events(
    sender: &Option<ToolEventSender>,
    dup: &DuplicateCall,
    result: &ExecutableToolResult,
) {
    let Some(sender) = sender else { return };
    let status = if result.is_error {
        ToolStatus::Failed
    } else {
        ToolStatus::Completed
    };
    sender(ToolEvent::Call {
        id: dup.tool_call_id.clone(),
        name: dup.name.clone(),
        args: serde_json::to_string(&dup.args).unwrap_or_default(),
        status,
    });
    sender(ToolEvent::Result {
        id: dup.tool_call_id.clone(),
        result: result.clone(),
        status,
    });
}

/// Fire PostToolUse / PostToolUseFailure for a finished call, including calls
/// that failed or were blocked before they ran. Spawned, so error paths that
/// build a result inline do not have to await it.
fn fire_tool_lifecycle_hook(
    ctx: &ExecutableToolContext,
    name: &str,
    args: &Value,
    result: &ExecutableToolResult,
) {
    let Some(hooks) = ctx.user_hooks.clone() else {
        return;
    };
    let success = !result.is_error;
    let mut fields = serde_json::Map::new();
    fields.insert("tool_name".to_string(), Value::String(name.to_string()));
    fields.insert("tool_input".to_string(), args.clone());
    let text: String = result.text_content().chars().take(2_000).collect();
    if success {
        fields.insert("tool_response".to_string(), Value::String(text));
    } else {
        fields.insert("error".to_string(), Value::String(text));
    }
    hooks.fire_spawned(
        if success {
            crate::user_hooks::UserHookEvent::PostToolUse
        } else {
            crate::user_hooks::UserHookEvent::PostToolUseFailure
        },
        name.to_string(),
        fields,
    );
}

/// What the model is told about a call that needed approval and did not get
/// it: the tool's own words, else the user's refusal, or that nobody could
/// approve it — a run with no one to ask (`rusta -p --yes`, a background
/// sub-agent) refuses at once, and the refusal is not the user's.
fn refusal(tool: &dyn ExecutableTool, name: &str, asked: bool) -> String {
    if let Some(text) = tool.refusal(asked) {
        return text.to_string();
    }
    if asked {
        format!(
            "User rejected approval for {name}. Do not retry the exact same call; diagnose and adjust your approach."
        )
    } else {
        format!("{name} needs approval, but nobody can approve it in this run. Do not retry it.")
    }
}

/// Fire PermissionRequest before the approval wait and PermissionResult after
/// it, with the decision the user gave.
fn fire_permission_hook(
    ctx: &ExecutableToolContext,
    name: &str,
    args: &Value,
    request: bool,
    response: Option<&ApprovalResponse>,
) {
    let Some(hooks) = ctx.user_hooks.clone() else {
        return;
    };
    let mut fields = serde_json::Map::new();
    fields.insert("tool_name".to_string(), Value::String(name.to_string()));
    fields.insert("tool_input".to_string(), args.clone());
    let event = if request {
        crate::user_hooks::UserHookEvent::PermissionRequest
    } else {
        crate::user_hooks::UserHookEvent::PermissionResult
    };
    if let Some(response) = response {
        let decision = match response {
            ApprovalResponse::AllowOnce => "allow_once",
            ApprovalResponse::AllowSession => "allow_session",
            ApprovalResponse::DenyOnce => "deny_once",
            ApprovalResponse::DenySession => "deny_session",
            ApprovalResponse::Cancel => "cancel",
        };
        fields.insert("decision".to_string(), Value::String(decision.to_string()));
    }
    hooks.fire_spawned(event, name.to_string(), fields);
}

struct PreparedToolCall {
    id: String,
    name: String,
    tool: Option<Arc<dyn ExecutableTool>>,
    args: Value,
    accesses: ToolAccesses,
    missing_reason: Option<String>,
}

fn prepare_tool_call(tc: ToolCall, registry: &ToolRegistry) -> PreparedToolCall {
    if tc.name.trim().is_empty() {
        return PreparedToolCall {
            id: tc.id,
            name: tc.name,
            tool: None,
            args: Value::Object(serde_json::Map::new()),
            accesses: ToolAccesses::default(),
            missing_reason: Some(
                "The tool call had no tool name, so it could not be run. Call the tool again \
                 by name."
                    .to_string(),
            ),
        };
    }
    // Arguments that are not valid JSON — most often a call cut off mid-stream
    // — answer *this* call with an error the model can act on. Failing the
    // whole batch instead would throw away every other call in the step and end
    // the turn over one malformed argument string.
    let args = match parse_arguments(&tc) {
        Ok(args) => args,
        Err(e) => {
            return PreparedToolCall {
                id: tc.id,
                name: tc.name.clone(),
                tool: None,
                args: Value::Object(serde_json::Map::new()),
                accesses: ToolAccesses::default(),
                missing_reason: Some(format!(
                    "The arguments for {} were not valid JSON ({e}), so the call was not run. \
                     They may have been cut off; call the tool again with complete arguments.",
                    tc.name
                )),
            };
        }
    };
    let Some(tool) = registry.get(&tc.name) else {
        return PreparedToolCall {
            id: tc.id,
            name: tc.name.clone(),
            tool: None,
            args,
            accesses: ToolAccesses::default(),
            missing_reason: Some(format!(r#"Tool "{}" not found"#, tc.name)),
        };
    };

    let args = tool.normalize_arguments(args);
    let accesses = tool.accesses(&args);
    PreparedToolCall {
        id: tc.id,
        name: tc.name,
        tool: Some(tool),
        args,
        accesses,
        missing_reason: None,
    }
}

fn parse_arguments(tc: &ToolCall) -> Result<Value, serde_json::Error> {
    match &tc.arguments {
        Some(args) if !args.trim().is_empty() => serde_json::from_str(args),
        _ => Ok(Value::Object(serde_json::Map::new())),
    }
}

fn validate_arguments(tool: &lm_bridge::tool::Tool, args: &Value) -> Result<(), AgentError> {
    let validator = jsonschema::Validator::new(&tool.parameters)
        .map_err(|e| AgentError::InvalidSchema(e.to_string()))?;
    if let Err(e) = validator.validate(args) {
        return Err(AgentError::InvalidArguments(e.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::types::ToolEvent;
    use crate::permissions::PermissionMode;
    use crate::tools::ToolRegistry;
    use crate::tools::builtin::BashTool;
    use lm_bridge::message::ToolCall;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn test_ctx() -> ExecutableToolContext {
        ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::env::current_dir().unwrap(),
            allowed_paths: HashSet::new(),
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
            background: Arc::new(crate::background::BackgroundManager::new()),
            cron: Arc::new(crate::cron::CronManager::new()),
            goal_manager: Arc::new(Mutex::new(crate::goal::GoalManager::new())),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_unknown_tool_name_is_preserved_in_events() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(BashTool::new()));
        let registry = Arc::new(Mutex::new(registry));
        let ctx = test_ctx();
        let permissions =
            crate::permissions::PermissionEngine::new(PermissionMode::BypassPermissions);
        let cancel = CancellationToken::new();

        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let sender = Arc::new(move |event: ToolEvent| {
            captured.lock().unwrap().push(event);
        }) as ToolEventSender;

        let calls = vec![ToolCall::new("tc-1", "nonexistent_tool").with_arguments("{}")];
        let mut dedup = ToolCallDeduplicator::new();
        let _ = run_tool_call_batch(
            calls,
            registry,
            &ctx,
            &permissions,
            &cancel,
            Some(sender),
            &mut dedup,
        )
        .await
        .unwrap();

        let names: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                ToolEvent::Call { name, .. } => name.clone(),
                ToolEvent::Result { .. } => "<result>".to_string(),
                _ => panic!("unexpected event"),
            })
            .collect();
        assert!(names.contains(&"nonexistent_tool".to_string()));
        assert!(!names.contains(&"<unknown>".to_string()));
    }

    /// A tool whose schema requires `entity_name` and that takes `name` for
    /// it, recording what it runs with.
    struct RenamingTool {
        definition: lm_bridge::tool::Tool,
        ran_with: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait::async_trait]
    impl ExecutableTool for RenamingTool {
        fn tool_definition(&self) -> &lm_bridge::tool::Tool {
            &self.definition
        }

        fn normalize_arguments(&self, input: Value) -> Value {
            let Value::Object(mut arguments) = input else {
                return input;
            };
            if let Some(name) = arguments.remove("name") {
                arguments.insert("entity_name".into(), name);
            }
            Value::Object(arguments)
        }

        fn accesses(&self, _input: &Value) -> ToolAccesses {
            ToolAccesses::default()
        }

        fn approval_rule(&self, _input: &Value) -> &str {
            "lookup"
        }

        async fn execute(
            &self,
            input: Value,
            _ctx: &ExecutableToolContext,
            _tool_call_id: &str,
        ) -> ExecutableToolResult {
            self.ran_with.lock().unwrap().push(input);
            ExecutableToolResult::text("found")
        }
    }

    /// An argument a tool takes under another name is renamed before the call
    /// is checked against the schema: the schema requires it under its own
    /// name, and the call is not refused for it. A server's alias table
    /// (`name` for Devscriptor's `entity_name`) was applied only once a call
    /// had passed that check, so it never rescued a required argument.
    #[tokio::test]
    async fn an_argument_takes_the_tools_own_name_before_it_is_checked() {
        let ran_with = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(RenamingTool {
            definition: lm_bridge::tool::Tool::new(
                "lookup",
                "Look an entity up",
                json!({
                    "type": "object",
                    "properties": {"entity_name": {"type": "string"}},
                    "required": ["entity_name"]
                }),
            ),
            ran_with: ran_with.clone(),
        }));
        let permissions =
            crate::permissions::PermissionEngine::new(PermissionMode::BypassPermissions);
        let calls = vec![ToolCall::new("tc-1", "lookup").with_arguments(r#"{"name": "Ledger"}"#)];
        let outcome = run_tool_call_batch(
            calls,
            Arc::new(Mutex::new(registry)),
            &test_ctx(),
            &permissions,
            &CancellationToken::new(),
            None,
            &mut ToolCallDeduplicator::new(),
        )
        .await
        .unwrap();

        assert!(outcome.failed.is_empty(), "{:?}", outcome.messages);
        assert_eq!(
            *ran_with.lock().unwrap(),
            [json!({"entity_name": "Ledger"})]
        );
    }

    fn bash_registry() -> Arc<Mutex<ToolRegistry>> {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(BashTool::new()));
        Arc::new(Mutex::new(registry))
    }

    async fn run_batch(
        calls: Vec<ToolCall>,
        dedup: &mut ToolCallDeduplicator,
        sender: Option<ToolEventSender>,
    ) -> ToolBatchOutcome {
        let ctx = test_ctx();
        let permissions =
            crate::permissions::PermissionEngine::new(PermissionMode::BypassPermissions);
        let cancel = CancellationToken::new();
        run_tool_call_batch(
            calls,
            bash_registry(),
            &ctx,
            &permissions,
            &cancel,
            sender,
            dedup,
        )
        .await
        .unwrap()
    }

    /// A tool repeated within one step runs once; both calls get the result,
    /// each keyed to its own `tool_call_id` so the provider's exchange stays
    /// well-formed.
    #[tokio::test]
    async fn same_step_duplicate_executes_the_tool_once() {
        let side_effect = std::env::temp_dir().join(format!("rusta-dedup-{}", std::process::id()));
        let _ = std::fs::remove_file(&side_effect);
        let command = format!("echo x >> '{}'", side_effect.display());
        let args = serde_json::to_string(&json!({ "command": command })).unwrap();

        let calls = vec![
            ToolCall::new("tc-1", "bash").with_arguments(&args),
            ToolCall::new("tc-2", "bash").with_arguments(&args),
        ];
        let mut dedup = ToolCallDeduplicator::new();
        let outcome = run_batch(calls, &mut dedup, None).await;

        assert_eq!(outcome.messages.len(), 2, "both calls get a result message");
        let ids: Vec<_> = outcome
            .messages
            .iter()
            .map(|m| m.tool_call_id.clone().unwrap())
            .collect();
        assert_eq!(ids, vec!["tc-1", "tc-2"], "provider order preserved");
        assert_eq!(
            outcome.messages[0].text_content(),
            outcome.messages[1].text_content(),
            "duplicate inherits the original's result"
        );

        let appended = std::fs::read_to_string(&side_effect).unwrap_or_default();
        assert_eq!(appended.lines().count(), 1, "the tool ran exactly once");
        let _ = std::fs::remove_file(&side_effect);
    }

    /// The duplicate must emit its own Call/Result pair, or a UI pairing events
    /// by id leaves it spinning forever. It never reaches `Running`, because it
    /// never executes.
    #[tokio::test]
    async fn same_step_duplicate_still_emits_events() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let sender = Arc::new(move |event: ToolEvent| {
            captured.lock().unwrap().push(event);
        }) as ToolEventSender;

        let args = r#"{"command":"echo hi"}"#;
        let calls = vec![
            ToolCall::new("tc-1", "bash").with_arguments(args),
            ToolCall::new("tc-2", "bash").with_arguments(args),
        ];
        let mut dedup = ToolCallDeduplicator::new();
        run_batch(calls, &mut dedup, Some(sender)).await;

        let events = events.lock().unwrap();
        let dup_call_statuses: Vec<ToolStatus> = events
            .iter()
            .filter_map(|e| match e {
                ToolEvent::Call { id, status, .. } if id == "tc-2" => Some(*status),
                _ => None,
            })
            .collect();
        let dup_results = events
            .iter()
            .filter(|e| matches!(e, ToolEvent::Result { id, .. } if id == "tc-2"))
            .count();
        let dup_progress = events
            .iter()
            .filter(|e| matches!(e, ToolEvent::Progress { id, .. } if id == "tc-2"))
            .count();

        assert_eq!(
            dup_call_statuses,
            vec![ToolStatus::Completed],
            "the duplicate is announced already resolved, not pending"
        );
        assert_eq!(dup_results, 1, "duplicate resolved");
        assert_eq!(
            dup_progress, 0,
            "the duplicate never ran, so it never reported progress"
        );
    }

    /// Argument key order differs between providers; it must not defeat dedup.
    #[tokio::test]
    async fn dedup_ignores_argument_key_order() {
        let side_effect =
            std::env::temp_dir().join(format!("rusta-dedup-order-{}", std::process::id()));
        let _ = std::fs::remove_file(&side_effect);
        let command =
            serde_json::to_string(&format!("echo x >> '{}'", side_effect.display())).unwrap();

        // Same call, same timeout, different serialization order.
        let calls = vec![
            ToolCall::new("tc-1", "bash")
                .with_arguments(format!(r#"{{"command":{command},"timeout":5}}"#)),
            ToolCall::new("tc-2", "bash")
                .with_arguments(format!(r#"{{"timeout":5,"command":{command}}}"#)),
        ];
        let mut dedup = ToolCallDeduplicator::new();
        let outcome = run_batch(calls, &mut dedup, None).await;

        assert_eq!(outcome.messages.len(), 2);
        let appended = std::fs::read_to_string(&side_effect).unwrap_or_default();
        assert_eq!(
            appended.lines().count(),
            1,
            "key order must not make one call look like two"
        );
        let _ = std::fs::remove_file(&side_effect);
    }

    /// Across steps, the third identical call carries a redirecting reminder.
    #[tokio::test]
    async fn cross_step_repeat_appends_reminder_on_the_third_call() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = r#"{"command":"echo hi"}"#;

        for step in 1..=2 {
            let calls = vec![ToolCall::new(format!("tc-{step}"), "bash").with_arguments(args)];
            let outcome = run_batch(calls, &mut dedup, None).await;
            assert!(
                !outcome.messages[0]
                    .text_content()
                    .contains("system-reminder"),
                "no reminder at streak {step}"
            );
            assert!(!outcome.stop_turn);
        }

        let calls = vec![ToolCall::new("tc-3", "bash").with_arguments(args)];
        let outcome = run_batch(calls, &mut dedup, None).await;
        assert!(
            outcome.messages[0]
                .text_content()
                .contains("what new information you expect")
        );
        assert!(!outcome.stop_turn, "a nudge does not end the turn");
    }

    /// A model that will not stop gets the turn taken away from it.
    #[tokio::test]
    async fn twelfth_consecutive_repeat_stops_the_turn() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = r#"{"command":"echo hi"}"#;

        for step in 1..=11 {
            let calls = vec![ToolCall::new(format!("tc-{step}"), "bash").with_arguments(args)];
            let outcome = run_batch(calls, &mut dedup, None).await;
            assert!(!outcome.stop_turn, "still running at streak {step}");
        }

        let calls = vec![ToolCall::new("tc-12", "bash").with_arguments(args)];
        let outcome = run_batch(calls, &mut dedup, None).await;
        assert!(outcome.stop_turn, "streak 12 force-stops the turn");
        assert!(
            outcome.messages[0]
                .text_content()
                .contains("Write your final response now")
        );
    }

    /// Distinct calls must never be collapsed or nudged.
    #[tokio::test]
    async fn distinct_calls_are_untouched() {
        let mut dedup = ToolCallDeduplicator::new();
        for step in 1..=6 {
            let command = format!("echo {step}");
            let args = serde_json::to_string(&json!({ "command": command })).unwrap();
            let calls = vec![ToolCall::new(format!("tc-{step}"), "bash").with_arguments(&args)];
            let outcome = run_batch(calls, &mut dedup, None).await;
            let text = outcome.messages[0].text_content();
            assert!(!text.contains("system-reminder"), "step {step}: {text}");
            assert!(text.contains(&step.to_string()));
            assert!(!outcome.stop_turn);
        }
    }

    /// A PreToolUse hook that exits 2 refuses the call before it runs, with the
    /// hook's stderr as the reason the model sees.
    #[tokio::test]
    async fn pre_tool_use_hook_block_refuses_the_call() {
        let side_effect =
            std::env::temp_dir().join(format!("rusta-hook-block-{}", std::process::id()));
        let _ = std::fs::remove_file(&side_effect);
        let command = "echo denied-by-hook 1>&2; exit 2".to_string();
        let hooks = crate::user_hooks::UserHooks::new(
            vec![crate::user_hooks::UserHookRule {
                event: crate::user_hooks::UserHookEvent::PreToolUse,
                matcher: Some("^bash$".to_string()),
                command,
                timeout: Some(5),
            }],
            "session_test",
            "Hooks test",
            "rusta_cli",
            std::env::temp_dir(),
        )
        .unwrap();
        let ctx = ExecutableToolContext {
            user_hooks: Some(Arc::new(hooks)),
            ..test_ctx()
        };
        let permissions =
            crate::permissions::PermissionEngine::new(PermissionMode::BypassPermissions);
        let call = format!("echo x >> '{}'", side_effect.display());
        let args = serde_json::to_string(&json!({ "command": call })).unwrap();
        let calls = vec![ToolCall::new("tc-1", "bash").with_arguments(&args)];
        let mut dedup = ToolCallDeduplicator::new();
        let outcome = run_tool_call_batch(
            calls,
            bash_registry(),
            &ctx,
            &permissions,
            &CancellationToken::new(),
            None,
            &mut dedup,
        )
        .await
        .unwrap();
        assert_eq!(outcome.messages.len(), 1);
        let text = outcome.messages[0].text_content();
        assert!(
            text.contains("Blocked by hook: denied-by-hook"),
            "the model is told why: {text}"
        );
        assert!(!side_effect.exists(), "a blocked call must never execute");
    }

    /// Counts the approval requests it receives and allows each one.
    struct CountingApprovals(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl crate::permissions::ApprovalHook for CountingApprovals {
        async fn request_approval(
            &self,
            _request: crate::permissions::ApprovalRequest,
        ) -> ApprovalResponse {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ApprovalResponse::AllowSession
        }
    }

    /// A PreToolUse block is final: the user is never asked to approve the
    /// blocked call, no permission hook fires for it, and no answer becomes
    /// a session rule.
    #[tokio::test]
    async fn a_pre_tool_use_block_never_asks_for_approval() {
        let dir = tempfile::tempdir().unwrap();
        let permission_log = dir.path().join("permission-hooks.log");
        let rule = |event, command: String| crate::user_hooks::UserHookRule {
            event,
            matcher: None,
            command,
            timeout: Some(5),
        };
        let hooks = crate::user_hooks::UserHooks::new(
            vec![
                rule(
                    crate::user_hooks::UserHookEvent::PreToolUse,
                    "echo blocked-by-hook 1>&2; exit 2".to_string(),
                ),
                rule(
                    crate::user_hooks::UserHookEvent::PermissionRequest,
                    format!("echo request >> '{}'", permission_log.display()),
                ),
                rule(
                    crate::user_hooks::UserHookEvent::PermissionResult,
                    format!("echo result >> '{}'", permission_log.display()),
                ),
            ],
            "session_test",
            "Hooks test",
            "rusta_cli",
            dir.path().to_path_buf(),
        )
        .unwrap();
        let ctx = ExecutableToolContext {
            user_hooks: Some(Arc::new(hooks)),
            ..test_ctx()
        };
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let permissions = crate::permissions::PermissionEngine::with_hook(
            PermissionMode::Default,
            Arc::new(CountingApprovals(Arc::clone(&asked))),
        );
        let args = serde_json::to_string(&json!({ "command": "echo hi" })).unwrap();
        let calls = vec![ToolCall::new("tc-1", "bash").with_arguments(&args)];
        let mut dedup = ToolCallDeduplicator::new();
        let outcome = run_tool_call_batch(
            calls,
            bash_registry(),
            &ctx,
            &permissions,
            &CancellationToken::new(),
            None,
            &mut dedup,
        )
        .await
        .unwrap();

        let text = outcome.messages[0].text_content();
        assert!(text.contains("Blocked by hook: blocked-by-hook"), "{text}");
        assert_eq!(
            asked.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the user was asked to approve a call the hook had blocked"
        );
        assert!(
            !permissions.is_allowed("run_command", &json!({}), &ctx),
            "no session rule was recorded for the blocked call"
        );
        // Permission hooks are spawned; give a wrongly spawned one time to run.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !permission_log.exists(),
            "a permission hook fired for a blocked call"
        );
    }

    /// An allowing PreToolUse hook does not disturb the call.
    #[tokio::test]
    async fn pre_tool_use_hook_allow_runs_the_call() {
        let hooks = crate::user_hooks::UserHooks::new(
            vec![crate::user_hooks::UserHookRule {
                event: crate::user_hooks::UserHookEvent::PreToolUse,
                matcher: None,
                command: "true".to_string(),
                timeout: Some(5),
            }],
            "session_test",
            "Hooks test",
            "rusta_cli",
            std::env::temp_dir(),
        )
        .unwrap();
        let ctx = ExecutableToolContext {
            user_hooks: Some(Arc::new(hooks)),
            ..test_ctx()
        };
        let permissions =
            crate::permissions::PermissionEngine::new(PermissionMode::BypassPermissions);
        let calls =
            vec![ToolCall::new("tc-1", "bash").with_arguments(r#"{"command":"echo fine"}"#)];
        let mut dedup = ToolCallDeduplicator::new();
        let outcome = run_tool_call_batch(
            calls,
            bash_registry(),
            &ctx,
            &permissions,
            &CancellationToken::new(),
            None,
            &mut dedup,
        )
        .await
        .unwrap();
        assert_eq!(outcome.messages[0].text_content(), "fine");
    }

    /// A blocking PostToolUse (observation-only) cannot change anything.
    #[tokio::test]
    async fn post_tool_use_hook_block_is_ignored() {
        let hooks = crate::user_hooks::UserHooks::new(
            vec![crate::user_hooks::UserHookRule {
                event: crate::user_hooks::UserHookEvent::PostToolUse,
                matcher: None,
                command: "echo nope 1>&2; exit 2".to_string(),
                timeout: Some(5),
            }],
            "session_test",
            "Hooks test",
            "rusta_cli",
            std::env::temp_dir(),
        )
        .unwrap();
        let ctx = ExecutableToolContext {
            user_hooks: Some(Arc::new(hooks)),
            ..test_ctx()
        };
        let permissions =
            crate::permissions::PermissionEngine::new(PermissionMode::BypassPermissions);
        let calls =
            vec![ToolCall::new("tc-1", "bash").with_arguments(r#"{"command":"echo fine"}"#)];
        let mut dedup = ToolCallDeduplicator::new();
        let outcome = run_tool_call_batch(
            calls,
            bash_registry(),
            &ctx,
            &permissions,
            &CancellationToken::new(),
            None,
            &mut dedup,
        )
        .await
        .unwrap();
        assert_eq!(outcome.messages[0].text_content(), "fine");
    }

    /// A call whose arguments are not valid JSON — typically cut off mid-stream
    /// — is answered with an error of its own; the other calls in the step
    /// still run, and the batch does not fail.
    #[tokio::test]
    async fn malformed_arguments_fail_only_their_own_call() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(BashTool::new()));
        let registry = Arc::new(Mutex::new(registry));
        let ctx = test_ctx();
        let permissions =
            crate::permissions::PermissionEngine::new(PermissionMode::BypassPermissions);
        let calls = vec![
            ToolCall::new("tc-1", "bash").with_arguments(r#"{"command": "echo ok"#),
            ToolCall::new("tc-2", "bash").with_arguments(r#"{"command": "echo fine"}"#),
            ToolCall::new("tc-3", "").with_arguments("{}"),
        ];
        let mut dedup = ToolCallDeduplicator::new();
        let outcome = run_tool_call_batch(
            calls,
            registry,
            &ctx,
            &permissions,
            &CancellationToken::new(),
            None,
            &mut dedup,
        )
        .await
        .expect("a malformed call must not fail the batch");
        assert_eq!(outcome.messages.len(), 3);
        let first = outcome.messages[0].text_content();
        assert!(first.contains("not valid JSON"), "{first}");
        let second = outcome.messages[1].text_content();
        assert!(second.contains("fine"), "{second}");
        let third = outcome.messages[2].text_content();
        assert!(third.contains("no tool name"), "{third}");
    }

    /// A tool's oversized output reaches the model bounded, with a pointer to
    /// the saved full text — and a repeated-call reminder still survives.
    #[tokio::test]
    async fn oversized_tool_output_is_bounded_before_it_enters_the_context() {
        let spill = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(BashTool::new()));
        let registry = Arc::new(Mutex::new(registry));
        let mut ctx = test_ctx();
        ctx.tool_output = crate::tool_output::ToolOutputPolicy::new(2_000, spill.path());
        let permissions =
            crate::permissions::PermissionEngine::new(PermissionMode::BypassPermissions);
        let calls =
            vec![ToolCall::new("tc-big", "bash").with_arguments(r#"{"command": "seq 1 20000"}"#)];
        let mut dedup = ToolCallDeduplicator::new();
        let outcome = run_tool_call_batch(
            calls,
            registry,
            &ctx,
            &permissions,
            &CancellationToken::new(),
            None,
            &mut dedup,
        )
        .await
        .unwrap();
        let text = outcome.messages[0].text_content();
        assert!(text.chars().count() < 3_000, "{}", text.chars().count());
        assert!(text.contains("characters omitted"));
        let saved = std::fs::read_to_string(spill.path().join("tc-big.txt")).unwrap();
        assert!(saved.contains("\n20000"), "the full output is saved");
    }
}
