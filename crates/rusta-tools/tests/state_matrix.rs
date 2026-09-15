//! The tool-state matrix test — ADR §8 M7: every (state × tool) cell with
//! representative input, asserting both directions — allowed tools execute,
//! unavailable tools yield the §6.4 corrective note and *physically* do
//! nothing (file bytes unchanged, no process spawned).

use std::sync::Arc;

use rusta_core::session::Status;
use rusta_core::{STATES, State, TOOLS, Tool};
use rusta_llm::{Backend, HttpConfig};
use rusta_tools::{ShellPolicy, Tools};
use serde_json::{Value, json};
use tempfile::TempDir;

fn tools(root: &TempDir) -> Tools {
    Tools::new(
        root.path(),
        Arc::new(
            Backend::http(HttpConfig {
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                ..HttpConfig::default()
            })
            .expect("offline construct"),
        ),
        ShellPolicy::standard().expect("policy"),
    )
    .expect("registry")
}

fn fixture(root: &TempDir) {
    std::fs::create_dir_all(root.path().join("src")).expect("dirs");
    std::fs::write(
        root.path().join("src/main.rs"),
        "fn main() {\n    todo!()\n}\n",
    )
    .expect("fixture");
    std::fs::create_dir_all(root.path().join("target")).expect("ignored dir");
    std::fs::write(root.path().join("target/junk.rs"), "fn ignored() {}\n").expect("junk");
}

/// Representative input per tool; mutation tools target `src/main.rs`.
fn input(tool: Tool) -> Value {
    match tool {
        Tool::Read => json!({"path": "src/main.rs"}),
        Tool::Grep => json!({"pattern": "todo"}),
        Tool::Glob => json!({"pattern": "*.rs"}),
        Tool::MapRefresh => json!({}),
        Tool::MapDrill => json!({"path": "src/main.rs", "name": "main"}),
        Tool::Ask => json!({"question": "proceed?"}),
        Tool::Edit => json!({"path": "src/main.rs", "search": "todo!()", "replace": "\"done\""}),
        Tool::Write => json!({"path": "src/main.rs", "content": "overwritten\n"}),
        Tool::Shell => json!({"command": "touch pwned.txt"}),
        // Never executed in the matrix (see the loop): asserted separately.
        Tool::Dispatch => json!({"task": "unreachable"}),
    }
}

const ORIGINAL: &str = "fn main() {\n    todo!()\n}\n";

#[tokio::test]
async fn every_state_tool_cell_matches_the_availability_table() {
    for state in STATES {
        let dir = TempDir::new().expect("tempdir");
        fixture(&dir);
        // `/auto` semantics: shell approval granted, so the allowed-state
        // shell cell can actually execute (blocked states never reach it).
        let tools = tools(&dir).with_approver(Box::new(rusta_tools::AutoApprove));

        for tool in TOOLS {
            if matches!(tool, Tool::Dispatch) {
                // dispatch would hit the (dead) backend URL; assert gating
                // through availability + the corrective note instead.
                assert_eq!(tool.available_in(state), state.tools().contains(&tool));
                if !tool.available_in(state) {
                    let outcome = tools.exec(state, "dispatch", &json!({"task": "x"})).await;
                    assert_eq!(outcome.status, Status::Error);
                    assert!(
                        outcome.content.contains("not available"),
                        "{}",
                        outcome.content
                    );
                }
                continue;
            }

            let outcome = tools.exec(state, tool.as_str(), &input(tool)).await;
            if tool.available_in(state) {
                assert_eq!(
                    outcome.status,
                    Status::Ok,
                    "{state}/{tool} must succeed: {}",
                    outcome.content
                );
            } else {
                assert_eq!(
                    outcome.status,
                    Status::Error,
                    "{state}/{tool} must be blocked"
                );
                assert!(
                    outcome.content.contains("not available in"),
                    "{state}/{tool} note: {}",
                    outcome.content
                );
                // Mutation is physically unreachable: bytes unchanged, no
                // side-effect file created (shell's touch never ran).
                let bytes = std::fs::read_to_string(dir.path().join("src/main.rs")).expect("src");
                assert_eq!(bytes, ORIGINAL, "{state}/{tool} touched the file");
                assert!(!dir.path().join("pwned.txt").exists());
            }
        }
    }
}

