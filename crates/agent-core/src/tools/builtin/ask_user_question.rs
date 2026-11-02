//! Structured multiple-choice question tool.

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, Question, QuestionOption,
    ToolAccesses,
};

pub struct AskUserQuestionTool {
    tool: Tool,
}

impl AskUserQuestionTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "ask_user_question",
                "Ask the user one or more structured multiple-choice questions. \
Only use it when the user's input genuinely changes what you do next; overusing it \
interrupts their flow. If you recommend an option, list it first and append \
'(Recommended)' to its label. The result is JSON with an `answers` object keyed by \
question text; each value is the chosen option label(s). If `answers` is empty and a \
`note` says the user dismissed the questions, they chose not to answer — do not treat \
this as selecting the recommended option; decide based on context and do not re-ask \
the same question.",
                json!({
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": {
                                        "type": "string",
                                        "description": "A specific, actionable question. End with '?'."
                                    },
                                    "header": {
                                        "type": "string",
                                        "description": "Short category tag (max 12 chars, e.g. 'Auth', 'Style')."
                                    },
                                    "options": {
                                        "type": "array",
                                        "minItems": 2,
                                        "maxItems": 4,
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": {
                                                    "type": "string",
                                                    "description": "Concise display text (1-5 words). If recommended, append '(Recommended)'."
                                                },
                                                "description": {
                                                    "type": "string",
                                                    "description": "Brief explanation of trade-offs or implications."
                                                }
                                            },
                                            "required": ["label"]
                                        }
                                    },
                                    "multi_select": {
                                        "type": "boolean",
                                        "description": "Whether the user can select multiple options.",
                                        "default": false
                                    }
                                },
                                "required": ["question", "options"]
                            }
                        }
                    },
                    "required": ["questions"]
                }),
            ),
        }
    }
}

impl Default for AskUserQuestionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for AskUserQuestionTool {
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
        let questions = match parse_questions(&input) {
            Ok(qs) => qs,
            Err(e) => return ExecutableToolResult::error(e),
        };

        if let Some(err) = validate_uniqueness(&questions) {
            return ExecutableToolResult::error(err);
        }

        // A configured channel that returns no answer means the user
        // *dismissed* the prompt — they declined, they did not pick the
        // recommended option. Surfacing that as a tool error would make the
        // model retry or stall; instead return an empty `answers` map with a
        // `note`, so the model proceeds on its own judgment. A genuine absence
        // of any interaction channel is still a hard error.
        let has_channel = ctx.user_interaction.is_some();
        match ctx.ask_user_question(&questions).await {
            Some(answers) if !answers.answers.is_empty() => ExecutableToolResult::text(
                serde_json::to_string(&json!({ "answers": answers.answers })).unwrap_or_default(),
            ),
            _ if has_channel => ExecutableToolResult::text(
                serde_json::to_string(&json!({
                    "answers": {},
                    "note": "The user dismissed the questions without answering. \
                They chose not to answer — decide based on context and do not re-ask.",
                }))
                .unwrap_or_default(),
            ),
            _ => ExecutableToolResult::error("Nobody can answer questions in this run."),
        }
    }
}

fn parse_questions(input: &serde_json::Value) -> Result<Vec<Question>, String> {
    let arr = input
        .get("questions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "Missing or invalid `questions` array.".to_string())?;

    let mut questions = Vec::with_capacity(arr.len());
    for item in arr {
        let question = item
            .get("question")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Each question must have a `question` string.".to_string())?
            .to_string();
        let header = item
            .get("header")
            .and_then(|v| v.as_str())
            .map(String::from);
        let multi_select = item
            .get("multi_select")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let options_arr = item
            .get("options")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "Each question must have an `options` array.".to_string())?;

        if options_arr.len() < 2 {
            return Err("Each question must have at least 2 options.".to_string());
        }

        let mut options = Vec::with_capacity(options_arr.len());
        for opt in options_arr {
            let label = opt
                .get("label")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Each option must have a `label`.".to_string())?
                .to_string();
            let description = opt
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from);
            options.push(QuestionOption { label, description });
        }

        questions.push(Question {
            question,
            header,
            options,
            multi_select,
        });
    }

    Ok(questions)
}

