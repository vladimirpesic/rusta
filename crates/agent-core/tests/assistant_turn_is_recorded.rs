//! The assistant's own words must survive the turn.
//!
//! A step is recorded with the model's text and reasoning, not only its tool
//! calls; the streaming callbacks alone would show them on the terminal and
//! drop them. Three things depend on it, and all three are pinned here because
//! none of them is visible from the unit tests of the pieces:
//!
//! * **Preserved Thinking replays it.** The vendors want prior reasoning
//!   echoed back, also on a turn that called a tool.
//! * **Sub-agents report it.** The `agent` tool answers with the child's last
//!   assistant text; without it every delegation would come back
//!   `(subagent returned no text)`.
//! * **A resumed session keeps the answer.** The final assistant message
//!   reaches the records.

use std::sync::Mutex;

use agent_core::Agent;
use agent_core::agent_loop::llm::{Llm, LlmCallbacks};
use agent_core::agent_loop::types::StepResult;
use async_trait::async_trait;
use lm_bridge::message::{ContentPart, Message, Role, ToolCall};
use lm_bridge::provider::FinishReason;
use lm_bridge::tool::Tool;
use lm_bridge::usage::TokenUsage;
use tokio_util::sync::CancellationToken;

/// Replays a fixed script of steps, and records what it was *sent* each time so
/// a test can assert on what the next request actually carried.
struct ScriptedLlm {
    steps: Mutex<Vec<StepResult>>,
    seen: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedLlm {
    fn new(steps: Vec<StepResult>) -> Self {
        Self {
            steps: Mutex::new(steps),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// The messages sent on request `n` (0-based).
    fn request(&self, n: usize) -> Vec<Message> {
        self.seen.lock().unwrap()[n].clone()
    }
}

#[async_trait]
impl Llm for ScriptedLlm {
    fn model_name(&self) -> &str {
        "scripted"
    }

    async fn chat(
        &self,
        messages: Vec<Message>,
        _tools: Vec<Tool>,
        _callbacks: LlmCallbacks,
        _cancel: &CancellationToken,
    ) -> Result<StepResult, lm_bridge::errors::LlmError> {
        self.seen.lock().unwrap().push(messages);
        let mut steps = self.steps.lock().unwrap();
        if steps.is_empty() {
            return Ok(StepResult {
                message: Message::assistant_text("done"),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            });
        }
        Ok(steps.remove(0))
    }
}

fn thinking_step(think: &str, text: &str, call: Option<ToolCall>) -> StepResult {
    let mut content = vec![ContentPart::Think {
        think: think.to_string(),
    }];
    if !text.is_empty() {
        content.push(ContentPart::text(text));
    }
    StepResult {
        message: Message {
            role: Role::Assistant,
            content,
            tool_calls: call.into_iter().collect(),
            tool_call_id: None,
            tools: Vec::new(),
            partial: false,
        },
        usage: TokenUsage::empty(),
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_reason: None,
    }
}

/// **The answer a turn produces must be readable afterwards.** This is what the
/// `agent` tool hands back to the parent; without it every delegation returned
/// `(subagent returned no text)`.
#[tokio::test]
async fn the_final_answer_is_recorded_and_readable() {
    let llm = ScriptedLlm::new(vec![thinking_step("", "the method is delay_for", None)]);
    let mut agent = Agent::new(std::env::temp_dir(), "sys");

    agent
        .prompt(&llm, "what is it?", &CancellationToken::new())
        .await
        .expect("the turn runs");

    assert_eq!(
        agent.last_assistant_text().as_deref(),
        Some("the method is delay_for"),
        "the model's answer must survive the turn"
    );
}

/// **Preserved Thinking needs reasoning in the history to replay.** On a turn
/// that ends in a tool call, the reasoning stays with the text, so the next
/// request carries it on every step of an agentic run.
#[tokio::test]
async fn reasoning_from_a_tool_calling_turn_reaches_the_next_request() {
    let llm = ScriptedLlm::new(vec![
        thinking_step(
            "I should read the file first",
            "reading it",
            Some(ToolCall::new("tc-1", "list_dir").with_arguments("{\"path\": \".\"}")),
        ),
        thinking_step("", "all done", None),
    ]);
    let mut agent = Agent::new(std::env::temp_dir(), "sys");

    agent
        .prompt(&llm, "go", &CancellationToken::new())
        .await
        .expect("the turn runs");

    // The second request is the one that had to carry the first step's turn.
    let second = llm.request(1);
    let reasoning: Vec<&str> = second
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|p| match p {
            ContentPart::Think { think } => Some(think.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        reasoning
            .iter()
            .any(|t| t.contains("I should read the file first")),
        "the previous step's reasoning must be replayed: {second:?}"
    );

    let text: String = second.iter().map(Message::text_content).collect();
    assert!(
        text.contains("reading it"),
        "and so must the model's own narration: {text}"
    );
}

/// The tool call itself still reaches the next request, paired with its result —
/// recording the whole message must not disturb the exchange the providers check.
#[tokio::test]
async fn the_tool_exchange_stays_well_formed() {
    let llm = ScriptedLlm::new(vec![
        thinking_step(
            "checking",
            "",
            Some(ToolCall::new("tc-1", "list_dir").with_arguments("{\"path\": \".\"}")),
        ),
        thinking_step("", "finished", None),
    ]);
    let mut agent = Agent::new(std::env::temp_dir(), "sys");
    agent
        .prompt(&llm, "go", &CancellationToken::new())
        .await
        .expect("the turn runs");

    let second = llm.request(1);
    let answered: std::collections::HashSet<&str> = second
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    let mut calls = 0;
    for message in &second {
        for call in &message.tool_calls {
            calls += 1;
            assert!(
                answered.contains(call.id.as_str()),
                "tool call {} would reach the provider unanswered",
                call.id
            );
        }
    }
    assert_eq!(calls, 1, "the tool call must be replayed: {second:?}");
}

/// A step that produced nothing at all must not wedge the history with an empty
/// assistant message — Moonshot and Z.AI both reject one.
#[tokio::test]
async fn an_empty_step_does_not_wedge_the_history() {
    let llm = ScriptedLlm::new(vec![
        thinking_step("", "", None),
        thinking_step("", "recovered", None),
    ]);
    let mut agent = Agent::new(std::env::temp_dir(), "sys");
    let _ = agent.prompt(&llm, "go", &CancellationToken::new()).await;

    for message in agent.context_history() {
        let sendable = !message.tool_calls.is_empty()
            || message.content.iter().any(|p| match p {
                ContentPart::Text { text } => !text.trim().is_empty(),
                ContentPart::Think { .. } => false,
                _ => true,
            });
        assert!(
            sendable,
            "an unsendable message reached the history: {message:?}"
        );
    }
}
