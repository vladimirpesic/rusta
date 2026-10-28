//! Tools loaded with `select_tools` reach a model that takes tools
//! mid-conversation on a system message, leaving the top-level list — the
//! request's cacheable prefix — as it was. Other models get them in the
//! top-level list.

use std::sync::{Arc, Mutex};

use agent_core::Agent;
use agent_core::agent_loop::llm::{Llm, LlmCallbacks};
use agent_core::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, StepResult, ToolAccesses,
};
use async_trait::async_trait;
use lm_bridge::message::{Message, Role, ToolCall};
use lm_bridge::provider::FinishReason;
use lm_bridge::tool::Tool;
use lm_bridge::usage::TokenUsage;
use tokio_util::sync::CancellationToken;

/// A loadable tool, as an MCP server's would be.
struct Lookup {
    def: Tool,
}

#[async_trait]
impl ExecutableTool for Lookup {
    fn tool_definition(&self) -> &Tool {
        &self.def
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
        _ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        ExecutableToolResult::text("found it")
    }
}

/// Plays a script; records each request's messages and tool names.
struct ScriptedLlm {
    dynamic: bool,
    steps: Mutex<Vec<StepResult>>,
    seen: Mutex<Vec<(Vec<Message>, Vec<String>)>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    fn model_name(&self) -> &str {
        "scripted"
    }

    fn supports_dynamic_tools(&self) -> bool {
        self.dynamic
    }

    async fn chat(
        &self,
        messages: Vec<Message>,
        tools: Vec<Tool>,
        _callbacks: LlmCallbacks,
        _cancel: &CancellationToken,
    ) -> Result<StepResult, lm_bridge::errors::LlmError> {
        let names = tools.iter().map(|t| t.name.clone()).collect();
        self.seen.lock().unwrap().push((messages, names));
        Ok(self.steps.lock().unwrap().remove(0))
    }
}

fn step(message: Message, finish_reason: FinishReason) -> StepResult {
    StepResult {
        message,
        usage: TokenUsage::empty(),
        finish_reason: Some(finish_reason),
        response_id: None,
        stop_reason: None,
    }
}

fn call(id: &str, name: &str, args: &str) -> StepResult {
    step(
        Message::assistant_tool_calls(vec![ToolCall::new(id, name).with_arguments(args)]),
        FinishReason::ToolCalls,
    )
}

async fn run(dynamic: bool) -> Arc<ScriptedLlm> {
    let llm = Arc::new(ScriptedLlm {
        dynamic,
        steps: Mutex::new(vec![
            call("c1", "select_tools", r#"{"names":["mcp__docs__lookup"]}"#),
            call("c2", "mcp__docs__lookup", r#"{"q":"x"}"#),
            step(Message::assistant_text("done"), FinishReason::Stop),
        ]),
        seen: Mutex::new(Vec::new()),
    });
    let mut agent = Agent::new(std::env::temp_dir(), "sys").with_llm(llm.clone());
    agent.register_dynamic_tool(Arc::new(Lookup {
        def: Tool::new(
            "mcp__docs__lookup",
            "Look something up.",
            serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        ),
    }));
    agent
        .prompt(llm.as_ref(), "find x", &CancellationToken::new())
        .await
        .unwrap();
    llm
}

fn tools_messages(messages: &[Message]) -> Vec<Vec<String>> {
    messages
        .iter()
        .filter(|m| m.role == Role::System && !m.tools.is_empty())
        .map(|m| m.tools.iter().map(|t| t.name.clone()).collect())
        .collect()
}

#[tokio::test]
async fn a_dynamic_model_gets_loaded_tools_on_a_system_message() {
    let llm = run(true).await;
    let seen = llm.seen.lock().unwrap();
    let (first_messages, first_tools) = &seen[0];
    let (second_messages, second_tools) = &seen[1];
    assert!(tools_messages(first_messages).is_empty());
    assert_eq!(first_tools, second_tools, "the top-level list stays fixed");
    assert!(!second_tools.contains(&"mcp__docs__lookup".to_string()));
    assert_eq!(
        tools_messages(second_messages),
        [vec!["mcp__docs__lookup".to_string()]],
        "delivered once, on a system message"
    );
    let (third_messages, _) = &seen[2];
    assert_eq!(
        tools_messages(third_messages).len(),
        1,
        "not delivered twice"
    );
}

#[tokio::test]
async fn other_models_get_loaded_tools_in_the_top_level_list() {
    let llm = run(false).await;
    let seen = llm.seen.lock().unwrap();
    let (second_messages, second_tools) = &seen[1];
    assert!(second_tools.contains(&"mcp__docs__lookup".to_string()));
    assert!(tools_messages(second_messages).is_empty());
}
