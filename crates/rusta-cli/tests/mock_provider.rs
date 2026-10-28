//! End-to-end runs of the `rusta` binary against a mock model endpoint.
//!
//! These drive the real startup path — config layering, provider selection,
//! request building — and inspect what reaches the wire, which unit tests of
//! the pieces cannot show.

mod cleared_env;

use std::path::Path;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use cleared_env::ClearedEnv;
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A streamed chat completion that answers `text` and stops.
fn sse_answer(text: &str) -> String {
    let chunk = |delta: Value, finish: Value| {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "kimi-for-coding",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    };
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            serde_json::json!({"role": "assistant", "content": text}),
            Value::Null
        ),
        chunk(serde_json::json!({}), serde_json::json!("stop")),
    )
}

/// A mock endpoint that answers every chat completion with `answer`.
async fn mock_endpoint(answer: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_answer(answer)),
        )
        .mount(&server)
        .await;
    server
}

/// `rusta` started in `workdir`, isolated from the user's config, data and
/// keys, with `path` as its PATH.
fn isolated_rusta(home: &Path, workdir: &Path, path: &str) -> Command {
    let mut command = Command::cargo_bin("rusta").expect("rusta binary");
    command
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .cleared_env_path(path)
        .current_dir(workdir);
    command
}

/// Runs `rusta` headless with `args`, isolated from the user's config,
/// data, keys and PATH.
async fn run_rusta(home: &Path, workdir: &Path, args: &[&str]) -> Output {
    run_rusta_with_path(home, workdir, args, "").await
}

/// [`run_rusta`], with `path` as the PATH its tools see.
async fn run_rusta_with_path(home: &Path, workdir: &Path, args: &[&str], path: &str) -> Output {
    let mut command = isolated_rusta(home, workdir, path);
    command.args(args);
    tokio::task::spawn_blocking(move || command.output().expect("rusta runs"))
        .await
        .expect("the run completes")
}

/// Runs the `rusta` REPL with `args`, isolated as [`run_rusta`] is, typing
/// `input` into it; fails when it has not exited within a minute.
async fn run_repl(home: &Path, workdir: &Path, args: &[&str], input: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;

    let home = home.to_path_buf();
    let workdir = workdir.to_path_buf();
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    let input = input.to_string();
    tokio::task::spawn_blocking(move || {
        let mut child = isolated_rusta(&home, &workdir, "")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("rusta runs");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input.as_bytes())
            .expect("the input is written");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while child.try_wait().expect("the REPL's status").is_none() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let output = child.wait_with_output().expect("the killed REPL's output");
                panic!("the REPL hung: {}", String::from_utf8_lossy(&output.stdout));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        child.wait_with_output().expect("the REPL's output")
    })
    .await
    .expect("the run completes")
}

/// Bodies of the chat-completion requests `server` received.
async fn chat_requests(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("request recording is on")
        .into_iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .map(|request| serde_json::from_slice(&request.body).expect("a JSON body"))
        .collect()
}

/// A session another live process is writing refuses a headless run with
/// exit code 4, naming the holder, and nothing reaches the model; with the
/// lock turned off, the run goes ahead.
#[tokio::test]
async fn a_session_another_process_writes_refuses_a_headless_run() {
    let server = mock_endpoint("hello").await;
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let records = home.path().join(".local/share/rusta/records");
    std::fs::create_dir_all(&records).unwrap();
    // Another host's lock is taken to be alive.
    std::fs::write(
        records.join("work.lock"),
        serde_json::json!({
            "version": 1, "token": "t", "pid": 1, "hostname": "elsewhere.example",
            "started_at": 0, "client": "rusta repl"
        })
        .to_string(),
    )
    .unwrap();
    let base_url = server.uri();
    let args = [
        "--provider",
        "kimi",
        "--base-url",
        base_url.as_str(),
        "--api-key",
        "sk-test",
        "--session",
        "work",
        "-p",
        "hello",
    ];

    let output = run_rusta(home.path(), workdir.path(), &args).await;
    assert_eq!(output.status.code(), Some(4));
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        said.contains(
            "Session 'work' is in use by another Rusta process, rusta repl (pid 1 on \
             elsewhere.example)"
        ),
        "{said}"
    );
    assert!(chat_requests(&server).await.is_empty());

    let mut unlocked = isolated_rusta(home.path(), workdir.path(), "");
    unlocked.env("RUSTA_SESSION_LOCK", "0").args(args);
    let output = tokio::task::spawn_blocking(move || unlocked.output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(chat_requests(&server).await.len(), 1);
}