fn validate_uniqueness(questions: &[Question]) -> Option<String> {
    let mut seen_questions = std::collections::HashSet::new();
    for q in questions {
        if !seen_questions.insert(&q.question) {
            return Some(format!(
                "Duplicate question text: {:?}. Question texts must be unique.",
                q.question
            ));
        }
        let mut seen_labels = std::collections::HashSet::new();
        for opt in &q.options {
            if !seen_labels.insert(&opt.label) {
                return Some(format!(
                    "Duplicate option label {:?} in question {:?}. Option labels must be unique within a question.",
                    opt.label, q.question
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserInteraction;

    struct MockUserInteraction {
        answer: String,
    }

    #[async_trait::async_trait]
    impl UserInteraction for MockUserInteraction {
        async fn ask(&self, _question: &str) -> Option<String> {
            Some(self.answer.clone())
        }
    }

    /// A channel that is present but returns no answer — i.e. the user dismissed
    /// the prompt.
    struct DismissingUserInteraction;

    #[async_trait::async_trait]
    impl UserInteraction for DismissingUserInteraction {
        async fn ask(&self, _question: &str) -> Option<String> {
            None
        }
    }

    fn make_ctx(answer: &str) -> ExecutableToolContext {
        build_ctx(Some(std::sync::Arc::new(MockUserInteraction {
            answer: answer.to_string(),
        })))
    }

    fn build_ctx(
        user_interaction: Option<std::sync::Arc<dyn UserInteraction>>,
    ) -> ExecutableToolContext {
        let background = std::sync::Arc::new(crate::background::BackgroundManager::new());
        ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::path::PathBuf::from("."),
            allowed_paths: std::collections::HashSet::new(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            user_interaction,
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
    async fn test_ask_user_question_returns_answer() {
        let tool = AskUserQuestionTool::new();
        let ctx = make_ctx("1");
        let input = json!({
            "questions": [{
                "question": "Continue?",
                "options": [
                    {"label": "Yes"},
                    {"label": "No"}
                ]
            }]
        });
        let result = tool.execute(input, &ctx, "tc-1").await;
        assert!(!result.is_error);
        let text = result.text_content();
        assert!(text.contains("Yes"));
    }

    /// A dismissed prompt (channel present, no answer) is not an error and not
    /// the recommended pick: it returns empty `answers` with a `note`.
    #[tokio::test]
    async fn dismissed_question_returns_empty_answers_with_note() {
        let tool = AskUserQuestionTool::new();
        let ctx = build_ctx(Some(std::sync::Arc::new(DismissingUserInteraction)));
        let input = json!({
            "questions": [{
                "question": "Continue?",
                "options": [
                    {"label": "Yes (Recommended)"},
                    {"label": "No"}
                ]
            }]
        });
        let result = tool.execute(input, &ctx, "tc-1").await;
        assert!(!result.is_error, "a dismissal must not be a tool error");
        let value: serde_json::Value =
            serde_json::from_str(&result.text_content()).expect("valid JSON");
        assert!(
            value["answers"].as_object().unwrap().is_empty(),
            "answers must be empty on dismissal"
        );
        assert!(
            value["note"].is_string(),
            "a dismissal note must be present"
        );
        // The recommended label must never leak in as if it were chosen.
        assert!(!result.text_content().contains("Yes (Recommended)"));
    }

    /// With no interaction channel at all, asking is a hard error.
    #[tokio::test]
    async fn missing_channel_is_an_error() {
        let tool = AskUserQuestionTool::new();
        let ctx = build_ctx(None);
        let input = json!({
            "questions": [{
                "question": "Continue?",
                "options": [{"label": "Yes"}, {"label": "No"}]
            }]
        });
        let result = tool.execute(input, &ctx, "tc-1").await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn test_ask_user_question_rejects_duplicate_labels() {
        let tool = AskUserQuestionTool::new();
        let ctx = make_ctx("Yes");
        let input = json!({
            "questions": [{
                "question": "Continue?",
                "options": [
                    {"label": "Yes"},
                    {"label": "Yes"}
                ]
            }]
        });
        let result = tool.execute(input, &ctx, "tc-1").await;
        assert!(result.is_error);
    }

    #[test]
    fn test_parse_questions_multi_select() {
        let input = json!({
            "questions": [{
                "question": "Pick colors",
                "multi_select": true,
                "options": [
                    {"label": "Red", "description": "warm"},
                    {"label": "Blue"}
                ]
            }]
        });
        let qs = parse_questions(&input).unwrap();
        assert!(qs[0].multi_select);
        assert_eq!(qs[0].options[0].description.as_deref(), Some("warm"));
    }
}
