use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

pub struct TaskListTool {
    tool: Tool,
}

impl TaskListTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "task_list",
                "List all background tasks created by the bash tool with run_in_background=true.",
                json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            ),
        }
    }
}

impl Default for TaskListTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for TaskListTool {
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
        _input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let tasks = ctx.background.list();
        if tasks.is_empty() {
            ExecutableToolResult::text("No background tasks.")
        } else {
            ExecutableToolResult::text(tasks.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[tokio::test]
    async fn test_task_list_empty() {
        let tool = TaskListTool::new();
        let result = tool.execute(json!({}), &ctx(), "test-id").await;
        assert!(!result.is_error);
        assert!(result.text_content().contains("No background tasks"));
    }

    #[tokio::test]
    async fn test_task_list_shows_task() {
        let ctx = ctx();
        ctx.background.spawn(
            "echo listed",
            std::env::current_dir().unwrap(),
            std::collections::HashMap::new(),
            None,
        );
        let tool = TaskListTool::new();
        let result = tool.execute(json!({}), &ctx, "test-id").await;
        assert!(!result.is_error);
        assert!(result.text_content().contains("task-"));
        assert!(result.text_content().contains("listed"));
    }
}
