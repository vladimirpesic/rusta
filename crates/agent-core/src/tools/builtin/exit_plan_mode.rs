//! `exit_plan_mode` — the user's review of a plan, and the way out of plan
//! mode.
//!
//! Only the user turns plan mode on, and every plan waits for their review, in
//! every permission mode. The review is the approval gate itself
//! ([`crate::permissions::PLAN_REVIEW_RULE`]), so the tool runs only once the
//! user has approved the plan. There is no plan file: the plan is the call's
//! argument.

use std::sync::atomic::Ordering;

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};
use crate::permissions::PLAN_REVIEW_RULE;

const DESCRIPTION: &str = include_str!("exit_plan_mode.md");

/// The call outside plan mode, which only the user can turn on.
const NOT_IN_PLAN_MODE: &str = "exit_plan_mode can only be called while plan mode is active. \
     Only the user can turn plan mode on.";

const REJECTED: &str = "The user rejected this plan, and plan mode is still on. Do not carry it \
     out. Find out what they want changed, revise the plan, and call exit_plan_mode again.";

const NOBODY_TO_REVIEW: &str = "Nobody can review this plan in this run, so plan mode stays on \
     for the rest of the run and files cannot be edited.";

const APPROVED: &str = "The user approved the plan, and plan mode is off. Carry the plan out \
     now under the normal tool and permission rules.";

pub struct ExitPlanModeTool {
    tool: Tool,
}

impl ExitPlanModeTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                PLAN_REVIEW_RULE,
                DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "plan": {
                            "type": "string",
                            "description": "The plan, in Markdown, as the user should review it."
                        }
                    },
                    "required": ["plan"]
                }),
            ),
        }
    }
}

impl Default for ExitPlanModeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for ExitPlanModeTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
        ToolAccesses::none()
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        PLAN_REVIEW_RULE
    }

    /// Nothing to review outside plan mode, or without a plan.
    fn preflight(
        &self,
        input: &serde_json::Value,
        ctx: &ExecutableToolContext,
    ) -> Result<(), String> {
        if !ctx.plan_mode.load(Ordering::SeqCst) {
            return Err(NOT_IN_PLAN_MODE.to_string());
        }
        let plan = input.get("plan").and_then(|v| v.as_str()).unwrap_or("");
        if plan.trim().is_empty() {
            return Err("Pass the plan to review as `plan`.".to_string());
        }
        Ok(())
    }

    fn refusal(&self, asked: bool) -> Option<&'static str> {
        Some(if asked { REJECTED } else { NOBODY_TO_REVIEW })
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        ctx.plan_mode.store(false, Ordering::SeqCst);
        ExecutableToolResult::text(APPROVED)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;

    fn ctx(plan_mode: bool) -> ExecutableToolContext {
        ExecutableToolContext {
            plan_mode: Arc::new(AtomicBool::new(plan_mode)),
            ..Default::default()
        }
    }

    /// The user cannot be asked to review a plan outside plan mode, or an
    /// empty one: the call is refused before the review.
    #[test]
    fn a_call_outside_plan_mode_or_without_a_plan_is_refused_before_review() {
        let tool = ExitPlanModeTool::new();
        let plan = json!({ "plan": "## Context\nFix the parser." });
        assert_eq!(
            tool.preflight(&plan, &ctx(false)),
            Err(NOT_IN_PLAN_MODE.to_string())
        );
        assert!(
            tool.preflight(&json!({ "plan": "  \n" }), &ctx(true))
                .is_err()
        );
        assert_eq!(tool.preflight(&plan, &ctx(true)), Ok(()));
    }

    /// The tool runs once the user has approved the plan: plan mode ends.
    #[tokio::test]
    async fn an_approved_plan_ends_plan_mode() {
        let tool = ExitPlanModeTool::new();
        let ctx = ctx(true);
        let result = tool
            .execute(json!({ "plan": "## Context\nFix it." }), &ctx, "call-1")
            .await;
        assert!(!result.is_error);
        assert!(result.text_content().contains("approved"));
        assert!(!ctx.plan_mode.load(Ordering::SeqCst));
    }

    /// A rejected plan and a plan nobody could review say different things:
    /// only the first is the user's answer.
    #[test]
    fn a_refusal_says_whether_the_user_rejected_the_plan() {
        let tool = ExitPlanModeTool::new();
        assert_eq!(tool.refusal(true), Some(REJECTED));
        assert_eq!(tool.refusal(false), Some(NOBODY_TO_REVIEW));
    }
}
