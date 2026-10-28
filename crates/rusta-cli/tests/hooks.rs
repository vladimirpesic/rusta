//! User hooks, end to end: `[[hooks]]` in the user config fire with the
//! session payload, a PreToolUse hook can block a tool call, and a project
//! `.rusta.toml` that declares hooks is refused — a cloned repository must
//! not run code.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use assert_cmd::cargo::CommandCargoExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A mock endpoint: the first chat request gets a `bash` tool call, every
/// later one gets a plain `done` answer. Whether the tool ran is visible in
/// the session records.
async fn mock_endpoint() -> MockServer {
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_request: &Request| {
            let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                serde_json::json!({
                    "id": "chatcmpl-test",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "kimi-for-coding",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                })
            };
            let body = if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    chunk(
                        serde_json::json!({
                            "role": "assistant",
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "bash", "arguments": "{\"command\":\"echo hi\"}"}
                            }]
                        }),
                        serde_json::json!("tool_calls")
                    )
                )
            } else {
                format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    chunk(
                        serde_json::json!({"role": "assistant", "content": "done"}),
                        serde_json::Value::Null
                    ),
                    chunk(serde_json::json!({}), serde_json::json!("stop"))
                )
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body)
        })
        .mount(&server)
        .await;
    server
}

/// Runs `rusta` in `repo` isolated from the user's config and data. `hooks`
/// is written to the user config when given. PATH carries just the system
/// directories so hook commands (`sh -c …`) resolve.
async fn rusta(home: &Path, repo: &Path, hooks: Option<&str>, args: &[&str]) -> Output {
    let home = home.to_path_buf();
    let repo = repo.to_path_buf();
    let hooks = hooks.map(ToString::to_string);
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    tokio::task::spawn_blocking(move || {
        let config_dir = home.join(".config/rusta");
        std::fs::create_dir_all(&config_dir).unwrap();
        if let Some(hooks) = hooks {
            std::fs::write(config_dir.join("config.toml"), hooks).unwrap();
        }
        Command::cargo_bin("rusta")
            .expect("rusta binary")
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&repo)
            .args(&args)
            .output()
            .expect("rusta runs")
    })
    .await
    .expect("the run completes")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn session_args(server: &MockServer) -> Vec<String> {
    vec![
        "--provider".into(),
        "kimi".into(),
        "--base-url".into(),
        server.uri(),
        "--api-key".into(),
        "sk-test".into(),
        "--permission-mode".into(),
        "bypass_permissions".into(),
        "-p".into(),
        "run echo".into(),
    ]
}

/// Every session record file the run wrote, wherever the data dir put them.
fn record_files(home: &Path) -> Vec<std::path::PathBuf> {
    let records = home.join(".local/share/rusta/records");
    let mut files: Vec<_> = std::fs::read_dir(&records)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// SessionStart and SessionEnd hooks fire with the session payload, and a
/// blocking PreToolUse hook stops the bash call before it runs.
#[tokio::test]
async fn user_hooks_fire_and_can_block_a_tool_call() {
    let server = mock_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let start_payload = repo.path().join("start-payload.json");
    let end_marker = repo.path().join("end-marker");
    let hooks = format!(
        r#"[[hooks]]
event = "SessionStart"
command = "cat > {}"

[[hooks]]
event = "SessionEnd"
command = "echo ended >> {}"

[[hooks]]
event = "PreToolUse"
matcher = "^bash$"
command = "echo no bash for you 1>&2; exit 2"
timeout = 5
"#,
        start_payload.display(),
        end_marker.display()
    );

    let args = session_args(&server);
    let output = rusta(
        home.path(),
        repo.path(),
        Some(&hooks),
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    assert!(
        output.status.success(),
        "the run finishes: {}",
        stderr(&output)
    );

    // SessionStart: the payload on stdin carries the session facts.
    let payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&start_payload).unwrap()).unwrap();
    assert_eq!(payload["hook_event_name"], "SessionStart");
    assert_eq!(payload["client_type"], "rusta_cli");
    assert_eq!(
        payload["cwd"],
        serde_json::json!(repo.path().canonicalize().unwrap().to_string_lossy())
    );
    assert!(
        payload["session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );

    // SessionEnd ran at shutdown, and said so.
    assert_eq!(
        std::fs::read_to_string(&end_marker).unwrap().trim(),
        "ended"
    );
    assert!(
        stderr(&output).contains("Running SessionEnd hooks…"),
        "{}",
        stderr(&output)
    );

    // The PreToolUse block reached the model as the tool call's result.
    let records: String = record_files(home.path())
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    assert!(
        records.contains("Blocked by hook: no bash for you"),
        "the block reason is in the records: {records}"
    );
    // And the model was asked again after the block — the turn continued.
    assert!(records.contains("done"), "the turn ran to its answer");
}

/// An exit-0 PreToolUse hook allows the call; its stdout joins the call's
/// context.
#[tokio::test]
async fn allowing_pre_tool_use_hook_adds_stdout_to_the_context() {
    let server = mock_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let hooks = r#"[[hooks]]
event = "PreToolUse"
matcher = "^bash$"
command = "echo context from hook"
"#;

    let args = session_args(&server);
    let output = rusta(
        home.path(),
        repo.path(),
        Some(hooks),
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));

    let records: String = record_files(home.path())
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    assert!(
        records.contains("context from hook"),
        "the hook stdout joined the context: {records}"
    );
    assert!(
        records.contains(r#""text":"hi""#),
        "the bash call itself ran: {records}"
    );
}

/// A project `.rusta.toml` declaring hooks is refused: the session does not
/// start, and nothing the hook would have done happened.
#[tokio::test]
async fn project_hooks_are_refused_end_to_end() {
    let server = mock_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let marker = repo.path().join("hook-ran");
    std::fs::write(
        repo.path().join(".rusta.toml"),
        format!(
            "[[hooks]]\nevent = \"SessionStart\"\ncommand = \"touch {}\"\n",
            marker.display()
        ),
    )
    .unwrap();

    let args = session_args(&server);
    let output = rusta(
        home.path(),
        repo.path(),
        None,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    assert!(
        !output.status.success(),
        "a project with hooks must not run"
    );
    let text = stderr(&output);
    assert!(
        text.contains("refuses") && text.contains("[[hooks]]"),
        "the refusal is clear: {text}"
    );
    assert!(!marker.exists(), "the project hook must never have run");
}

/// A mock endpoint that rejects every chat request with a 400, which is
/// never retried.
async fn rejecting_endpoint() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "invalid request", "type": "invalid_request_error"}
        })))
        .mount(&server)
        .await;
    server
}

