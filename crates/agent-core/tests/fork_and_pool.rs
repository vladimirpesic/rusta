//! What a forked sub-agent inherits, and what the model pool advertises.
//!
//! Both features are visible only at the seams the unit tests cannot reach: the
//! messages a child actually sends, and the tool schema a model actually sees.

use std::sync::Arc;

use agent_core::model_pool::{ModelPool, ModelPoolEntry};
use agent_core::{Agent, FORK_INHERITANCE_NOTICE};
use lm_bridge::message::{ContentPart, Message, Role, ToolCall};

// ---- fork -----------------------------------------------------------------

fn seeded_history(inherited: Vec<Message>) -> Vec<Message> {
    Agent::new(std::env::temp_dir(), "child system prompt")
        .with_forked_context(inherited)
        .context_history()
}

fn assistant_calling(text: &str, id: &str, name: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentPart::text(text)],
        tool_calls: vec![ToolCall::new(id, name)],
        tool_call_id: None,
        tools: Vec::new(),
        partial: false,
    }
}

/// The caller's turns must arrive, in order, ahead of the child's own prompt.
#[test]
fn the_callers_conversation_is_inherited_in_order() {
    let history = seeded_history(vec![
        Message::user("the bug is in the retry loop"),
        Message::assistant_text("I found it in retry.rs"),
    ]);
    let text: Vec<String> = history.iter().map(Message::text_content).collect();
    let bug = text.iter().position(|t| t.contains("retry loop"));
    let found = text.iter().position(|t| t.contains("retry.rs"));
    assert!(bug.is_some() && found.is_some(), "{text:?}");
    assert!(bug < found, "order must be preserved: {text:?}");
}

/// **Unframed, the child reads the inherited turns as its own** and carries that
/// conversation on instead of doing the job it was given.
#[test]
fn the_inheritance_notice_follows_the_inherited_turns() {
    let history = seeded_history(vec![
        Message::user("earlier context"),
        Message::assistant_text("understood"),
    ]);
    let notice = history
        .iter()
        .position(|m| {
            m.text_content()
                .contains("inherited from the agent that spawned you")
        })
        .expect("the notice must be present");
    let context = history
        .iter()
        .position(|m| m.text_content().contains("earlier context"))
        .expect("the inherited turn must be present");

    assert!(context < notice, "the notice frames what came before it");
    assert_eq!(history[notice].role, Role::User);
    assert!(FORK_INHERITANCE_NOTICE.contains("not your own prior turns"));
}

/// The child keeps its *own* system prompt. A second system message buried
/// mid-history is instruction the model cannot act on.
#[test]
fn the_child_keeps_one_system_prompt_of_its_own() {
    let history = seeded_history(vec![
        Message::system("the CALLER's system prompt"),
        Message::user("go"),
    ]);
    let systems: Vec<&Message> = history.iter().filter(|m| m.role == Role::System).collect();
    assert_eq!(systems.len(), 1, "exactly one system message: {history:?}");
    assert!(systems[0].text_content().contains("child system prompt"));
    assert_eq!(history[0].role, Role::System, "and it must lead");
}

/// **The in-flight `agent` call must reach the child answered.** It created the
/// fork, so it can never have a real result — and a history with an unanswered
/// tool call is one Moonshot and Z.AI both reject.
#[test]
fn the_in_flight_spawn_arrives_closed_off() {
    let history = seeded_history(vec![
        Message::user("investigate this"),
        assistant_calling("spawning a helper", "tc-fork", "agent"),
    ]);

    assert!(
        history
            .iter()
            .any(|m| m.tool_calls.iter().any(|tc| tc.id == "tc-fork")),
        "the spawning call must survive — it is why the child exists"
    );
    let result = history
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("tc-fork"))
        .expect("and it must be answered");
    assert!(
        result.text_content().contains("outcome is unknown"),
        "the child must not read the silence as failure: {}",
        result.text_content()
    );

    let answered: std::collections::HashSet<&str> = history
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    for message in &history {
        for call in &message.tool_calls {
            assert!(
                answered.contains(call.id.as_str()),
                "tool call {} reaches the provider unanswered",
                call.id
            );
        }
    }
}

/// Forking an empty conversation is legal and degenerate.
#[test]
fn forking_an_empty_conversation_is_harmless() {
    let history = seeded_history(Vec::new());
    assert_eq!(history[0].role, Role::System);
    assert!(history.iter().all(|m| m.tool_calls.is_empty()));
}

// ---- model pool -----------------------------------------------------------

struct Pool {
    entries: Vec<ModelPoolEntry>,
    default_alias: Option<String>,
}