/// A request always carries a cache key, with or without `--session`.
#[tokio::test]
async fn kimi_requests_carry_a_prompt_cache_key_without_a_session() {
    let server = mock_endpoint("hello").await;
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let base_url = server.uri();
    let common = [
        "--provider",
        "kimi",
        "--base-url",
        base_url.as_str(),
        "--api-key",
        "sk-test",
    ];

    let mut args = common.to_vec();
    args.extend(["-p", "say hello"]);
    let output = run_rusta(home.path(), workdir.path(), &args).await;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    let requests = chat_requests(&server).await;
    let key = requests[0]["prompt_cache_key"]
        .as_str()
        .expect("prompt_cache_key is sent without --session")
        .to_string();
    assert_eq!(key.len(), 26, "a lowercase ULID: {key}");

    // A second run gets a key of its own; a named session keys by its name.
    let mut args = common.to_vec();
    args.extend(["--session", "named-session", "-p", "again"]);
    let output = run_rusta(home.path(), workdir.path(), &args).await;
    assert!(output.status.success());
    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["prompt_cache_key"], "named-session");
}

/// A streamed generation the provider cut short: some text, a `write_file`
/// call for `marker.txt`, then `finish_reason` `reason`.
fn sse_cut_short(reason: &str) -> String {
    let chunk = |delta: Value, finish: Value| {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "kimi-for-coding",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    };
    let call = serde_json::json!({
        "index": 0,
        "id": "call_cut",
        "type": "function",
        "function": {
            "name": "write_file",
            "arguments": "{\"path\":\"marker.txt\",\"content\":\"written by a fragment\"}"
        }
    });
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            serde_json::json!({"role": "assistant", "content": "partial answer "}),
            Value::Null
        ),
        chunk(serde_json::json!({"tool_calls": [call]}), Value::Null),
        chunk(serde_json::json!({}), serde_json::json!(reason)),
    )
}

/// A mock endpoint whose first `cut` chat completions are cut short with
/// `reason`; the ones after answer `done`.
async fn cutting_endpoint(reason: &str, cut: usize) -> MockServer {
    let server = MockServer::start().await;
    let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reason = reason.to_string();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            let n = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body = if n < cut {
                sse_cut_short(&reason)
            } else {
                sse_answer("done")
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body)
        })
        .mount(&server)
        .await;
    server
}

fn cut_short_args(server: &MockServer) -> Vec<String> {
    [
        "--provider",
        "kimi",
        "--base-url",
        &server.uri(),
        "--api-key",
        "sk-test",
        "--permission-mode",
        "bypass_permissions",
        "-p",
        "write the marker",
    ]
    .iter()
    .map(ToString::to_string)
    .collect()
}

/// A generation the provider cut short (DeepSeek's
/// `insufficient_system_resource` and `aborted`, Z.AI's `network_error`) is
/// retried like a dropped connection, and the tool call it streamed before
/// the cut never runs.
#[tokio::test]
async fn a_generation_cut_short_is_retried_and_its_tool_call_never_runs() {
    for reason in ["insufficient_system_resource", "aborted", "network_error"] {
        let server = cutting_endpoint(reason, 1).await;
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();

        let args = cut_short_args(&server);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = run_rusta(home.path(), workdir.path(), &args).await;

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{reason}: {stderr}");
        // The backoff carries jitter, so the delay itself is not pinned.
        assert!(
            stderr.contains(&format!(
                "[retry] attempt 1 of 10 failed: API connection error: provider ended the \
                 generation early: finish_reason={reason}; retrying in "
            )),
            "{reason}: {stderr}"
        );
        assert!(
            stderr.contains("(output above is discarded)"),
            "{reason}: {stderr}"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("done"));
        assert_eq!(chat_requests(&server).await.len(), 2, "{reason}");
        assert!(
            !workdir.path().join("marker.txt").exists(),
            "{reason}: the cut generation's write_file ran"
        );
    }
}

