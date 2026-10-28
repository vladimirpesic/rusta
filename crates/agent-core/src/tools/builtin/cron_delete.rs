//! `cron_delete` — cancel a scheduled cron job by id.
//!
//! Needs approval (`schedule_prompt`) and is refused in plan mode, on
//! sub-agents, and against ids that are not a cron job id shape.

use std::sync::LazyLock;

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use regex::Regex;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};
use crate::cron::{CRON_MAIN_AGENT_ONLY, ID_PATTERN, plan_mode_denial};

const DESCRIPTION: &str = include_str!("cron_delete.md");

/// [`ID_PATTERN`], compiled once.
static ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(ID_PATTERN).expect("valid cron id pattern"));

pub struct CronDeleteTool {
    tool: Tool,
}

impl CronDeleteTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "cron_delete",
                DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "The cron job id returned by cron_create / cron_list."
                        }
                    },
                    "required": ["id"]
                }),
            ),
        }
    }
}

impl Default for CronDeleteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for CronDeleteTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
        ToolAccesses::none()
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        "schedule_prompt"
    }

    fn preflight(
        &self,
        input: &serde_json::Value,
        ctx: &ExecutableToolContext,
    ) -> Result<(), String> {
        if ctx.depth > 0 {
            return Err(CRON_MAIN_AGENT_ONLY.to_string());
        }
        if ctx.plan_mode.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(plan_mode_denial("cron_delete"));
        }
        let id = input.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !ID_RE.is_match(id) {
            return Err(format!(
                "Invalid cron job id {id:?} — must be a cron job id."
            ));
        }
        Ok(())
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let id = input.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if ctx.cron.remove(id) {
            ExecutableToolResult::text(format!("Deleted cron job {id}."))
        } else {
            ExecutableToolResult::error(format!("No cron job with id {id}."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::tool_call::{ToolBatchOutcome, run_tool_call_batch};
    use crate::agent_loop::tool_dedup::ToolCallDeduplicator;
    use crate::permissions::{
        ApprovalHook, ApprovalRequest, ApprovalResponse, PermissionEngine, PermissionMode,
    };
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use lm_bridge::message::ToolCall;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ExecutableToolContext {
        ExecutableToolContext::default()
    }

    struct RecordingHook {
        seen: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[async_trait]
    impl ApprovalHook for RecordingHook {
        async fn request_approval(&self, request: ApprovalRequest) -> ApprovalResponse {
            self.seen
                .lock()
                .unwrap()
                .push((request.tool_name, request.rule));
            ApprovalResponse::AllowOnce
        }
    }

    async fn run(
        args: serde_json::Value,
        ctx: &ExecutableToolContext,
        permissions: &PermissionEngine,
    ) -> ToolBatchOutcome {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(CronDeleteTool::new()));
        let registry = Arc::new(Mutex::new(registry));
        let cancel = CancellationToken::new();
        let calls = vec![
            ToolCall::new("tc-1", "cron_delete")
                .with_arguments(serde_json::to_string(&args).unwrap()),
        ];
        let mut dedup = ToolCallDeduplicator::new();
        run_tool_call_batch(calls, registry, ctx, permissions, &cancel, None, &mut dedup)
            .await
            .unwrap()
    }

    /// Deleting mutates scheduled work, so it is asked about like a create.
    #[tokio::test]
    async fn delete_asks_the_schedule_prompt_rule() {
        let ctx = ctx();
        let task = ctx.cron.add("*/5 * * * *", "ping", true).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let permissions = PermissionEngine::with_hook(
            PermissionMode::Default,
            Arc::new(RecordingHook { seen: seen.clone() }),
        );

        let outcome = run(json!({"id": task.id}), &ctx, &permissions).await;
        assert!(
            outcome.failed.is_empty(),
            "{}",
            outcome.messages[0].text_content()
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("cron_delete".to_string(), "schedule_prompt".to_string())]
        );
        assert_eq!(ctx.cron.task_count(), 0);
    }

    #[tokio::test]
    async fn delete_is_refused_in_plan_mode() {
        let ctx = ExecutableToolContext {
            plan_mode: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ..ctx()
        };
        let outcome = run(
            json!({"id": "01234567"}),
            &ctx,
            &PermissionEngine::new(PermissionMode::BypassPermissions),
        )
        .await;
        assert!(
            outcome.messages[0]
                .text_content()
                .contains("cron_delete is not available in plan mode"),
            "{}",
            outcome.messages[0].text_content()
        );
    }

    #[tokio::test]
    async fn delete_is_refused_on_a_sub_agent() {
        let ctx = ExecutableToolContext { depth: 1, ..ctx() };
        let outcome = run(
            json!({"id": "01234567"}),
            &ctx,
            &PermissionEngine::new(PermissionMode::BypassPermissions),
        )
        .await;
        assert!(
            outcome.messages[0]
                .text_content()
                .contains(crate::cron::CRON_MAIN_AGENT_ONLY),
            "{}",
            outcome.messages[0].text_content()
        );
    }

    #[tokio::test]
    async fn delete_rejects_ids_that_are_not_cron_job_shapes() {
        let outcome = run(
            json!({"id": "bash"}),
            &ctx(),
            &PermissionEngine::new(PermissionMode::BypassPermissions),
        )
        .await;
        assert!(
            outcome.messages[0]
                .text_content()
                .contains("Invalid cron job id"),
            "{}",
            outcome.messages[0].text_content()
        );
    }

    /// Rusta's 8-hex ids and ULIDs pass; other text does not.
    #[test]
    fn the_id_pattern_takes_both_id_shapes() {
        assert!(ID_RE.is_match("0123abcd"));
        assert!(ID_RE.is_match("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        for id in [
            "",
            "bash",
            "0123ABCD",
            "0123abcd0",
            "01ARZ3NDEKTSV4RRFFQ69G5FAI",
        ] {
            assert!(!ID_RE.is_match(id), "{id:?}");
        }
    }

    /// A well-shaped id that matches nothing reaches execute and errors there
    /// ("No cron job with id").
    #[tokio::test]
    async fn delete_of_an_unknown_id_errors_in_execute() {
        let outcome = run(
            json!({"id": "01234567"}),
            &ctx(),
            &PermissionEngine::new(PermissionMode::BypassPermissions),
        )
        .await;
        assert!(!outcome.failed.is_empty());
        assert!(
            outcome.messages[0]
                .text_content()
                .contains("No cron job with id 01234567"),
            "{}",
            outcome.messages[0].text_content()
        );
    }
}
