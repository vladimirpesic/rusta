use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

pub struct CreateGoalTool {
    tool: Tool,
}

impl CreateGoalTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "create_goal",
                "Create a new active goal for the agent to pursue across multiple turns. Only one goal may be active at a time.",
                json!({
                    "type": "object",
                    "properties": {
                        "objective": {
                            "type": "string",
                            "description": "Clear statement of what the agent should achieve"
                        },
                        "completion_criterion": {
                            "type": "string",
                            "description": "Optional signal that tells the agent when the goal is complete"
                        },
                        "replace": {
                            "type": "boolean",
                            "description": "If true, replace any existing active goal. If false and a goal exists, the call fails.",
                            "default": false
                        }
                    },
                    "required": ["objective"]
                }),
            ),
        }
    }
}

impl Default for CreateGoalTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for CreateGoalTool {
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
        let objective = args.get("objective").and_then(|v| v.as_str()).unwrap_or("");
        if objective.is_empty() {
            return ExecutableToolResult::error("objective is required");
        }
        let completion_criterion = args
            .get("completion_criterion")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let replace = args
            .get("replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut manager = ctx.goal_manager.lock().unwrap();
        match manager.create(objective, completion_criterion, replace) {
            Ok(id) => ExecutableToolResult::text(format!(
                "Created goal {id}: {objective}. I will work toward this objective across turns until you update it to complete, paused, or blocked."
            )),
            Err(e) => ExecutableToolResult::error(e),
        }
    }
}