impl ModelPool for Pool {
    fn entries(&self) -> &[ModelPoolEntry] {
        &self.entries
    }
    fn default_alias(&self) -> Option<&str> {
        self.default_alias.as_deref()
    }
    fn resolve(&self, _alias: &str) -> Result<Arc<dyn agent_core::Llm>, String> {
        Err("not spawned in these tests".to_string())
    }
}

fn pool() -> Arc<dyn ModelPool> {
    Arc::new(Pool {
        entries: vec![
            ModelPoolEntry {
                alias: "fast".to_string(),
                model: "glm-5.3-flash".to_string(),
                description: "Locating files and simple lookups.".to_string(),
            },
            ModelPoolEntry {
                alias: "deep".to_string(),
                model: "glm-5.3".to_string(),
                description: "Architecture and tricky bugs.".to_string(),
            },
        ],
        default_alias: Some("fast".to_string()),
    })
}

fn schema_of(agent: &Agent, tool: &str) -> serde_json::Value {
    agent
        .tool_definitions()
        .into_iter()
        .find(|t| t.name == tool)
        .unwrap_or_else(|| panic!("{tool} must be registered"))
        .parameters
}

/// Without a pool the schema must not gain a parameter whose only legal value is
/// the status quo — that invites the model to reason about a non-decision.
#[test]
fn without_a_pool_no_model_parameter_is_advertised() {
    let agent = Agent::new(std::env::temp_dir(), "sys");
    for tool in ["agent", "agent_swarm"] {
        assert!(
            schema_of(&agent, tool)["properties"].get("model").is_none(),
            "{tool} must not advertise `model` without a pool"
        );
    }
}

/// With one, both delegation tools advertise it, described by purpose.
#[test]
fn a_pool_reaches_both_delegation_tools() {
    let agent = Agent::new(std::env::temp_dir(), "sys").with_model_pool(Some(pool()), "glm-5.3");
    for tool in ["agent", "agent_swarm"] {
        let schema = schema_of(&agent, tool);
        let model = schema["properties"]
            .get("model")
            .unwrap_or_else(|| panic!("{tool} must advertise `model`: {schema}"));
        let text = model["description"].as_str().expect("a description");
        assert!(
            text.contains("`primary` (= glm-5.3)"),
            "{tool} must name the caller's model: {text}"
        );
        if tool == "agent" {
            assert!(
                text.contains("Locating files"),
                "agent must carry the hints"
            );
            assert!(
                text.contains("`fast` [glm-5.3-flash] [default]"),
                "{tool}: {text}"
            );
        } else {
            assert!(
                text.contains("`fast` [default]"),
                "the swarm lists the choices on one line: {text}"
            );
            assert!(!text.contains('\n'), "{text}");
        }
        assert!(
            !schema["required"]
                .as_array()
                .expect("required")
                .contains(&serde_json::json!("model")),
            "{tool}: `model` must stay optional"
        );
    }
}

/// **Attaching a pool must not drop the profiles, and vice versa.** Both are
/// baked into the same tool descriptions, so each builder rebuilds the tools —
/// and rebuilding from scratch would discard whatever the other had set.
#[test]
fn a_pool_and_a_profile_registry_survive_each_other() {
    let profiles = Arc::new(agent_core::ProfileRegistry::with_builtin_profiles());
    let expected = profiles
        .names()
        .first()
        .cloned()
        .expect("a builtin profile");

    for agent in [
        Agent::new(std::env::temp_dir(), "sys")
            .with_profile_registry(Arc::clone(&profiles))
            .with_model_pool(Some(pool()), "glm-5.3"),
        Agent::new(std::env::temp_dir(), "sys")
            .with_model_pool(Some(pool()), "glm-5.3")
            .with_profile_registry(Arc::clone(&profiles)),
    ] {
        let schema = schema_of(&agent, "agent");
        assert!(
            schema["properties"].get("model").is_some(),
            "the pool must survive: {schema}"
        );
        assert!(
            schema["properties"]["profile"]["description"]
                .as_str()
                .expect("a profile description")
                .contains(&expected),
            "the profiles must survive"
        );
    }
}

/// `fork` is advertised and optional on the `agent` tool.
#[test]
fn fork_is_advertised_and_optional() {
    let schema = schema_of(&Agent::new(std::env::temp_dir(), "sys"), "agent");
    assert_eq!(
        schema["properties"]["fork"]["type"],
        serde_json::json!("boolean")
    );
    assert!(
        !schema["required"]
            .as_array()
            .expect("required")
            .contains(&serde_json::json!("fork")),
        "forking must stay opt-in"
    );
    assert!(
        schema["properties"]["fork"]["description"]
            .as_str()
            .expect("a description")
            .contains("one-time"),
        "the model must be told the two contexts do not stay in sync"
    );
}

// ---- the bound model is the one that answers --------------------------------

