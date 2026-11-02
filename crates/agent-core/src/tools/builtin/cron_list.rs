//! `cron_list` — list every cron job scheduled in this session.
//!
//! Read-only and silent on the approval gate: safe in plan mode. Records are
//! sorted by next fire time.

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};
use crate::cron::{
    CRON_MAIN_AGENT_ONLY, CronTask, cron_to_human, format_local_iso_with_offset, now_millis,
    parse_cron_expression,
};

const DESCRIPTION: &str = include_str!("cron_list.md");

const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
const PROMPT_PREVIEW_BYTES: usize = 200;

pub struct CronListTool {
    tool: Tool,
}

impl CronListTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "cron_list",
                DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            ),
        }
    }
}

impl Default for CronListTool {
    fn default() -> Self {
        Self::new()
    }
}

/// JSON-encode the prompt so embedded newlines stay on one line, cut to 200
/// UTF-8 bytes without splitting a character.
fn preview_prompt(prompt: &str) -> String {
    let encoded = serde_json::to_string(prompt).unwrap_or_else(|_| "\"\"".to_string());
    if encoded.len() <= PROMPT_PREVIEW_BYTES {
        return encoded;
    }
    let mut end = PROMPT_PREVIEW_BYTES;
    while end > 0 && !encoded.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated)", &encoded[..end])
}

#[async_trait]
impl ExecutableTool for CronListTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
        ToolAccesses::none()
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        // Read-only: safe in plan mode, and nothing to ask about.
        "silent"
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        if ctx.depth > 0 {
            return ExecutableToolResult::error(CRON_MAIN_AGENT_ONLY);
        }
        let tasks = ctx.cron.list();
        let now = now_millis();
        let header = format!("cron_jobs: {}", tasks.len());
        if tasks.is_empty() {
            return ExecutableToolResult::text(format!("{header}\nNo cron jobs scheduled."));
        }

        // Sorted by next fire time, `null` last.
        let mut with_fire: Vec<(CronTask, Option<i64>)> = tasks
            .into_iter()
            .map(|task| (task.clone(), ctx.cron.next_fire_for(&task, now)))
            .collect();
        with_fire.sort_by_key(|(_, next)| next.unwrap_or(i64::MAX));

        let records: Vec<String> = with_fire
            .iter()
            .map(|(task, next)| render_record(ctx, task, *next, now))
            .collect();
        ExecutableToolResult::text(format!("{header}\n{}", records.join("\n---\n")))
    }
}

fn render_record(
    ctx: &ExecutableToolContext,
    task: &CronTask,
    next: Option<i64>,
    now: i64,
) -> String {
    let mut human_schedule = task.cron.clone();
    let mut next_fire_iso = "null".to_string();
    if let Ok(parsed) = parse_cron_expression(&task.cron) {
        human_schedule = cron_to_human(&parsed);
        if let Some(next) = next {
            next_fire_iso = format_local_iso_with_offset(next);
        }
    }
    let age_days = (now - task.created_at) as f64 / MS_PER_DAY as f64;
    let stale = ctx.cron.is_stale_at(task, now);
    [
        format!("id: {}", task.id),
        format!("cron: {}", task.cron),
        format!("humanSchedule: {human_schedule}"),
        format!("prompt: {}", preview_prompt(&task.prompt)),
        format!("nextFireAt: {next_fire_iso}"),
        format!("recurring: {}", task.recurring),
        format!("ageDays: {age_days:.2}"),
        format!("stale: {stale}"),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ExecutableToolContext {
        ExecutableToolContext::default()
    }

    /// `cron_list` is read-only, so it never sits on the approval gate —
    /// unlike create/delete.
    #[test]
    fn list_is_silent_on_the_approval_gate() {
        assert_eq!(
            CronListTool::new().approval_rule(&serde_json::json!({})),
            "silent"
        );
    }

    /// Sorted by next fire time rather than insertion order: the task firing
    /// soonest leads, one with no upcoming fire
    /// (a paused-far-future schedule still parses, so give it a yearly cron
    /// and compare against a minutely one) trails.
    #[tokio::test]
    async fn list_is_sorted_by_next_fire_time() {
        let ctx = ctx();
        let yearly = ctx.cron.add("0 0 1 1 *", "yearly report", true).unwrap();
        let minutely = ctx.cron.add("* * * * *", "every minute", true).unwrap();

        let outcome = CronListTool::new()
            .execute(serde_json::json!({}), &ctx, "tc-1")
            .await;
        let text = outcome.text_content();
        let yearly_at = text.find(&yearly.id).unwrap();
        let minutely_at = text.find(&minutely.id).unwrap();
        assert!(
            minutely_at < yearly_at,
            "the sooner fire should lead:\n{text}"
        );
        assert!(text.starts_with("cron_jobs: 2"), "{text}");
        assert!(text.contains("---"), "records are separated:\n{text}");
        // The 200-byte JSON prompt preview keeps embedded newlines on one line.
        assert!(text.contains("\"every minute\""), "{text}");
    }

    #[tokio::test]
    async fn an_empty_session_says_so() {
        let outcome = CronListTool::new()
            .execute(serde_json::json!({}), &ctx(), "tc-1")
            .await;
        assert_eq!(
            outcome.text_content(),
            "cron_jobs: 0\nNo cron jobs scheduled."
        );
    }
}
