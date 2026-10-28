use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};
use crate::goal::GoalBudgets;

pub struct SetGoalBudgetTool {
    tool: Tool,
}

impl SetGoalBudgetTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "set_goal_budget",
                "Set optional spending limits on the active goal.",
                json!({
                    "type": "object",
                    "properties": {
                        "token_budget": {
                            "type": "integer",
                            "description": "Maximum total tokens to spend on this goal"
                        },
                        "turn_budget": {
                            "type": "integer",
                            "description": "Maximum agent turns to spend on this goal"
                        },
                        "wall_clock_budget_ms": {
                            "type": "integer",
                            "description": "Maximum wall-clock milliseconds to spend on this goal"
                        }
                    },
                    "required": []
                }),
            ),
        }
    }
}

impl Default for SetGoalBudgetTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for SetGoalBudgetTool {
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
        let budgets = GoalBudgets {
            token_budget: args.get("token_budget").and_then(|v| v.as_u64()),
            turn_budget: args
                .get("turn_budget")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32),
            wall_clock_budget_ms: args.get("wall_clock_budget_ms").and_then(|v| v.as_u64()),
        };

        let mut manager = ctx.goal_manager.lock().unwrap();
        match manager.set_budgets(budgets) {
            Ok(id) => ExecutableToolResult::text(format!("Budgets set for goal {id}.")),
            Err(e) => ExecutableToolResult::error(e),
        }
    }
}
