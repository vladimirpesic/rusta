use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

/// The tool's name, by which the conversation shows when the list changed.
pub const TODO_LIST_TOOL_NAME: &str = "todo_list";

pub struct TodoListTool {
    tool: Tool,
}

impl TodoListTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                TODO_LIST_TOOL_NAME,
                "Manage a todo list for the current task. Use this to track progress through \
                 multi-step work. Keep it in step with what you have actually done: mark an item \
                 done as soon as it is finished, before you start the next one, and make sure \
                 the list matches what you did before you write your final message.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["add", "done", "list", "clear"],
                            "description": "Action to perform"
                        },
                        "description": {
                            "type": "string",
                            "description": "Description of the new item (required for add)"
                        },
                        "id": {
                            "type": "integer",
                            "description": "Item ID (required for done)"
                        }
                    },
                    "required": ["action"]
                }),
            ),
        }
    }
}

impl Default for TodoListTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for TodoListTool {
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
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        let mut list = ctx.todo_list.lock().unwrap_or_else(|e| e.into_inner());

        match action {
            "add" => {
                let description = input
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if description.is_empty() {
                    return ExecutableToolResult::error("description is required for add");
                }
                let id = list.add(description);
                ExecutableToolResult::text(format!("Added todo {id}: {description}"))
            }
            "done" => {
                let id = input.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                if list.mark_done(id) {
                    ExecutableToolResult::text(format!("Marked todo {id} as done"))
                } else {
                    ExecutableToolResult::error(format!("Todo {id} not found"))
                }
            }
            "clear" => {
                list.clear();
                ExecutableToolResult::text("Cleared todo list")
            }
            "list" => ExecutableToolResult::text(list.format()),
            _ => ExecutableToolResult::error(format!("unknown action: {action}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TodoList;

    fn ctx() -> ExecutableToolContext {
        let background = std::sync::Arc::new(crate::background::BackgroundManager::new());
        ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::path::PathBuf::from("."),
            allowed_paths: std::collections::HashSet::new(),
            todo_list: TodoList::shared(),
            tool_event_sender: None,
            user_interaction: None,
            search_url: None,
            plan_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: crate::permissions::PermissionMode::Default,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            background: background.clone(),
            goal_manager: std::sync::Arc::new(std::sync::Mutex::new(
                crate::goal::GoalManager::new(),
            )),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            cron: std::sync::Arc::new(crate::cron::CronManager::new()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_todo_list_add_and_list() {
        let tool = TodoListTool::new();
        let ctx = ctx();

        let add = tool
            .execute(
                json!({"action": "add", "description": "write tests"}),
                &ctx,
                "test-id",
            )
            .await;
        assert!(!add.is_error);

        let list = tool
            .execute(json!({"action": "list"}), &ctx, "test-id")
            .await;
        assert!(list.text_content().contains("write tests"));
    }

    #[tokio::test]
    async fn test_todo_list_done() {
        let tool = TodoListTool::new();
        let ctx = ctx();

        tool.execute(
            json!({"action": "add", "description": "write tests"}),
            &ctx,
            "test-id",
        )
        .await;
        let done = tool
            .execute(json!({"action": "done", "id": 0}), &ctx, "test-id")
            .await;
        assert!(!done.is_error);

        let list = tool
            .execute(json!({"action": "list"}), &ctx, "test-id")
            .await;
        assert!(list.text_content().contains("[x]"));
    }
}