/// When every attempt is cut short, the turn fails with that error.
#[tokio::test]
async fn a_generation_cut_short_on_every_attempt_fails_the_turn() {
    let server = cutting_endpoint("aborted", usize::MAX).await;
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();

    let mut args = cut_short_args(&server);
    args.extend(["--max-attempts-per-step".to_string(), "2".to_string()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run_rusta(home.path(), workdir.path(), &args).await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("provider ended the generation early: finish_reason=aborted"),
        "{stderr}"
    );
    assert_eq!(chat_requests(&server).await.len(), 2);
    assert!(!workdir.path().join("marker.txt").exists());
}

/// The REPL takes the next prompt after a turn, and titles the session after
/// the first: on the Kimi Coding Plan, by the plan's title service. Making
/// the title once locked the agent the REPL still held, and the REPL hung
/// after its first turn.
#[tokio::test]
async fn the_repl_titles_the_session_and_takes_the_next_prompt() {
    let server = mock_endpoint("MOCK-DONE").await;
    Mock::given(method("POST"))
        .and(path("/tools"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"title": "Mock session"})),
        )
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let uri = server.uri();
    let output = run_repl(
        home.path(),
        work.path(),
        &[
            "--provider",
            "kimi",
            "--base-url",
            &uri,
            "--api-key",
            "sk-test",
            "--session",
            "repl",
        ],
        "first prompt\nsecond prompt\n/exit\n",
    )
    .await;

    assert!(output.status.success(), "{output:?}");
    assert_eq!(chat_requests(&server).await.len(), 2, "both prompts ran");
    let index: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".local/share/rusta/records/index.json"))
            .expect("the session index"),
    )
    .unwrap();
    let entry = index
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "repl")
        .expect("the session's entry");
    assert_eq!(entry["display_title"], "Mock session");
}

/// A streamed generation that calls the tool `name` with `arguments`.
fn sse_call(name: &str, arguments: Value) -> String {
    let chunk = |delta: Value, finish: Value| {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "kimi-for-coding",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    };
    let call = serde_json::json!({
        "index": 0,
        "id": format!("call_{name}"),
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments.to_string(),
        }
    });
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            serde_json::json!({"role": "assistant", "tool_calls": [call]}),
            Value::Null
        ),
        chunk(serde_json::json!({}), serde_json::json!("tool_calls")),
    )
}

/// A streamed generation that calls `bash` with `command`.
fn sse_bash_call(command: &str) -> String {
    sse_call("bash", serde_json::json!({ "command": command }))
}

/// The session's tools run in `--cwd`, wherever Rusta was started: they
/// once ran in Rusta's own directory, while the prompt named `--cwd`.
#[tokio::test]
async fn tools_run_in_the_given_cwd() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_bash_call("pwd > where.txt")),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_answer("done")),
        )
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let started_in = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();
    let base_url = server.uri();
    let cwd = session.path().to_str().unwrap();
    let output = run_rusta_with_path(
        home.path(),
        started_in.path(),
        &[
            "--provider",
            "kimi",
            "--base-url",
            &base_url,
            "--api-key",
            "sk-test",
            "--yes",
            "--cwd",
            cwd,
            "-p",
            "where are you?",
        ],
        "/usr/bin:/bin",
    )
    .await;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = std::fs::read_to_string(session.path().join("where.txt")).unwrap_or_else(|e| {
        panic!(
            "the command ran in --cwd: {e}\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    if cfg!(windows) {
        // Git Bash's `pwd` spells the folder its own way; the name is the same.
        let name = session.path().file_name().unwrap().to_str().unwrap();
        assert!(recorded.trim().ends_with(name), "{recorded}");
    } else {
        assert_eq!(
            std::path::Path::new(recorded.trim()),
            std::fs::canonicalize(session.path()).unwrap()
        );
    }
    assert!(!started_in.path().join("where.txt").exists());
}

/// A `--cwd` that is not a directory stops Rusta before it starts.
#[tokio::test]
async fn a_cwd_that_is_no_directory_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let file = workdir.path().join("notes.txt");
    std::fs::write(&file, "").unwrap();
    for cwd in [file, workdir.path().join("missing")] {
        let cwd = cwd.to_str().unwrap().to_string();
        let output = run_rusta(home.path(), workdir.path(), &["--cwd", &cwd, "-p", "hi"]).await;
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&format!("--cwd {cwd}")), "{stderr}");
    }
}

