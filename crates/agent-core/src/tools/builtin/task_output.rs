use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

pub struct TaskOutputTool {
    tool: Tool,
}

impl TaskOutputTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "task_output",
                "Return the captured stdout/stderr of a background task.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Task id" },
                        "offset": { "type": "integer", "description": "0-based line offset" },
                        "limit": { "type": "integer", "description": "Maximum lines to return" }
                    },
                    "required": ["id"]
                }),
            ),
        }
    }
}

impl Default for TaskOutputTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for TaskOutputTool {
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
        let id = input.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            return ExecutableToolResult::error("id is required");
        }
        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        match ctx.background.output(id, offset, limit).await {
            Some(out) => ExecutableToolResult::text(out),
            None => ExecutableToolResult::error(format!("Task {id} not found")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> ExecutableToolContext {
        let background = std::sync::Arc::new(crate::background::BackgroundManager::new());
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
            background,
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

    #[tokio::test]
    async fn test_task_output_returns_output() {
        let ctx = ctx();
        let id = ctx.background.spawn(
            "echo hello",
            std::env::current_dir().unwrap(),
            std::collections::HashMap::new(),
            None,
        );
        for _ in 0..50 {
            if ctx
                .background
                .output(&id, 0, None)
                .await
                .map(|o| o.contains("hello"))
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let tool = TaskOutputTool::new();
        let result = tool.execute(json!({"id": id}), &ctx, "test-id").await;
        assert!(!result.is_error);
        assert!(result.text_content().contains("hello"));
    }

    #[tokio::test]
    async fn test_task_output_errors_for_missing_task() {
        let ctx = ctx();
        let tool = TaskOutputTool::new();
        let result = tool
            .execute(json!({"id": "task-does-not-exist"}), &ctx, "test-id")
            .await;
        assert!(result.is_error);
    }
}
