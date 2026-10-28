//! The REPL in a real pseudo-terminal, against a mock model endpoint: a line
//! that arrives in one write — a paste, typeahead, an input method's word —
//! is read whole, and the session gets its title after the first turn. The
//! driver and the endpoint are Python (`tests/fixtures/repl_driver.py`,
//! `tests/fixtures/mock_llm.py`); run with
//!
//! ```text
//! cargo test -p rusta-cli --test repl_pty -- --ignored
//! ```
#![cfg(unix)]

mod common;

use std::process::Command;

use common::{Mock, fixture};
use serde_json::Value;

/// Each line goes in at once; each is answered without waiting for another
/// key. rustyline's printer for announcing scheduled prompts once left all
/// but the first key of such a line unread until the next one came.
#[test]
#[ignore = "drives the REPL in a pty with python3; run with -- --ignored"]
fn a_line_that_arrives_at_once_is_read_whole() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start("text", dir.path());
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let output = Command::new("python3")
        .arg("-I")
        .arg(fixture("repl_driver.py"))
        .arg(env!("CARGO_BIN_EXE_rusta"))
        .arg(format!("http://127.0.0.1:{}", mock.port))
        .arg(&home)
        .arg(&work)
        .output()
        .expect("python3 runs the REPL driver");
    assert!(
        output.status.success(),
        "the driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("the driver reports JSON");
    let marks = &report["marks"];
    for key in ["answer", "help", "exited"] {
        assert!(
            marks[key].is_number(),
            "{key} never reached the screen: {marks}"
        );
    }
    assert_eq!(mock.requests().len(), 1, "one prompt, one model request");

    // The title made after the first turn is in the session index.
    let index: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".local/share/rusta/records/index.json"))
            .expect("the session index"),
    )
    .unwrap();
    let entry = index
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "repl-pty")
        .expect("the session's entry");
    assert_eq!(entry["display_title"], "Mock session");
}