/// A mock endpoint whose model presents a plan with `exit_plan_mode`, then
/// answers "done".
async fn planning_endpoint() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_call(
                    "exit_plan_mode",
                    serde_json::json!({ "plan": "## Context\nFix the parser." }),
                )),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_answer("done")),
        )
        .mount(&server)
        .await;
    server
}

/// A headless `--yes --plan` run has nobody to review a plan: it says so up
/// front, the model is told plan mode is on, and its plan stays unapproved —
/// the model hears that nobody could review it, not that the user refused.
#[tokio::test]
async fn a_headless_run_with_nobody_to_review_keeps_the_plan_unapproved() {
    let server = planning_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let base_url = server.uri();
    let output = run_rusta(
        home.path(),
        workdir.path(),
        &[
            "--provider",
            "kimi",
            "--base-url",
            &base_url,
            "--api-key",
            "sk-test",
            "--yes",
            "--plan",
            "-p",
            "plan a fix",
        ],
    )
    .await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("Plan mode is on, but nobody can review a plan"),
        "{stderr}"
    );
    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 2);
    let first = requests[0].to_string();
    assert!(first.contains("Plan mode is active: the user turned it on"));
    assert!(first.contains("\"exit_plan_mode\""));
    assert!(
        !first.contains("enter_plan_mode"),
        "the model cannot enter it"
    );
    assert!(
        requests[1]
            .to_string()
            .contains("Nobody can review this plan in this run"),
        "{}",
        requests[1]
    );
}

/// `plan = true` in the config file is not applied to a headless `--yes`
/// run, which nobody can review a plan in.
#[tokio::test]
async fn a_configured_plan_mode_is_skipped_where_nobody_can_review() {
    let server = mock_endpoint("done").await;
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/rusta");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "plan = true\n").unwrap();
    let base_url = server.uri();
    let output = run_rusta(
        home.path(),
        workdir.path(),
        &[
            "--provider",
            "kimi",
            "--base-url",
            &base_url,
            "--api-key",
            "sk-test",
            "--yes",
            "-p",
            "hello",
        ],
    )
    .await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("plan = true is not applied"), "{stderr}");
    let requests = chat_requests(&server).await;
    assert!(!requests[0].to_string().contains("Plan mode is active"));
}

/// The REPL reviews a plan in every mode, `--yes` included: the plan itself
/// is shown, and `y` approves it.
#[tokio::test]
async fn the_repl_reviews_a_plan_under_yes() {
    let server = planning_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let uri = server.uri();
    let output = run_repl(
        home.path(),
        work.path(),
        &[
            "--provider",
            "kimi",
            "--base-url",
            &uri,
            "--api-key",
            "sk-test",
            "--yes",
            "--plan",
        ],
        "plan a fix\ny\n/exit\n",
    )
    .await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout.contains("[plan review] The agent's plan:\n\n## Context\nFix the parser."),
        "{stdout}"
    );
    let requests = chat_requests(&server).await;
    assert_eq!(requests.len(), 2, "{stdout}");
    assert!(
        requests[1]
            .to_string()
            .contains("The user approved the plan"),
        "{}",
        requests[1]
    );
}

/// Only the user turns swarm mode on, and a `-p` run swarms only with
/// `--swarm`: without it, `agent_swarm` stays offered but starts nothing, and
/// the model hears why; with it, the run starts in swarm mode and the call goes
/// on.
#[tokio::test]
async fn a_headless_run_swarms_only_with_swarm() {
    for swarm in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_call(
                        "agent_swarm",
                        serde_json::json!({ "description": "audit the services" }),
                    )),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_answer("done")),
            )
            .mount(&server)
            .await;
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let base_url = server.uri();
        let mut args = vec![
            "--provider",
            "kimi",
            "--base-url",
            &base_url,
            "--api-key",
            "sk-test",
            "--yes",
            "-p",
            "audit every service",
        ];
        if swarm {
            args.push("--swarm");
        }
        let output = run_rusta(home.path(), workdir.path(), &args).await;

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "stderr: {stderr}");
        let requests = chat_requests(&server).await;
        assert_eq!(requests.len(), 2, "{stderr}");
        let first = requests[0].to_string();
        assert!(
            first.contains("\"agent_swarm\""),
            "the tool list stays the same"
        );
        assert_eq!(
            first.contains("You are now in \\\"agent swarm\\\" mode"),
            swarm,
            "{first}"
        );
        assert_eq!(
            requests[1]
                .to_string()
                .contains("Swarm mode is off, so agent_swarm did not run"),
            !swarm,
            "{}",
            requests[1]
        );
    }
}