mod bound_model_answers {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agent_core::agent_loop::llm::LlmCallbacks;
    use agent_core::model_pool::{ModelPool, ModelPoolEntry};
    use agent_core::{Agent, Llm, PermissionMode, StepResult};
    use lm_bridge::message::{Message, ToolCall};
    use lm_bridge::provider::FinishReason;
    use lm_bridge::{LlmError, TokenUsage};
    use tokio_util::sync::CancellationToken;

    /// Replies from a script, counting how often it is asked.
    struct Scripted {
        name: &'static str,
        calls: Arc<AtomicUsize>,
        script: std::sync::Mutex<Vec<Message>>,
    }

    impl Scripted {
        fn new(name: &'static str, script: Vec<Message>) -> Arc<Self> {
            Arc::new(Self {
                name,
                calls: Arc::new(AtomicUsize::new(0)),
                script: std::sync::Mutex::new(script),
            })
        }
    }

    #[async_trait::async_trait]
    impl Llm for Scripted {
        fn model_name(&self) -> &str {
            self.name
        }

        fn context_limit(&self) -> Option<u64> {
            Some(1_048_576)
        }

        async fn chat(
            &self,
            _messages: Vec<Message>,
            _tools: Vec<lm_bridge::tool::Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut script = self.script.lock().unwrap();
            let message = if script.is_empty() {
                Message::assistant_text(format!("{} is done", self.name))
            } else {
                script.remove(0)
            };
            let finish = if message.tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            };
            Ok(StepResult {
                message,
                usage: TokenUsage::empty(),
                finish_reason: Some(finish),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    struct FastPool {
        entries: Vec<ModelPoolEntry>,
        fast: Arc<Scripted>,
    }

    impl ModelPool for FastPool {
        fn entries(&self) -> &[ModelPoolEntry] {
            &self.entries
        }
        fn default_alias(&self) -> Option<&str> {
            None
        }
        fn resolve(&self, alias: &str) -> Result<Arc<dyn Llm>, String> {
            match alias {
                "fast" => Ok(self.fast.clone()),
                other => Err(format!("unknown alias {other}")),
            }
        }
    }

    async fn delegate(tool: &str) -> (usize, usize, String) {
        let args = match tool {
            "agent" => r#"{"prompt": "find the file", "model": "fast"}"#,
            _ => {
                r#"{"description": "find the files", "prompt_template": "find {{item}}", "items": ["a.rs", "b.rs"], "model": "fast"}"#
            }
        };
        let primary = Scripted::new(
            "primary",
            vec![Message::assistant_tool_calls(vec![
                ToolCall::new("tc-1", tool).with_arguments(args),
            ])],
        );
        let fast = Scripted::new("fast", Vec::new());
        let pool = Arc::new(FastPool {
            entries: vec![ModelPoolEntry {
                alias: "fast".to_string(),
                model: "glm-5.3-flash".to_string(),
                description: "Simple lookups.".to_string(),
            }],
            fast: fast.clone(),
        });
        let dir = tempfile::tempdir().unwrap();
        let mut agent = Agent::new(dir.path().to_path_buf(), "sys")
            .with_data_dir(dir.path().join("data"))
            .with_permission_mode(PermissionMode::BypassPermissions)
            .with_llm(primary.clone())
            .with_model_pool(Some(pool), "primary");
        // Only the user turns swarm mode on, which `agent_swarm` needs.
        agent.set_swarm_mode(Some(agent_core::swarm::SwarmTrigger::Manual));
        agent
            .prompt(primary.as_ref(), "delegate it", &CancellationToken::new())
            .await
            .unwrap();
        let result = agent
            .history()
            .into_iter()
            .find(|m| m.role == lm_bridge::message::Role::Tool)
            .map(|m| m.text_content())
            .unwrap_or_default();
        (
            primary.calls.load(Ordering::SeqCst),
            fast.calls.load(Ordering::SeqCst),
            result,
        )
    }

    /// `agent` with `model: "fast"` runs the sub-agent on the pooled model,
    /// not on the caller's.
    #[tokio::test]
    async fn a_sub_agent_runs_on_its_bound_model() {
        let (primary_calls, fast_calls, result) = delegate("agent").await;
        assert_eq!(fast_calls, 1, "the pooled model answers the sub-agent");
        assert_eq!(primary_calls, 2, "the caller only runs its own two steps");
        assert!(result.contains("fast is done"), "{result}");
    }

    /// The same holds for every swarm worker.
    #[tokio::test]
    async fn swarm_workers_run_on_their_bound_model() {
        let (primary_calls, fast_calls, result) = delegate("agent_swarm").await;
        assert_eq!(fast_calls, 2, "each worker answers on the pooled model");
        assert_eq!(primary_calls, 2);
        assert!(result.contains("fast is done"), "{result}");
    }
}
