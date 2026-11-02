//! Approvals under `--stream-json`, end to end: each is an `approval_request`
//! event on stdout, and each takes its answer from the next line of stdin —
//! including answers piped in all at once, ahead of the questions. Each call
//! is also a `tool_call` event with its complete arguments, a sub-agent's
//! with its `agent_id`.

mod cleared_env;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use cleared_env::ClearedEnv;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One streamed chat completion: `delta`, then `finish_reason`.
fn sse(delta: Value, finish: &str) -> String {
    sse_deltas(vec![delta], finish)
}

/// One streamed chat completion: each of `deltas` in its own chunk, then
/// `finish_reason`.
fn sse_deltas(deltas: Vec<Value>, finish: &str) -> String {
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "kimi-for-coding",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    };
    let mut body: String = deltas
        .into_iter()
        .map(|delta| format!("data: {}\n\n", chunk(delta, Value::Null)))
        .collect();
    body.push_str(&format!(
        "data: {}\n\ndata: [DONE]\n\n",
        chunk(json!({}), json!(finish))
    ));
    body
}

/// A step that writes `file` — a call the default permission mode asks about.
fn write_step(id: &str, file: &str) -> String {
    let arguments = json!({"path": file, "content": id}).to_string();
    sse(
        json!({
            "role": "assistant",
            "tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": {"name": "write_file", "arguments": arguments},
            }],
        }),
        "tool_calls",
    )
}

/// A step that writes `file`, its arguments streamed in two fragments after
/// the call's header, as providers stream them.
fn fragmented_write_step(id: &str, file: &str) -> String {
    let arguments = json!({"path": file, "content": id}).to_string();
    let (head, tail) = arguments.split_at(arguments.len() / 2);
    let fragment =
        |part: &str| json!({"tool_calls": [{"index": 0, "function": {"arguments": part}}]});
    sse_deltas(
        vec![
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "type": "function",
                    "function": {"name": "write_file", "arguments": ""},
                }],
            }),
            fragment(head),
            fragment(tail),
        ],
        "tool_calls",
    )
}

/// A model that writes `one.txt`, then `two.txt`, then answers.
async fn two_writes_then_done() -> MockServer {
    model_answering([
        write_step("call_one", "one.txt"),
        write_step("call_two", "two.txt"),
    ])
    .await
}

/// A model that answers with `steps` in turn, then with "done".
async fn model_answering<const N: usize>(steps: [String; N]) -> MockServer {
    let server = MockServer::start().await;
    let respond = |body: String| {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    };
    for body in steps {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(respond(body))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(respond(sse(
            json!({"role": "assistant", "content": "done"}),
            "stop",
        )))
        .mount(&server)
        .await;
    server
}

/// Runs `rusta --stream-json` in `repo` with `answers` piped to stdin.
async fn run_with_answers(home: &Path, repo: &Path, base_url: &str, answers: &str) -> Output {
    let answers_file = home.join("answers.txt");
    std::fs::write(&answers_file, answers).unwrap();
    let home = home.to_path_buf();
    let repo = repo.to_path_buf();
    let base_url = base_url.to_string();
    tokio::task::spawn_blocking(move || {
        Command::cargo_bin("rusta")
            .expect("rusta binary")
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .cleared_env_path("")
            .current_dir(&repo)
            .args([
                "--provider",
                "kimi",
                "--base-url",
                &base_url,
                "--api-key",
                "sk-test",
                "--stream-json",
                "-p",
                "write the two files",
            ])
            .stdin(Stdio::from(std::fs::File::open(&answers_file).unwrap()))
            .output()
            .expect("rusta runs")
    })
    .await
    .expect("the run completes")
}

#[tokio::test]
async fn piped_answers_approve_one_call_each() {
    let server = two_writes_then_done().await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    let output = run_with_answers(home.path(), repo.path(), &server.uri(), "y\ny\n").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("stdout must be JSON lines, got {line:?}: {e}"))
        })
        .collect();
    let asked: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "approval_request")
        .collect();
    assert_eq!(asked.len(), 2, "one approval per write: {events:?}");
    assert_eq!(asked[0]["tool"], "write_file");
    assert_eq!(asked[0]["arguments"]["path"], "one.txt");
    assert_eq!(asked[1]["arguments"]["path"], "two.txt");

    assert_eq!(
        std::fs::read_to_string(repo.path().join("one.txt")).unwrap(),
        "call_one"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("two.txt")).unwrap(),
        "call_two",
        "the second answer, piped with the first, approves the second write"
    );
}

