//! A tool touching a subdirectory with its own `AGENTS.md` makes the next
//! request name that file.

use std::sync::Mutex;

use agent_core::Agent;
use agent_core::agent_loop::llm::{Llm, LlmCallbacks};
use agent_core::agent_loop::types::StepResult;
use async_trait::async_trait;
use lm_bridge::message::{Message, ToolCall};
use lm_bridge::provider::FinishReason;
use lm_bridge::tool::Tool;
use lm_bridge::usage::TokenUsage;
use tokio_util::sync::CancellationToken;

/// Plays back a script, recording each request's messages.
struct ScriptedLlm {
    steps: Mutex<Vec<StepResult>>,
    seen: Mutex<Vec<Vec<Message>>>,
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

fn read(id: &str, path: &str) -> StepResult {
    step(
        Message::assistant_tool_calls(vec![
            ToolCall::new(id, "read_file").with_arguments(format!(r#"{{"path":"{path}"}}"#)),
        ]),
        FinishReason::ToolCalls,
    )
}

#[tokio::test]
async fn touching_a_subdirectory_names_its_instructions_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
    std::fs::create_dir_all(root.join("web/src")).unwrap();
    std::fs::write(root.join("web/AGENTS.md"), "use pnpm").unwrap();
    std::fs::write(root.join("web/src/app.ts"), "export {}").unwrap();

    let llm = ScriptedLlm {
        steps: Mutex::new(vec![
            read("c1", "web/src/app.ts"),
            read("c2", "web/AGENTS.md"),
            read("c3", "web/src/app.ts"),
            step(Message::assistant_text("done"), FinishReason::Stop),
        ]),
        seen: Mutex::new(Vec::new()),
    };
    let prompt = format!(
        "sys\n<!-- From: {} -->\nroot rules",
        root.join("AGENTS.md").display()
    );
    let mut agent = Agent::new(root.clone(), prompt);

    agent
        .prompt(&llm, "look at the app", &CancellationToken::new())
        .await
        .unwrap();

    let seen = llm.seen.lock().unwrap();
    let reminders_in = |n: usize| {
        seen[n]
            .iter()
            .filter(|m| m.text_content().contains("These instruction files apply"))
            .count()
    };
    let web_rules = root.join("web/AGENTS.md").display().to_string();
    assert_eq!(reminders_in(0), 0, "nothing touched yet");
    assert_eq!(reminders_in(1), 1, "named after the first read");
    let reminder = seen[1]
        .iter()
        .find(|m| m.text_content().contains("These instruction files apply"))
        .unwrap()
        .text_content();
    assert!(reminder.contains(&web_rules), "{reminder}");
    assert!(
        !reminder.contains(&format!("- {}\n", root.join("AGENTS.md").display())),
        "the root file is in the prompt: {reminder}"
    );
    assert_eq!(reminders_in(3), 1, "read, so not named again");
}
