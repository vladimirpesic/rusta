//! The TUI in a real pseudo-terminal, against a mock model endpoint: it keeps
//! rendering and taking keys while a turn runs, and an approval the turn
//! waits on can be answered. The driver and the endpoint are Python
//! (`tests/fixtures/tui_driver.py`, `tests/fixtures/mock_llm.py`); run with
//!
//! ```text
//! cargo test -p rusta-cli --features tui --test tui_pty -- --ignored
//! ```
#![cfg(all(feature = "tui", unix))]

mod common;

use std::path::Path;
use std::process::Command;

use common::{Mock, fixture};
use serde_json::Value;

/// Run `scenario` against `mock` and return the screen markers it saw, in
/// seconds since the prompt's Enter.
fn drive(scenario: &str, mock: &Mock, dir: &Path) -> Value {
    let home = dir.join("home");
    let work = dir.join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let output = Command::new("python3")
        .arg("-I")
        .arg(fixture("tui_driver.py"))
        .arg(env!("CARGO_BIN_EXE_rusta"))
        .arg(format!("http://127.0.0.1:{}", mock.port))
        .arg(&home)
        .arg(&work)
        .arg(scenario)
        .output()
        .expect("python3 runs the TUI driver");
    assert!(
        output.status.success(),
        "the driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("the driver reports JSON");
    report["marks"].clone()
}

/// When `key` reached the screen; fails the test when it never did.
fn mark(marks: &Value, key: &str) -> f64 {
    marks[key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key} never reached the screen: {marks}"))
}

/// The texts of the user messages a request carried.
fn user_texts(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["text"].as_str().map(str::to_string))
        .collect()
}

#[test]
#[ignore = "drives the TUI in a pty with python3; run with --features tui -- --ignored"]
fn a_turn_streams_onto_the_screen_as_it_is_generated() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start("text", dir.path());
    let marks = drive("stream", &mock, dir.path());

    let first = mark(&marks, "first_chunk");
    let last = mark(&marks, "last_chunk");
    mark(&marks, "final_answer");
    // The title made after the turn shows on the chat pane.
    mark(&marks, "title");
    assert!(first < 2.5, "the first chunk took {first} s to show");
    assert!(
        last - first >= 1.0,
        "the chunks reached the screen together ({first} s, {last} s), not as they streamed"
    );
}

#[test]
#[ignore = "drives the TUI in a pty with python3; run with --features tui -- --ignored"]
fn an_approval_is_answered_while_its_turn_waits() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start("tool", dir.path());
    let marks = drive("approval", &mock, dir.path());

    mark(&marks, "approval_prompt");
    mark(&marks, "final_answer");
    assert_eq!(
        mock.requests().len(),
        2,
        "the approved tool ran and its result went back to the model"
    );
}

#[test]
#[ignore = "drives the TUI in a pty with python3; run with --features tui -- --ignored"]
fn enter_queues_ctrl_s_steers_and_esc_interrupts_during_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start("text", dir.path());
    let marks = drive("keys", &mock, dir.path());

    for key in ["queued", "steered", "queued_turn_streams", "interrupting"] {
        mark(&marks, key);
    }
    assert_eq!(
        marks["alive_after_interrupt"],
        Value::Bool(true),
        "Esc interrupts the turn without quitting"
    );
    let requests = mock.requests();
    assert!(requests.len() >= 3, "{requests:?}");
    assert!(
        user_texts(&requests[1])
            .iter()
            .any(|text| text == "steer-one"),
        "the steered message reached the running turn's next request: {:?}",
        requests[1]
    );
    assert_eq!(
        user_texts(&requests[2]).last().map(String::as_str),
        Some("queued-one"),
        "the queued prompt ran as the next turn"
    );
}