/// A session whose hooks do not listen for SessionEnd says nothing about
/// SessionEnd hooks when it closes.
#[tokio::test]
async fn a_session_without_session_end_hooks_says_nothing_about_them() {
    let server = mock_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    let args = session_args(&server);
    let output = rusta(
        home.path(),
        repo.path(),
        Some("[[hooks]]\nevent = \"Stop\"\ncommand = \"true\"\n"),
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("SessionEnd"),
        "{}",
        stderr(&output)
    );
}

/// A user config whose one hook appends `ended` to `marker` at SessionEnd.
fn session_end_hook(marker: &Path) -> String {
    format!(
        "[[hooks]]\nevent = \"SessionEnd\"\ncommand = \"echo ended >> {}\"\n",
        marker.display()
    )
}

/// The marker a SessionEnd hook wrote: `ended` once per closed session.
fn session_ends(marker: &Path) -> String {
    std::fs::read_to_string(marker).unwrap_or_default()
}

/// A headless turn the endpoint rejects fails the run, and SessionEnd still
/// runs, once.
#[tokio::test]
async fn session_end_runs_when_a_headless_turn_fails() {
    let server = rejecting_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let marker = repo.path().join("end-marker");

    let args = session_args(&server);
    let output = rusta(
        home.path(),
        repo.path(),
        Some(&session_end_hook(&marker)),
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("Error:"), "{}", stderr(&output));
    assert_eq!(session_ends(&marker), "ended\n");
}

/// A headless run that runs out of steps fails, and SessionEnd still runs,
/// once.
#[tokio::test]
async fn session_end_runs_when_a_headless_run_stops_at_max_turns() {
    let server = mock_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let marker = repo.path().join("end-marker");

    let mut args = session_args(&server);
    args.extend(["--max-turns".to_string(), "1".to_string()]);
    let output = rusta(
        home.path(),
        repo.path(),
        Some(&session_end_hook(&marker)),
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("Max turns exceeded"),
        "{}",
        stderr(&output)
    );
    assert_eq!(session_ends(&marker), "ended\n");
}

/// A prompt file that cannot be read ends the run before any request, with
/// exit code 2, and SessionEnd still runs, once.
#[tokio::test]
async fn session_end_runs_when_the_prompt_cannot_be_read() {
    let server = mock_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let marker = repo.path().join("end-marker");
    let missing = repo.path().join("missing-task.md");

    let mut args = session_args(&server);
    args.truncate(args.len() - 2);
    args.extend(["--prompt-file".to_string(), missing.display().to_string()]);
    let output = rusta(
        home.path(),
        repo.path(),
        Some(&session_end_hook(&marker)),
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(session_ends(&marker), "ended\n");
    let chats = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path().ends_with("/chat/completions"))
        .count();
    assert_eq!(chats, 0, "no turn ran");
}
