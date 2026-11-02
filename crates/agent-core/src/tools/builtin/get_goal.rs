use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

pub struct GetGoalTool {
    tool: Tool,
}

impl GetGoalTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "get_goal",
                "Return the currently active goal, including its status, objective, and budget usage.",
                json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            ),
        }
    }
}

impl Default for GetGoalTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for GetGoalTool {
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
        _args: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let manager = ctx.goal_manager.lock().unwrap();
        match manager.active_summary() {
            Some(summary) => ExecutableToolResult::text(summary),
            None => ExecutableToolResult::text("No active goal.".to_string()),
        }
    }
}
