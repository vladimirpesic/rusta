//! `cron_create` — schedule a prompt to be enqueued at a future time.
//!
//! The model schedules a *prompt* that is re-injected into this session when
//! idle, never a shell command. Create needs approval (`schedule_prompt`) and
//! is refused in plan mode and on sub-agents; the limits are 50 tasks, 8 KiB
//! prompts, and the 5-year and 350-day horizons.

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};
use crate::cron::{
    CRON_MAIN_AGENT_ONLY, MAX_PROMPT_BYTES, MAX_TASKS, ONE_SHOT_MAX_FUTURE_MS,
    compute_jittered_next, compute_next_cron_run, cron_to_human, disabled_message,
    format_local_iso_with_offset, has_fire_within_years, now_millis, parse_cron_expression,
    plan_mode_denial,
};

const DESCRIPTION: &str = include_str!("cron_create.md");

pub struct CronCreateTool {
    tool: Tool,
}

impl CronCreateTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "cron_create",
                DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "cron": {
                            "type": "string",
                            "description": "5-field cron expression in local time: \"M H DoM Mon DoW\" (e.g. \"*/5 * * * *\" = every 5 minutes; \"30 14 28 2 *\" = Feb 28 at 2:30pm local — a pinned date like this repeats yearly unless you also pass recurring: false)."
                        },
                        "prompt": {
                            "type": "string",
                            "minLength": 1,
                            "description": "The prompt to enqueue at each fire time. Limited to 8 KiB (UTF-8)."
                        },
                        "recurring": {
                            "type": "boolean",
                            "description": "true (default) = fire on every cron match until deleted or auto-expired after 7 days. false = fire once at the next match, then auto-delete. Use false for \"remind me at X\" one-shot requests with pinned minute/hour/dom/month."
                        }
                    },
                    "required": ["cron", "prompt"]
                }),
            ),
        }
    }

    fn normalize(input: &serde_json::Value) -> Result<(String, crate::cron::ParsedCron), String> {
        let raw = input.get("cron").and_then(|v| v.as_str()).unwrap_or("");
        let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let parsed = parse_cron_expression(&normalized)
            .map_err(|e| format!("Invalid cron expression: {e}"))?;
        Ok((normalized, parsed))
    }
}

