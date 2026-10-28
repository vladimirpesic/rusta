use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};
use crate::goal::GoalStatus;

pub struct UpdateGoalTool {
    tool: Tool,
}

impl UpdateGoalTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "update_goal",
                "Set the status of the active goal. This is how you resume, complete, or block \
                 an autonomous goal. Most goal turns should not call this tool: if you finish a \
                 slice of work and more remains, just end the turn and you will be prompted to \
                 continue. Call `complete` only when all required work is done and validated — \
                 never after only a plan, summary, or partial result. Call `blocked` only for an \
                 impossible, unsafe, or contradictory objective, or after the same blocking \
                 condition repeats across at least 3 consecutive goal turns.",
                json!({
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            // `paused` is deliberately absent: pausing is the
                            // user's call, or a runtime failure. A model that
                            // can pause its own goal will pause it instead of
                            // finishing it.
                            "enum": ["active", "blocked", "complete"],
                            "description": "New status for the active goal"
                        }
                    },
                    "required": ["status"]
                }),
            ),
        }
    }
}

impl Default for UpdateGoalTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for UpdateGoalTool {
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
        args: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let status_str = args.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let status = match status_str {
            "active" => GoalStatus::Active,
            "blocked" => GoalStatus::Blocked,
            "complete" => GoalStatus::Complete,
            // Reachable only if the schema is bypassed; `paused` belongs to the
            // user and to runtime failures, never to the model.
            "paused" => {
                return ExecutableToolResult::error(
                    "a goal cannot be paused by the model; use `blocked` if you cannot proceed",
                );
            }
            _ => return ExecutableToolResult::error(format!("unknown status: {status_str}")),
        };

        let mut manager = ctx.goal_manager.lock().unwrap();
        match manager.update(status) {
            Ok(id) => ExecutableToolResult::text(format!("Goal {id} updated to {status}.")),
            Err(e) => ExecutableToolResult::error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn ctx() -> ExecutableToolContext {
        ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::env::current_dir().unwrap(),
            allowed_paths: std::collections::HashSet::new(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            user_interaction: None,
            search_url: None,
            plan_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: crate::permissions::PermissionMode::Default,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            background: std::sync::Arc::new(crate::background::BackgroundManager::new()),
            cron: std::sync::Arc::new(crate::cron::CronManager::new()),
            goal_manager: std::sync::Arc::new(std::sync::Mutex::new(
                crate::goal::GoalManager::new(),
            )),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            ..Default::default()
        }
    }

    fn status_enum() -> Vec<String> {
        let tool = UpdateGoalTool::new();
        tool.tool_definition().parameters["properties"]["status"]["enum"]
            .as_array()
            .expect("status must be an enum")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    /// A model that can pause its own goal will pause it rather than finish it.
    /// Pausing belongs to the user (`/goal pause`) and to runtime failures.
    #[test]
    fn the_model_cannot_pause_a_goal() {
        let statuses = status_enum();
        assert!(
            !statuses.contains(&"paused".to_string()),
            "`paused` must not be offered to the model, got {statuses:?}"
        );
        assert_eq!(statuses, vec!["active", "blocked", "complete"]);
    }

    /// The schema rejects `paused`, and so does the executor — a tool call that
    /// slipped past validation must not quietly pause the goal.
    #[tokio::test]
    async fn executing_a_paused_update_is_an_error() {
        let tool = UpdateGoalTool::new();
        let validator =
            jsonschema::Validator::new(&tool.tool_definition().parameters).expect("valid schema");
        let paused: Value = serde_json::json!({"status": "paused"});
        assert!(
            validator.validate(&paused).is_err(),
            "schema must reject `paused`"
        );

        let result = tool.execute(paused, &ctx(), "tc-1").await;
        assert!(result.is_error);
        assert!(
            result
                .text_content()
                .contains("cannot be paused by the model"),
            "got {}",
            result.text_content()
        );
    }
}