/// A call whose arguments stream in fragments is a `tool_call` event with
/// the complete arguments, after its approval and before its result.
#[tokio::test]
async fn a_tool_call_event_carries_the_complete_arguments() {
    let server = model_answering([fragmented_write_step("call_one", "one.txt")]).await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    let output = run_with_answers(home.path(), repo.path(), &server.uri(), "y\n").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let position = |kind: &str| {
        events
            .iter()
            .position(|event| event["type"] == kind)
            .unwrap_or_else(|| panic!("no {kind} event: {events:?}"))
    };
    let call = &events[position("tool_call")];
    assert_eq!(call["id"], "call_one");
    assert_eq!(call["name"], "write_file");
    assert_eq!(
        call["arguments"],
        json!({"path": "one.txt", "content": "call_one"})
    );
    assert_eq!(call["status"], "pending");
    assert!(position("approval_request") < position("tool_call"));
    assert!(position("tool_call") < position("tool_result"));
    assert_eq!(
        std::fs::read_to_string(repo.path().join("one.txt")).unwrap(),
        "call_one"
    );
}

/// A command reads nothing of Rusta's input, which carries the approval
/// answers: `read` gets end of input at once, though more lines follow on
/// Rusta's stdin. A command that inherited it waited for the next line and
/// took it — an answer meant for the next approval.
#[tokio::test]
async fn a_command_gets_none_of_the_approval_answers() {
    use std::io::{BufRead, Write};

    let arguments = json!({"command": "read line; echo \"read [$line]\""}).to_string();
    let server = model_answering([sse(
        json!({
            "role": "assistant",
            "tool_calls": [{
                "index": 0,
                "id": "call_read",
                "type": "function",
                "function": {"name": "bash", "arguments": arguments},
            }],
        }),
        "tool_calls",
    )])
    .await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let mut child = Command::cargo_bin("rusta")
        .expect("rusta binary")
        .env_clear()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .cleared_env_path("/usr/bin:/bin")
        .current_dir(repo.path())
        .args([
            "--provider",
            "kimi",
            "--base-url",
            &server.uri(),
            "--api-key",
            "sk-test",
            "--stream-json",
            "-p",
            "read a line",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("rusta runs");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"y\n").unwrap();

    // The result arrives while the next line is still unwritten.
    let (lines, results) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let event: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if event["type"] == "tool_result" {
                let _ = lines.send(event);
            }
        }
    });
    let result = results.recv_timeout(std::time::Duration::from_secs(20));
    stdin.write_all(b"the next answer\n").unwrap();
    drop(stdin);
    let result = result.unwrap_or_else(|_| {
        results
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("a tool result")
    });
    let _ = child.wait();

    let output: String = result["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect();
    assert!(output.contains("read []"), "{result}");
    assert!(!output.contains("the next answer"), "{result}");
}

/// A call of the model's: `name` with `arguments`, as call `id`.
fn call_step(id: &str, name: &str, arguments: Value) -> String {
    sse(
        json!({
            "role": "assistant",
            "tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            }],
        }),
        "tool_calls",
    )
}

/// A sub-agent's tool calls and results are events too, each with the
/// sub-agent's `agent_id`; the session's own carry none.
#[tokio::test]
async fn a_sub_agents_tool_calls_are_events_with_its_id() {
    let server = model_answering([
        call_step("call_agent", "agent", json!({"prompt": "List the files."})),
        call_step("call_list", "list_dir", json!({"path": "."})),
        sse(json!({"role": "assistant", "content": "One file."}), "stop"),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("one.txt"), "one").unwrap();

    let output = run_with_answers(home.path(), repo.path(), &server.uri(), "").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let event = |kind: &str, id: &str| {
        events
            .iter()
            .find(|e| e["type"] == kind && e["id"] == id)
            .unwrap_or_else(|| panic!("no {kind} for {id}: {events:?}"))
    };
    let delegated = event("tool_call", "call_agent");
    assert!(delegated.get("agent_id").is_none(), "{delegated}");
    let listed = event("tool_call", "call_list");
    let agent_id = listed["agent_id"].as_str().expect("the sub-agent's id");
    assert!(agent_id.starts_with("sub-"), "{agent_id}");
    let result = event("tool_result", "call_list");
    assert_eq!(result["agent_id"], agent_id);
    assert!(result["output"].to_string().contains("one.txt"), "{result}");
    assert!(event("tool_result", "call_agent").get("agent_id").is_none());
}
