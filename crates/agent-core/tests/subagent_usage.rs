//! A turn reports the tokens its sub-agents spent, once.
//!
//! A sub-agent's turn usage travels with its hand-off, the `agent` tool's
//! result, so `--stream-json`'s `usage` covers delegated work too: a swarm's
//! cost is reported, and a goal's token budget counts the work it
//! delegated.

use std::sync::{Arc, Mutex};

use agent_core::Agent;
use agent_core::agent_loop::llm::{Llm, LlmCallbacks};
use agent_core::agent_loop::types::StepResult;
use async_trait::async_trait;
use lm_bridge::message::{Message, ToolCall};
use lm_bridge::provider::FinishReason;
use lm_bridge::tool::Tool;
use lm_bridge::usage::TokenUsage;
use tokio_util::sync::CancellationToken;

/// Plays back a script of steps, for the session and its sub-agent alike:
/// a sub-agent's steps run while the session waits on its tool call.
struct ScriptedLlm(Mutex<Vec<StepResult>>);

#[async_trait]
impl Llm for ScriptedLlm {
    fn model_name(&self) -> &str {
        "scripted"
    }

    async fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: Vec<Tool>,
        _callbacks: LlmCallbacks,
        _cancel: &CancellationToken,
    ) -> Result<StepResult, lm_bridge::errors::LlmError> {
        let mut steps = self.0.lock().unwrap();
        assert!(!steps.is_empty(), "the script ran out");
        Ok(steps.remove(0))
    }
}

fn spent(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        ..TokenUsage::empty()
    }
}

fn step(message: Message, finish_reason: FinishReason, usage: TokenUsage) -> StepResult {
    StepResult {
        message,
        usage,
        finish_reason: Some(finish_reason),
        response_id: None,
        stop_reason: None,
    }
}

#[tokio::test]
async fn a_turn_reports_what_its_sub_agents_spent() {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(ScriptedLlm(Mutex::new(vec![
        // The session delegates…
        step(
            Message::assistant_tool_calls(vec![
                ToolCall::new("c0", "agent").with_arguments(r#"{"prompt":"Find the bug."}"#),
            ]),
            FinishReason::ToolCalls,
            spent(1_000, 5),
        ),
        // …the sub-agent answers…
        step(
            Message::assistant_text("It is in retry.rs."),
            FinishReason::Stop,
            spent(100, 10),
        ),
        // …and the session reports.
        step(
            Message::assistant_text("The bug is in retry.rs."),
            FinishReason::Stop,
            spent(1_200, 7),
        ),
        // A second turn, without sub-agents.
        step(
            Message::assistant_text("Nothing else."),
            FinishReason::Stop,
            spent(1_300, 2),
        ),
    ])));
    // The sub-agent's records go to a directory of the test's own: by
    // default they go to the user's data directory.
    let data = tempfile::tempdir().unwrap();
    let mut agent = Agent::new(dir.path().to_path_buf(), "sys")
        .with_data_dir(data.path().to_path_buf())
        .with_llm(llm.clone());
    let cancel = CancellationToken::new();

    let first = agent
        .prompt(llm.as_ref(), "Find the bug.", &cancel)
        .await
        .unwrap();
    assert_eq!(first.message.text_content(), "The bug is in retry.rs.");
    assert_eq!(first.usage.input_tokens, 2_300, "{:?}", first.usage);
    assert_eq!(first.usage.output_tokens, 22);
    assert_eq!(first.usage.total_tokens, 2_322);
    let records = std::fs::read_dir(data.path().join("subagents"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|entry| entry.path().extension().is_some_and(|e| e == "jsonl"))
        })
        .count();
    assert_eq!(
        records, 1,
        "the sub-agent's records are in the test's directory"
    );

    let second = agent
        .prompt(llm.as_ref(), "Anything else?", &cancel)
        .await
        .unwrap();
    assert_eq!(
        second.usage.total_tokens, 1_302,
        "the sub-agent's tokens are reported once"
    );
}