/// A coder sub-agent is sent the session's prompt with its role: the rules
/// for tools and risky actions and the project's `AGENTS.md`, but not plan
/// mode, which is the session agent's alone; and it is offered the session's
/// skills. It was sent its role and the environment only, and no `skill`.
#[tokio::test]
async fn a_subagent_is_sent_the_sessions_prompt_with_its_role() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_call(
                    "agent",
                    serde_json::json!({"prompt": "Fix the parser.", "profile": "coder"}),
                )),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_answer("done")),
        )
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    std::fs::write(
        workdir.path().join("AGENTS.md"),
        "Run `make check` before you hand over.",
    )
    .unwrap();
    let skills = workdir.path().join(".rusta/skills");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(
        skills.join("regression-test.md"),
        "Add a failing test first.",
    )
    .unwrap();
    let base_url = server.uri();
    let output = run_rusta(
        home.path(),
        workdir.path(),
        &[
            "--provider",
            "kimi",
            "--base-url",
            &base_url,
            "--api-key",
            "sk-test",
            "--yes",
            "-p",
            "fix the parser",
        ],
    )
    .await;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = chat_requests(&server).await;
    let systems: Vec<String> = requests
        .iter()
        .map(|body| {
            body["messages"][0]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let session = &systems[0];
    let coder_at = systems
        .iter()
        .position(|system| system.contains("You are running as a sub-agent."))
        .unwrap_or_else(|| panic!("no request from the sub-agent: {systems:?}"));
    let coder = &systems[coder_at];
    let coder_tools: Vec<&str> = requests[coder_at]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert!(coder_tools.contains(&"skill"), "{coder_tools:?}");
    for kept in [
        "a file shown by `bash` has to be read again",
        "# Risky actions",
        "Run `make check` before you hand over.",
    ] {
        assert!(session.contains(kept), "{kept}: {session}");
        assert!(coder.contains(kept), "{kept}: {coder}");
    }
    assert!(
        coder.contains("You are a software engineering sub-agent"),
        "{coder}"
    );
    assert!(
        session.contains("Plan mode is a read-only planning state"),
        "{session}"
    );
    assert!(
        !coder.contains("Plan mode is a read-only planning state"),
        "{coder}"
    );
}

/// `/provider refresh` fetches the model list now, says what changed against
/// the list the session started with, and the session takes the new list:
/// `/model` shows it.
#[tokio::test]
async fn provider_refresh_fetches_the_list_now_and_says_what_changed() {
    let server = mock_endpoint("done").await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [
                {"id": "deepseek-flash", "object": "model", "context_window": 1048576,
                 "max_output_tokens": 393216, "input_modalities": ["text", "image"]},
                {"id": "deepseek-v4-pro", "object": "model", "context_window": 1048576,
                 "max_output_tokens": 393216, "input_modalities": ["text"]}
            ]
        })))
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // A fresh cached list, so startup does not refresh it on its own.
    let cache = home.path().join(".cache/rusta");
    std::fs::create_dir_all(&cache).unwrap();
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let model = |id: &str| {
        serde_json::json!({
            "id": id, "display_name": null, "context_window": 1048576,
            "max_output_tokens": 393216, "image_in": false, "efforts": null
        })
    };
    std::fs::write(
        cache.join("deepseek-models-127.0.0.1.json"),
        serde_json::json!({
            "fetched_at": fetched_at,
            "models": [model("deepseek-flash"), model("deepseek-old")]
        })
        .to_string(),
    )
    .unwrap();
    let uri = server.uri();
    let output = run_repl(
        home.path(),
        work.path(),
        &[
            "--provider",
            "deepseek",
            "--base-url",
            &uri,
            "--api-key",
            "sk-test",
        ],
        "/model\n/provider refresh\n/model\n/exit\n",
    )
    .await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    let (before, after) = stdout
        .split_once("deepseek: +1 / -1 models (added deepseek-v4-pro; removed deepseek-old).")
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(before.contains("deepseek-old"), "{before}");
    assert!(after.contains("deepseek-v4-pro"), "{after}");
    assert!(!after.contains("deepseek-old"), "{after}");
}
