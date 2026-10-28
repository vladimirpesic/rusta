//! Built-in `skill` tool for invoking reusable Markdown prompts.

use std::{
    collections::HashMap,
    sync::Mutex,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::{
    agent_loop::types::{
        ExecutableTool, ExecutableToolContext, ExecutableToolResult, ForkBinding, ToolAccesses,
    },
    skills::{ActiveSkills, SkillRegistry},
};

/// Maximum number of concurrent skill invocations allowed in one chain.
pub const MAX_SKILL_DEPTH: usize = 3;

pub struct SkillTool {
    tool: Tool,
    registry: Arc<SkillRegistry>,
    active_skills: Arc<Mutex<ActiveSkills>>,
    active_count: Arc<AtomicUsize>,
    /// Whether the user runs it (`/skill`), who may run any skill; the
    /// model may not run one marked `disable_model_invocation`.
    by_user: bool,
    /// Whether the model has a skill to run, without which the tool is not
    /// offered.
    has_skills: bool,
}

impl SkillTool {
    /// The model's `skill` tool, which lists the skills the model may run,
    /// each with its description and when to use it.
    pub fn new(registry: Arc<SkillRegistry>, active_skills: Arc<Mutex<ActiveSkills>>) -> Self {
        Self::build(registry, active_skills, false)
    }

    /// The tool `/skill` runs a skill with: the user may run any skill.
    pub fn for_user(registry: Arc<SkillRegistry>, active_skills: Arc<Mutex<ActiveSkills>>) -> Self {
        Self::build(registry, active_skills, true)
    }

    fn build(
        registry: Arc<SkillRegistry>,
        active_skills: Arc<Mutex<ActiveSkills>>,
        by_user: bool,
    ) -> Self {
        // Without a skill to list, the tool is not offered.
        let listing = registry
            .model_invocable()
            .into_iter()
            .map(crate::skills::Skill::listing)
            .collect::<Vec<_>>();
        let has_skills = !listing.is_empty();
        Self {
            tool: Tool::new(
                "skill",
                format!(
                    "Invoke a reusable skill prompt. The rendered prompt is returned as text for \
                     the agent to act on.\n\nAvailable skills:\n{}",
                    listing.join("\n")
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The skill's name, from those this tool lists"
                        },
                        "args": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                            "description": "Arguments to substitute into {{placeholders}} in the skill prompt"
                        },
                        "thought": {
                            "type": "string",
                            "description": "Optional reasoning for why this skill is being used"
                        }
                    },
                    "required": ["name"]
                }),
            ),
            registry,
            active_skills,
            active_count: Arc::new(AtomicUsize::new(0)),
            by_user,
            has_skills,
        }
    }
}

impl Default for SkillTool {
    fn default() -> Self {
        Self::new(
            Arc::new(SkillRegistry::new()),
            Arc::new(Mutex::new(ActiveSkills::new())),
        )
    }
}

