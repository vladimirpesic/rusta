//! A turn that is stopped — by the repeat breaker or by its step cap — ends
//! with a text-only handoff step, and says why it stopped.

use std::sync::Mutex;

use agent_core::Agent;
use agent_core::agent_loop::llm::{Llm, LlmCallbacks};
use agent_core::agent_loop::types::{StepResult, StopReason};
use agent_core::errors::AgentError;
use async_trait::async_trait;
use lm_bridge::message::{Message, ToolCall};
use lm_bridge::provider::FinishReason;
use lm_bridge::tool::Tool;
use lm_bridge::usage::TokenUsage;
use tokio_util::sync::CancellationToken;

/// Plays back a script of steps, and records what each request carried.
struct ScriptedLlm {
    steps: Mutex<Vec<StepResult>>,
    seen: Mutex<Vec<(Vec<Message>, usize)>>,
}

impl ScriptedLlm {
    fn new(steps: Vec<StepResult>) -> Self {
        Self {
            steps: Mutex::new(steps),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// The text of the last message of request `n`, and how many tools it
    /// declared.
    fn last_message_of(&self, n: usize) -> (String, usize) {
        let seen = self.seen.lock().unwrap();
        let (messages, tools) = &seen[n];
        (messages.last().unwrap().text_content(), *tools)
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
        tools: Vec<Tool>,
        _callbacks: LlmCallbacks,
        _cancel: &CancellationToken,
    ) -> Result<StepResult, lm_bridge::errors::LlmError> {
        self.seen.lock().unwrap().push((messages, tools.len()));
        let mut steps = self.steps.lock().unwrap();
        assert!(!steps.is_empty(), "the script ran out");
        Ok(steps.remove(0))
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

/// The same `list_dir` call, as a fresh tool call each time.
fn same_call(i: usize) -> StepResult {
    step(
        Message::assistant_tool_calls(vec![
            ToolCall::new(format!("call-{i}"), "list_dir").with_arguments(r#"{"path":"."}"#),
        ]),
        FinishReason::ToolCalls,
    )
}

fn text(t: &str) -> StepResult {
    step(Message::assistant_text(t), FinishReason::Stop)
}

fn agent(dir: &std::path::Path) -> Agent {
    Agent::new(dir.to_path_buf(), "sys")
}

/// Twelve identical calls in a row trip the breaker; one more step, told not
/// to call tools, writes the handoff — and a tool call it makes anyway is
/// dropped.
#[tokio::test]
async fn a_repeat_breaker_stop_ends_with_a_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let mut script: Vec<StepResult> = (0..12).map(same_call).collect();
    let mut handoff = Message::assistant_text("Listed the directory; stuck on X; next: try Y.");
    handoff.tool_calls = vec![ToolCall::new("late", "list_dir").with_arguments("{}")];
    script.push(step(handoff, FinishReason::ToolCalls));
    let llm = ScriptedLlm::new(script);
    let mut agent = agent(dir.path());

    let result = agent
        .prompt(&llm, "look around", &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.stop_reason, Some(StopReason::RepeatBreaker));
    assert!(
        result.message.tool_calls.is_empty(),
        "the late call is dropped"
    );
    assert_eq!(
        result.message.text_content(),
        "Listed the directory; stuck on X; next: try Y."
    );
    assert_eq!(llm.requests(), 13);
    let (last, tools) = llm.last_message_of(12);
    assert!(
        last.contains("You were stopped (it kept repeating the same tool call)"),
        "{last}"
    );
    assert!(tools > 0, "the handoff keeps the conversation's tool table");
}

/// With the breaker off, repeats earn no reminders and never stop the turn.
#[tokio::test]
async fn the_repeat_breaker_can_be_switched_off() {
    let dir = tempfile::tempdir().unwrap();
    let mut script: Vec<StepResult> = (0..14).map(same_call).collect();
    script.push(text("done"));
    let llm = ScriptedLlm::new(script);
    let mut agent = agent(dir.path()).with_repeat_breaker(false);

    let result = agent
        .prompt(&llm, "look around", &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.stop_reason, None);
    assert_eq!(result.message.text_content(), "done");
    assert!(
        agent.history().iter().all(|m| !m
            .text_content()
            .contains("<system-reminder>\nThe same tool call")),
        "no repeat reminders"
    );
}

/// A turn that uses every step it may take hands back instead of failing.
#[tokio::test]
async fn a_turn_at_its_step_cap_ends_with_a_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let mut script: Vec<StepResult> = (0..3)
        .map(|i| {
            step(
                Message::assistant_tool_calls(vec![
                    ToolCall::new(format!("c{i}"), "list_dir")
                        .with_arguments(format!(r#"{{"path":".","depth":{}}}"#, i + 1)),
                ]),
                FinishReason::ToolCalls,
            )
        })
        .collect();
    script.push(text("Three listings done; the fourth needs another turn."));
    let llm = ScriptedLlm::new(script);
    let mut agent = agent(dir.path()).with_max_turns(3);

    let result = agent
        .prompt(&llm, "look around", &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.stop_reason, Some(StopReason::MaxTurns));
    assert_eq!(llm.requests(), 4);
    let (last, _) = llm.last_message_of(3);
    assert!(
        last.contains("it used all the steps one turn may take"),
        "{last}"
    );
}

/// A handoff with no words is not a final message.
#[tokio::test]
async fn an_empty_handoff_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let script = vec![same_call(0), text("   ")];
    let llm = ScriptedLlm::new(script);
    let mut agent = agent(dir.path()).with_max_turns(1);

    let err = agent
        .prompt(&llm, "look around", &CancellationToken::new())
        .await
        .unwrap_err();

    assert!(
        matches!(err, AgentError::NoFinalMessage(StopReason::MaxTurns)),
        "{err}"
    );
}
