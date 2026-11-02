//! Scheduled prompts are per-session state: they persist beside the records
//! file, come back on resume, stay out of a fresh session, and a due fire
//! becomes a `<cron-fire>` envelope the headless keepalive delivers as a
//! continuation turn.

use std::sync::Mutex;

use agent_core::Agent;
use agent_core::agent_loop::llm::{Llm, LlmCallbacks};
use agent_core::agent_loop::types::StepResult;
use agent_core::cron::now_millis;
use agent_core::records::{Records, sidecar_path};
use async_trait::async_trait;
use lm_bridge::message::Message;
use lm_bridge::provider::FinishReason;
use lm_bridge::usage::TokenUsage;
use tokio_util::sync::CancellationToken;

/// Plays back one scripted step and records what the request carried.
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
}

#[async_trait]
impl Llm for ScriptedLlm {
    fn model_name(&self) -> &str {
        "scripted"
    }

    async fn chat(
        &self,
        messages: Vec<Message>,
        _tools: Vec<lm_bridge::tool::Tool>,
        _callbacks: LlmCallbacks,
        _cancel: &CancellationToken,
    ) -> Result<StepResult, lm_bridge::errors::LlmError> {
        self.seen.lock().unwrap().push(messages);
        let mut steps = self.steps.lock().unwrap();
        assert!(!steps.is_empty(), "the script ran out");
        Ok(steps.remove(0))
    }
}

fn text(t: &str) -> StepResult {
    StepResult {
        message: Message::assistant_text(t),
        usage: TokenUsage::empty(),
        finish_reason: Some(FinishReason::Stop),
        response_id: None,
        stop_reason: None,
    }
}

/// A cron task JSON whose next fire is in the past: a minutely cursor ten
/// minutes behind the current minute fires (and coalesces) on the next tick.
fn due_task(id: &str, recurring: bool, created_at: i64, last_fired_at: i64) -> String {
    format!(
        r#"[{{"id":"{id}","cron":"* * * * *","prompt":"ping","created_at":{created_at},"recurring":{recurring},"last_fired_at":{last_fired_at}}}]"#
    )
}

fn minute_floor(now: i64) -> i64 {
    now - now.rem_euclid(60_000)
}

/// What a session scheduled stays with that session: a resume loads the
/// sidecar, a fresh session under the same records file starts empty.
#[tokio::test]
async fn tasks_restored_on_resume_and_absent_in_a_fresh_session() {
    let dir = tempfile::tempdir().unwrap();
    let records = dir.path().join("records.jsonl");
    let task_id = {
        let agent =
            Agent::new(dir.path().to_path_buf(), "sys").with_records(Records::new(&records));
        let task = agent
            .tool_ctx()
            .cron
            .add("*/5 * * * *", "check the queue", true)
            .unwrap();
        task.id
    };
    assert!(
        sidecar_path(&records, "cron").exists(),
        "scheduling persists the sidecar"
    );

    let resumed = Agent::new(dir.path().to_path_buf(), "sys")
        .with_resumed_records(Records::new(&records))
        .await
        .unwrap();
    let tasks = resumed.tool_ctx().cron.list();
    assert_eq!(tasks.len(), 1, "a resume gets the tasks back");
    assert_eq!(tasks[0].id, task_id);
    assert_eq!(tasks[0].prompt, "check the queue");

    let fresh = Agent::new(dir.path().to_path_buf(), "sys").with_records(Records::new(&records));
    assert!(
        fresh.tool_ctx().cron.list().is_empty(),
        "a new session does not inherit scheduled work"
    );
}

/// One poll of the headless keepalive: `poll_cron` turns a due fire into a
/// pending notification and reports its job as newly due, and the
/// continuation turn's request carries the `<cron-fire>` envelope ahead of
/// the nudge. A recurring task survives its fire; the run stays alive for it.
#[tokio::test]
async fn an_idle_fire_starts_a_headless_turn() {
    let dir = tempfile::tempdir().unwrap();
    let records = dir.path().join("records.jsonl");
    let t0 = minute_floor(now_millis());
    std::fs::write(
        sidecar_path(&records, "cron"),
        due_task("0123abcd", true, t0 - 600_000, t0 - 600_000),
    )
    .unwrap();
    let mut agent = Agent::new(dir.path().to_path_buf(), "sys")
        .with_resumed_records(Records::new(&records))
        .await
        .unwrap();
    let llm = ScriptedLlm::new(vec![text("acknowledged")]);

    assert_eq!(agent.poll_cron(), vec!["0123abcd"], "the due task fired");
    assert!(agent.has_outstanding_headless_work());
    assert_eq!(agent.pending_notifications(), 1);

    agent
        .prompt(
            &llm,
            "(scheduled activity occurred; continue)",
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    let seen = llm.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0]
            .iter()
            .any(|m| m.text_content().contains("<cron-fire jobId=\"0123abcd\"")),
        "the envelope leads the continuation turn's request"
    );
    drop(seen);
    assert_eq!(agent.pending_notifications(), 0);
    assert_eq!(
        agent.tool_ctx().cron.task_count(),
        1,
        "a recurring task lives on after its fire"
    );
    assert!(agent.has_outstanding_headless_work());
}

/// A due one-shot fires, auto-deletes, and the headless run has nothing left
/// to stay alive for — the keepalive loop exits instead of spinning.
#[tokio::test]
async fn a_spent_one_shot_lets_the_headless_run_settle() {
    let dir = tempfile::tempdir().unwrap();
    let records = dir.path().join("records.jsonl");
    let t0 = minute_floor(now_millis());
    std::fs::write(
        sidecar_path(&records, "cron"),
        due_task("89abcdef", false, t0 - 180_000, t0 - 120_000),
    )
    .unwrap();
    let mut agent = Agent::new(dir.path().to_path_buf(), "sys")
        .with_resumed_records(Records::new(&records))
        .await
        .unwrap();
    let llm = ScriptedLlm::new(vec![text("done")]);

    assert_eq!(agent.poll_cron(), vec!["89abcdef"]);
    assert_eq!(
        agent.tool_ctx().cron.task_count(),
        0,
        "a one-shot auto-deletes"
    );

    agent
        .prompt(
            &llm,
            "(scheduled activity occurred; continue)",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let seen = llm.seen.lock().unwrap();
    assert!(
        seen[0]
            .iter()
            .any(|m| m.text_content().contains("recurring=\"false\"")),
        "the one-shot's envelope is delivered"
    );
    drop(seen);
    assert!(
        !agent.has_outstanding_headless_work(),
        "nothing keeps the run alive once the one-shot is spent"
    );
}