#[async_trait]
impl ExecutableTool for SkillTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    /// A fork offers the same skills, records their use in its own store,
    /// and counts its own nesting.
    fn rebind_for_fork(&self, binding: &ForkBinding) -> Option<Arc<dyn ExecutableTool>> {
        Some(Arc::new(Self {
            tool: self.tool.clone(),
            registry: Arc::clone(&self.registry),
            active_skills: Arc::clone(&binding.active_skills),
            active_count: Arc::new(AtomicUsize::new(0)),
            by_user: self.by_user,
            has_skills: self.has_skills,
        }))
    }

    fn is_available(&self, _ctx: &ExecutableToolContext) -> bool {
        self.has_skills
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
        _ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let name = match input.get("name").and_then(|v| v.as_str()) {
            Some(n) if !n.is_empty() => n,
            _ => return ExecutableToolResult::error("skill name is required"),
        };

        let depth = self.active_count.fetch_add(1, Ordering::SeqCst);
        if depth >= MAX_SKILL_DEPTH {
            self.active_count.fetch_sub(1, Ordering::SeqCst);
            return ExecutableToolResult::error(format!(
                "maximum skill nesting depth ({MAX_SKILL_DEPTH}) exceeded"
            ));
        }

        let result = self.invoke(name, &input).await;
        self.active_count.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

impl SkillTool {
    async fn invoke(&self, name: &str, input: &serde_json::Value) -> ExecutableToolResult {
        let args = input
            .get("args")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect::<HashMap<String, String>>()
            })
            .unwrap_or_default();

        let Some(skill) = self.registry.get(name) else {
            return ExecutableToolResult::error(format!("unknown skill: {name}"));
        };
        if skill.disable_model_invocation && !self.by_user {
            return ExecutableToolResult::error(format!(
                "skill {name} is run only by the user (its disable_model_invocation is set); \
                 the user runs it with /skill {name}"
            ));
        }
        let rendered = skill.render(&args);

        if let Ok(mut guard) = self.active_skills.lock() {
            let _ = guard.record(name, args);
        }

        ExecutableToolResult::text(rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::types::ExecutableToolContext;

    fn ctx() -> ExecutableToolContext {
        let background = Arc::new(crate::background::BackgroundManager::new());
        ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::path::PathBuf::from("."),
            allowed_paths: std::collections::HashSet::new(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            user_interaction: None,
            search_url: None,
            plan_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: crate::permissions::PermissionMode::Default,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            background: background.clone(),
            cron: Arc::new(crate::cron::CronManager::new()),
            goal_manager: Arc::new(Mutex::new(crate::goal::GoalManager::new())),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            ..Default::default()
        }
    }

    #[test]
    fn test_skill_tool_definition() {
        let tool = SkillTool::default();
        assert_eq!(tool.tool_definition().name, "skill");
    }

    #[tokio::test]
    async fn test_skill_tool_renders_and_records() {
        let mut registry = SkillRegistry::new();
        registry.register(crate::skills::Skill {
            name: "greet".to_string(),
            description: None,
            skill_type: crate::skills::SkillType::System,
            when_to_use: None,
            disable_model_invocation: false,
            content: "Hello {{name}}".to_string(),
        });
        let active = Arc::new(Mutex::new(ActiveSkills::new()));
        let tool = SkillTool::new(Arc::new(registry), active.clone());

        let result = tool
            .execute(
                json!({ "name": "greet", "args": { "name": "world" } }),
                &ctx(),
                "id",
            )
            .await;
        assert!(!result.is_error);
        assert_eq!(result.text_content(), "Hello world");

        let guard = active.lock().unwrap();
        assert_eq!(guard.activations.len(), 1);
        assert_eq!(guard.activations[0].name, "greet");
    }

    /// A registry with `deploy`, which only the user may run, and `review`.
    fn deploy_and_review() -> Arc<SkillRegistry> {
        let mut registry = SkillRegistry::new();
        registry.register(crate::skills::Skill {
            name: "review".to_string(),
            description: Some("Review the diff.".to_string()),
            skill_type: crate::skills::SkillType::User,
            when_to_use: Some("Before a commit".to_string()),
            disable_model_invocation: false,
            content: "Review it.".to_string(),
        });
        registry.register(crate::skills::Skill {
            name: "deploy".to_string(),
            description: Some("Ship to production.".to_string()),
            skill_type: crate::skills::SkillType::User,
            when_to_use: None,
            disable_model_invocation: true,
            content: "Ship it.".to_string(),
        });
        Arc::new(registry)
    }

    /// The model is shown, and may run, only the skills not kept for the
    /// user; `/skill` runs any of them.
    #[tokio::test]
    async fn a_skill_kept_for_the_user_is_neither_listed_nor_run_for_the_model() {
        let active = Arc::new(Mutex::new(ActiveSkills::new()));
        let model = SkillTool::new(deploy_and_review(), active.clone());
        assert_eq!(
            model.tool_definition().description,
            "Invoke a reusable skill prompt. The rendered prompt is returned as text for the \
             agent to act on.\n\nAvailable skills:\n- review: Review the diff.\n  When to use: \
             Before a commit"
        );
        let refused = model
            .execute(json!({ "name": "deploy" }), &ctx(), "id")
            .await;
        assert!(refused.is_error);
        assert!(
            refused.text_content().contains("/skill deploy"),
            "{}",
            refused.text_content()
        );
        assert!(active.lock().unwrap().activations.is_empty());
        // A fork's tool is still the model's.
        let fork = model
            .rebind_for_fork(&ForkBinding {
                registry: std::sync::Weak::new(),
                disclosure: Default::default(),
                active_skills: active.clone(),
            })
            .unwrap();
        let refused = fork
            .execute(json!({ "name": "deploy" }), &ctx(), "id")
            .await;
        assert!(refused.is_error);

        let user = SkillTool::for_user(deploy_and_review(), active.clone());
        let ran = user
            .execute(json!({ "name": "deploy" }), &ctx(), "id")
            .await;
        assert_eq!(ran.text_content(), "Ship it.");
        assert!(!ran.is_error);
        assert_eq!(active.lock().unwrap().activations[0].name, "deploy");
    }

    /// Offered only with a skill the model may run.
    #[test]
    fn offered_only_with_a_skill_for_the_model() {
        let active = Arc::new(Mutex::new(ActiveSkills::new()));
        assert!(SkillTool::new(deploy_and_review(), active.clone()).is_available(&ctx()));
        assert!(!SkillTool::default().is_available(&ctx()));
        let mut user_only = SkillRegistry::new();
        user_only.register(crate::skills::Skill {
            name: "deploy".to_string(),
            description: None,
            skill_type: crate::skills::SkillType::User,
            when_to_use: None,
            disable_model_invocation: true,
            content: "Ship it.".to_string(),
        });
        assert!(!SkillTool::new(Arc::new(user_only), active).is_available(&ctx()));
    }

    #[tokio::test]
    async fn test_unknown_skill_errors() {
        let tool = SkillTool::default();
        let result = tool.execute(json!({"name": "missing"}), &ctx(), "id").await;
        assert!(result.is_error);
        assert!(result.text_content().contains("unknown skill"));
    }

    #[tokio::test]
    async fn test_depth_limit() {
        let registry = Arc::new(SkillRegistry::new());
        let active = Arc::new(Mutex::new(ActiveSkills::new()));
        let counter = Arc::new(AtomicUsize::new(MAX_SKILL_DEPTH));
        let tool = SkillTool {
            tool: Tool::new("skill", "", serde_json::Value::Null),
            registry,
            active_skills: active,
            active_count: counter,
            by_user: false,
            has_skills: false,
        };

        let result = tool.execute(json!({"name": "x"}), &ctx(), "id").await;
        assert!(result.is_error);
        assert!(
            result
                .text_content()
                .contains("maximum skill nesting depth")
        );
    }
}