#[tokio::test]
async fn correcting_state_allows_the_mutation() {
    let dir = TempDir::new().expect("tempdir");
    fixture(&dir);
    let tools = tools(&dir);

    // Exploring: write is blocked with the corrective note.
    let blocked = tools
        .exec(State::Exploring, "write", &input(Tool::Write))
        .await;
    assert_eq!(blocked.status, Status::Error);

    // Editing (the state that registers write): the unread overwrite is
    // refused with the read-first remedy...
    let refused = tools
        .exec(State::Editing, "write", &input(Tool::Write))
        .await;
    assert_eq!(refused.status, Status::Error);
    assert!(
        refused.content.contains("has not been read"),
        "{}",
        refused.content
    );

    // ...a read credits the ledger, then the write succeeds and is journaled.
    let read = tools.exec(State::Editing, "read", &input(Tool::Read)).await;
    assert_eq!(read.status, Status::Ok);
    let written = tools
        .exec(State::Editing, "write", &input(Tool::Write))
        .await;
    assert_eq!(written.status, Status::Ok, "{}", written.content);
    let undone = tools.editor().undo_last().expect("undo").expect("entry");
    assert!(undone.existed);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/main.rs")).expect("src"),
        ORIGINAL
    );
}

#[tokio::test]
async fn unknown_tool_names_yield_the_registered_list() {
    let dir = TempDir::new().expect("tempdir");
    let tools = tools(&dir);
    let outcome = tools.exec(State::Exploring, "search", &json!({})).await;
    assert_eq!(outcome.status, Status::Error);
    assert!(
        outcome.content.contains("unknown tool \"search\""),
        "{}",
        outcome.content
    );
    assert!(
        outcome.content.contains("read, grep, glob"),
        "{}",
        outcome.content
    );
}

#[tokio::test]
async fn shell_executes_in_verifying_with_approval_and_minimal_env() {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("f.txt"), "x").expect("fixture");
    let tools = tools(&dir).with_approver(Box::new(rusta_tools::AutoApprove));

    let outcome = tools
        .exec(
            State::Verifying,
            "shell",
            &json!({"command": "env | sort > env.txt; cat f.txt"}),
        )
        .await;
    assert_eq!(outcome.status, Status::Ok, "{}", outcome.content);
    assert!(outcome.content.contains('x'));

    let env = std::fs::read_to_string(dir.path().join("env.txt")).expect("env dump");
    for var in ["PATH=", "HOME=", "LANG="] {
        assert!(env.contains(var), "missing {var} in:\n{env}");
    }
    // Minimal environment: a variable the parent test process certainly
    // has must NOT leak through. (`PWD` is excluded — `sh` sets it itself.)
    assert!(!env.contains("CARGO="), "CARGO leaked through:\n{env}");
    // cwd is the repo root.
    let pwd = tools
        .exec(State::Verifying, "shell", &json!({"command": "pwd"}))
        .await;
    assert_eq!(pwd.status, Status::Ok);
    assert!(
        pwd.content
            .contains(dir.path().file_name().unwrap().to_string_lossy().as_ref()),
        "cwd is not the repo root: {}",
        pwd.content
    );
}

#[tokio::test]
async fn shell_denial_by_default_approver_leaves_no_trace() {
    let dir = TempDir::new().expect("tempdir");
    let tools = tools(&dir); // DenyAll is the default approver.
    let outcome = tools
        .exec(
            State::Verifying,
            "shell",
            &json!({"command": "touch pwned.txt"}),
        )
        .await;
    assert_eq!(outcome.status, Status::Error);
    assert!(
        outcome.content.contains("not approved"),
        "{}",
        outcome.content
    );
    assert!(!dir.path().join("pwned.txt").exists());
}
