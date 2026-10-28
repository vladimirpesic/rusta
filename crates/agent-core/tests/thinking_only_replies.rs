//! A reply of reasoning alone that finished normally is not resent as an
//! empty one: the model is asked for its answer, once a turn.

use std::sync::Mutex;

use agent_core::Agent;
use agent_core::agent_loop::llm::{Llm, LlmCallbacks};
use agent_core::agent_loop::types::{StepResult, StopReason};
use async_trait::async_trait;
use lm_bridge::message::{ContentPart, Message, Role, ToolCall};
use lm_bridge::provider::FinishReason;
use lm_bridge::tool::Tool;
use lm_bridge::usage::TokenUsage;
use tokio_util::sync::CancellationToken;

/// The marker of the reminder that asks for the answer.
const ASKED: &str = "Your previous response had no visible output.";

/// Plays back a script of steps, and keeps the messages of every request.
struct ScriptedLlm {
    steps: Mutex<Vec<StepResult>>,
    requests: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedLlm {
    fn new(steps: Vec<StepResult>) -> Self {
        Self {
            steps: Mutex::new(steps),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
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
        self.requests.lock().unwrap().push(messages);
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

/// A reply of reasoning alone that finished normally.
fn thinking(think: &str) -> StepResult {
    step(
        Message {
            content: vec![ContentPart::think(think)],
            ..Message::assistant_text("")
        },
        FinishReason::Stop,
    )
}

fn text(t: &str) -> StepResult {
    step(Message::assistant_text(t), FinishReason::Stop)
}

/// How often the requests asked for the answer.
fn times_asked(requests: &[Vec<Message>]) -> usize {
    let last = requests.last().expect("a request");
    last.iter()
        .map(|message| message.text_content().matches(ASKED).count())
        .sum()
}

/// The model is asked for its answer, and gives it; the reply of reasoning
/// alone never reaches the provider, which rejects an assistant message with
/// neither content nor tool calls.
#[tokio::test]
async fn a_reasoning_only_reply_is_asked_for_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let llm = ScriptedLlm::new(vec![
        thinking("The fix is already in place."),
        text("Nothing to change: the fix is already in place."),
    ]);
    let mut agent = Agent::new(dir.path().to_path_buf(), "sys");

    let result = agent
        .prompt(&llm, "Fix the login test.", &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        result.message.text_content(),
        "Nothing to change: the fix is already in place."
    );
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(times_asked(&requests), 1);
    assert!(
        !requests[1]
            .iter()
            .any(|message| message.role == Role::Assistant && message.is_thinking_only()),
        "the reply of reasoning alone stays off the wire"
    );
}

/// A second reply of reasoning alone ends the turn as it is; the next turn
/// may ask again.
#[tokio::test]
async fn the_answer_is_asked_for_once_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let llm = ScriptedLlm::new(vec![
        thinking("Hmm."),
        thinking("Still nothing to say."),
        thinking("Again."),
        text("Here it is."),
    ]);
    let mut agent = Agent::new(dir.path().to_path_buf(), "sys");

    let first = agent
        .prompt(&llm, "Anything to report?", &CancellationToken::new())
        .await
        .unwrap();
    assert!(first.message.is_thinking_only());
    assert_eq!(llm.requests().len(), 2);
    assert_eq!(times_asked(&llm.requests()), 1);

    let second = agent
        .prompt(&llm, "And now?", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(second.message.text_content(), "Here it is.");
    let requests = llm.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(times_asked(&requests), 2, "once in each turn");
}

/// A handoff of reasoning alone is asked for once more, like any answer.
#[tokio::test]
async fn a_handoff_of_reasoning_alone_is_asked_for_again() {
    let dir = tempfile::tempdir().unwrap();
    let llm = ScriptedLlm::new(vec![
        step(
            Message::assistant_tool_calls(vec![
                ToolCall::new("c0", "list_dir").with_arguments(r#"{"path":"."}"#),
            ]),
            FinishReason::ToolCalls,
        ),
        thinking("What did I do so far?"),
        text("Listed the directory; reading the files is next."),
    ]);
    let mut agent = Agent::new(dir.path().to_path_buf(), "sys").with_max_turns(1);

    let result = agent
        .prompt(&llm, "look around", &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.stop_reason, Some(StopReason::MaxTurns));
    assert_eq!(
        result.message.text_content(),
        "Listed the directory; reading the files is next."
    );
    let requests = llm.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(times_asked(&requests), 1);
}