impl Default for CronCreateTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for CronCreateTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
        ToolAccesses::none()
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        // Scheduling re-injects a prompt into the session without the user
        // typing it — that is what the approval gate is for. The defect this
        // replaces was `silent`, which never reached `is_allowed`.
        "schedule_prompt"
    }

    /// Everything validated before the approval prompt, so the user is never
    /// asked about a call that cannot run.
    fn preflight(
        &self,
        input: &serde_json::Value,
        ctx: &ExecutableToolContext,
    ) -> Result<(), String> {
        if ctx.depth > 0 {
            return Err(CRON_MAIN_AGENT_ONLY.to_string());
        }
        if ctx.plan_mode.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(plan_mode_denial("cron_create"));
        }
        if ctx.cron.is_disabled() {
            return Err(disabled_message());
        }
        let (normalized, parsed) = Self::normalize(input)?;
        let now = now_millis();
        if !has_fire_within_years(&parsed, 5, now) {
            return Err(format!(
                "Cron expression {normalized:?} has no fire within 5 years; refusing to schedule."
            ));
        }
        if ctx.cron.task_count() >= MAX_TASKS {
            return Err(format!(
                "Cron job cap reached (max {MAX_TASKS} per session)."
            ));
        }
        let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        if prompt.trim().is_empty() {
            return Err(
                "Prompt is empty: it is the message delivered to this session at each fire."
                    .to_string(),
            );
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(format!(
                "Prompt exceeds {MAX_PROMPT_BYTES} bytes (got {}).",
                prompt.len()
            ));
        }
        if input
            .get("recurring")
            .and_then(|v| v.as_bool())
            .is_some_and(|r| !r)
            && let Some(first_fire) = compute_next_cron_run(&parsed, now)
            && first_fire - now > ONE_SHOT_MAX_FUTURE_MS
        {
            return Err(format!(
                "One-shot cron {normalized:?} would not fire until {} (more than a year \
                 out). If you meant \"today\" or a near date, the pinned day/month has \
                 already passed this year — pick a future date or use wildcards.",
                format_local_iso_with_offset(first_fire)
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
        // The approval sat between preflight and here: the cap may have been
        // reached in the meantime, so it is checked again.
        if ctx.cron.task_count() >= MAX_TASKS {
            return ExecutableToolResult::error(format!(
                "Cron job cap reached (max {MAX_TASKS} per session)."
            ));
        }
        let Ok((normalized, parsed)) = Self::normalize(&input) else {
            return ExecutableToolResult::error("Invalid cron expression");
        };
        let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        let recurring = input
            .get("recurring")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let task = match ctx.cron.add(&normalized, prompt, recurring) {
            Ok(task) => task,
            Err(e) => return ExecutableToolResult::error(e),
        };
        let now = now_millis();
        let next_fire_at = compute_next_cron_run(&parsed, now)
            .map(|ideal| compute_jittered_next(&task, &parsed, ideal));
        let human_schedule = cron_to_human(&parsed);

        let mut out = String::new();
        out.push_str(&format!("id: {}\n", task.id));
        out.push_str(&format!("cron: {normalized}\n"));
        out.push_str(&format!("humanSchedule: {human_schedule}\n"));
        out.push_str(&format!("recurring: {recurring}\n"));
        out.push_str(&format!(
            "nextFireAt: {}",
            next_fire_at.map_or("null".to_string(), format_local_iso_with_offset)
        ));
        ExecutableToolResult::text(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::tool_call::{ToolBatchOutcome, run_tool_call_batch};
    use crate::agent_loop::tool_dedup::ToolCallDeduplicator;
    use crate::cron::MAX_PROMPT_BYTES;
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

    /// Records what the approval gate was asked, then allows the call.
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
        tool: Arc<dyn ExecutableTool>,
        name: &str,
        args: serde_json::Value,
        ctx: &ExecutableToolContext,
        permissions: &PermissionEngine,
    ) -> ToolBatchOutcome {
        let mut registry = ToolRegistry::new();
        registry.register(tool);
        let registry = Arc::new(Mutex::new(registry));
        let cancel = CancellationToken::new();
        let calls =
            vec![ToolCall::new("tc-1", name).with_arguments(serde_json::to_string(&args).unwrap())];
        let mut dedup = ToolCallDeduplicator::new();
        run_tool_call_batch(calls, registry, ctx, permissions, &cancel, None, &mut dedup)
            .await
            .unwrap()
    }

    fn bypass() -> PermissionEngine {
        PermissionEngine::new(PermissionMode::BypassPermissions)
    }

    #[tokio::test]
    async fn create_list_and_delete_round_trip() {
        let ctx = ctx();
        let permissions = bypass();

        let outcome = run(
            Arc::new(CronCreateTool::new()),
            "cron_create",
            json!({"cron": "*/5 * * * *", "prompt": "check the queue"}),
            &ctx,
            &permissions,
        )
        .await;
        assert!(
            outcome.failed.is_empty(),
            "{}",
            outcome.messages[0].text_content()
        );
        let created = outcome.messages[0].text_content().to_string();
        let id = created
            .lines()
            .find(|l| l.starts_with("id: "))
            .unwrap()
            .strip_prefix("id: ")
            .unwrap()
            .to_string();
        assert!(created.contains("cron: */5 * * * *"), "{created}");
        assert!(created.contains("humanSchedule:"), "{created}");
        assert!(created.contains("recurring: true"), "{created}");
        assert!(created.contains("nextFireAt: "), "{created}");

        let outcome = run(
            Arc::new(crate::tools::builtin::CronListTool::new()),
            "cron_list",
            json!({}),
            &ctx,
            &permissions,
        )
        .await;
        let listed = outcome.messages[0].text_content();
        assert!(listed.starts_with("cron_jobs: 1"), "{listed}");
        assert!(listed.contains(&format!("id: {id}")), "{listed}");
        assert!(listed.contains("stale: false"), "{listed}");

        let outcome = run(
            Arc::new(crate::tools::builtin::CronDeleteTool::new()),
            "cron_delete",
            json!({"id": id}),
            &ctx,
            &permissions,
        )
        .await;
        assert!(
            outcome.messages[0]
                .text_content()
                .contains(&format!("Deleted cron job {id}")),
            "{}",
            outcome.messages[0].text_content()
        );

        let outcome = run(
            Arc::new(crate::tools::builtin::CronListTool::new()),
            "cron_list",
            json!({}),
            &ctx,
            &permissions,
        )
        .await;
        assert_eq!(
            outcome.messages[0].text_content(),
            "cron_jobs: 0\nNo cron jobs scheduled."
        );
    }

    /// The approval gate is the point of `schedule_prompt`: a create must be
    /// asked about under the Default mode, by that rule, and only then run.
    #[tokio::test]
    async fn create_asks_the_schedule_prompt_rule() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let permissions = PermissionEngine::with_hook(
            PermissionMode::Default,
            Arc::new(RecordingHook { seen: seen.clone() }),
        );
        let ctx = ctx();

        let outcome = run(
            Arc::new(CronCreateTool::new()),
            "cron_create",
            json!({"cron": "*/5 * * * *", "prompt": "ping"}),
            &ctx,
            &permissions,
        )
        .await;
        assert!(
            outcome.failed.is_empty(),
            "{}",
            outcome.messages[0].text_content()
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("cron_create".to_string(), "schedule_prompt".to_string())]
        );
        assert_eq!(ctx.cron.task_count(), 1);
    }

    #[tokio::test]
    async fn create_is_refused_in_plan_mode_before_approval() {
        let ctx = ExecutableToolContext {
            plan_mode: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ..ctx()
        };
        let outcome = run(
            Arc::new(CronCreateTool::new()),
            "cron_create",
            json!({"cron": "*/5 * * * *", "prompt": "ping"}),
            &ctx,
            &bypass(),
        )
        .await;
        let text = outcome.messages[0].text_content();
        assert!(
            text.contains("cron_create is not available in plan mode"),
            "{text}"
        );
        assert_eq!(
            ctx.cron.task_count(),
            0,
            "a refused create schedules nothing"
        );
    }

    #[tokio::test]
    async fn create_is_refused_on_a_sub_agent() {
        let ctx = ExecutableToolContext { depth: 1, ..ctx() };
        let outcome = run(
            Arc::new(CronCreateTool::new()),
            "cron_create",
            json!({"cron": "*/5 * * * *", "prompt": "ping"}),
            &ctx,
            &bypass(),
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
    async fn create_refuses_an_expression_that_never_fires() {
        let outcome = run(
            Arc::new(CronCreateTool::new()),
            "cron_create",
            json!({"cron": "0 0 31 2 *", "prompt": "impossible"}),
            &ctx(),
            &bypass(),
        )
        .await;
        assert!(
            outcome.messages[0]
                .text_content()
                .contains("no fire within 5 years"),
            "{}",
            outcome.messages[0].text_content()
        );
    }

    /// An empty or whitespace-only prompt is refused before the user is asked
    /// to approve it: an empty one by the schema's `minLength`, a blank one by
    /// the preflight.
    #[tokio::test]
    async fn create_refuses_an_empty_prompt_before_approval() {
        for (prompt, refusal) in [
            ("", "shorter than 1 character"),
            (" \n\t ", "Prompt is empty"),
        ] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let permissions = PermissionEngine::with_hook(
                PermissionMode::Default,
                Arc::new(RecordingHook { seen: seen.clone() }),
            );
            let ctx = ctx();
            let outcome = run(
                Arc::new(CronCreateTool::new()),
                "cron_create",
                json!({"cron": "*/5 * * * *", "prompt": prompt}),
                &ctx,
                &permissions,
            )
            .await;
            let text = outcome.messages[0].text_content();
            assert!(text.contains(refusal), "{prompt:?}: {text}");
            assert!(seen.lock().unwrap().is_empty(), "{prompt:?}: never asked");
            assert_eq!(ctx.cron.task_count(), 0, "{prompt:?}");
        }
    }

    #[tokio::test]
    async fn create_refuses_a_prompt_over_8_kib() {
        let outcome = run(
            Arc::new(CronCreateTool::new()),
            "cron_create",
            json!({"cron": "*/5 * * * *", "prompt": "x".repeat(MAX_PROMPT_BYTES + 1)}),
            &ctx(),
            &bypass(),
        )
        .await;
        assert!(
            outcome.messages[0]
                .text_content()
                .contains(&format!("Prompt exceeds {MAX_PROMPT_BYTES} bytes")),
            "{}",
            outcome.messages[0].text_content()
        );
    }

    #[tokio::test]
    async fn create_refuses_the_51st_task() {
        let ctx = ctx();
        for _ in 0..crate::cron::MAX_TASKS {
            ctx.cron.add("*/5 * * * *", "ping", true).unwrap();
        }
        let outcome = run(
            Arc::new(CronCreateTool::new()),
            "cron_create",
            json!({"cron": "*/5 * * * *", "prompt": "one too many"}),
            &ctx,
            &bypass(),
        )
        .await;
        assert!(
            outcome.messages[0]
                .text_content()
                .contains("Cron job cap reached"),
            "{}",
            outcome.messages[0].text_content()
        );
        assert_eq!(ctx.cron.task_count(), crate::cron::MAX_TASKS);
    }
}
