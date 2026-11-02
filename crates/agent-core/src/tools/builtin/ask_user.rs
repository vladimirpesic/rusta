use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

pub struct AskUserTool {
    tool: Tool,
}

impl AskUserTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "ask_user",
                "Ask the user a question and wait for a text response.",
                json!({
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "Question to ask the user" }
                    },
                    "required": ["question"]
                }),
            ),
        }
    }
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for AskUserTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
        ToolAccesses::none()
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        "ask_user"
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let question = input.get("question").and_then(|v| v.as_str()).unwrap_or("");
        if ctx.user_interaction.is_none() {
            return ExecutableToolResult::error("Nobody can answer questions in this run.");
        }
        // A channel that returns no answer means the user dismissed the
        // question, as `ask_user_question` says too.
        match ctx.ask_user(question).await {
            Some(answer) => ExecutableToolResult::text(answer),
            None => ExecutableToolResult::text(
                "The user dismissed the question without answering. They chose not to answer — \
                 decide based on context and do not re-ask.",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserInteraction;

    struct MockUserInteraction {
        answer: String,
    }

    #[async_trait]
    impl UserInteraction for MockUserInteraction {
        async fn ask(&self, _question: &str) -> Option<String> {
            Some(self.answer.clone())
        }
    }

    #[tokio::test]
    async fn test_ask_user_returns_answer() {
        let tool = AskUserTool::new();
        let background = std::sync::Arc::new(crate::background::BackgroundManager::new());
        let ctx = ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::path::PathBuf::from("."),
            allowed_paths: std::collections::HashSet::new(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            user_interaction: Some(std::sync::Arc::new(MockUserInteraction {
                answer: "yes".to_string(),
            })),
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
        };
        let result = tool
            .execute(json!({"question": "continue?"}), &ctx, "test-id")
            .await;
        assert!(!result.is_error);
        assert_eq!(result.text_content(), "yes");
    }

    #[tokio::test]
    async fn test_ask_user_errors_without_channel() {
        let tool = AskUserTool::new();
        let background = std::sync::Arc::new(crate::background::BackgroundManager::new());
        let ctx = ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::path::PathBuf::from("."),
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
            background: background.clone(),
            goal_manager: std::sync::Arc::new(std::sync::Mutex::new(
                crate::goal::GoalManager::new(),
            )),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            cron: std::sync::Arc::new(crate::cron::CronManager::new()),
            ..Default::default()
        };
        let result = tool
            .execute(json!({"question": "continue?"}), &ctx, "test-id")
            .await;
        assert!(result.is_error);
        assert_eq!(
            result.text_content(),
            "Nobody can answer questions in this run."
        );
    }

    struct Dismissing;

    #[async_trait]
    impl UserInteraction for Dismissing {
        async fn ask(&self, _question: &str) -> Option<String> {
            None
        }
    }

    /// A dismissed question is the user's answer, not a missing channel.
    #[tokio::test]
    async fn a_dismissed_question_is_not_taken_for_nobody_to_ask() {
        let ctx = ExecutableToolContext {
            user_interaction: Some(std::sync::Arc::new(Dismissing)),
            ..Default::default()
        };
        let result = AskUserTool::new()
            .execute(json!({"question": "continue?"}), &ctx, "test-id")
            .await;
        assert!(!result.is_error);
        assert!(result.text_content().contains("dismissed"));
    }
}
