//! The built-in guardrails and session privacy, end to end: a project's
//! config, workspace trust, the approval prompt and the tool itself, with the
//! released binary's startup path. Each test checks what happened on disk —
//! that a refused edit or command did not happen — not only the decision.
#![cfg(unix)]

mod cleared_env;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use cleared_env::ClearedEnv;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A model that makes `calls` one after another, a tool call per request, and
/// then answers.
struct Script(Vec<(String, Value)>);

impl Respond for Script {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
        let done = body["messages"].as_array().map_or(0, |messages| {
            messages.iter().filter(|m| m["role"] == "tool").count()
        });
        let chunk = |delta: Value, finish: Value| {
            json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "kimi-for-coding",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
        };
        let (delta, finish) = match self.0.get(done) {
            Some((name, arguments)) => (
                json!({"role": "assistant", "tool_calls": [{
                    "index": 0,
                    "id": format!("call_{done}"),
                    "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()},
                }]}),
                json!("tool_calls"),
            ),
            None => (
                json!({"role": "assistant", "content": "done"}),
                json!("stop"),
            ),
        };
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            chunk(delta, Value::Null),
            chunk(json!({}), finish)
        );
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

async fn model(calls: Vec<(&str, Value)>) -> MockServer {
    let server = MockServer::start().await;
    let calls = calls
        .into_iter()
        .map(|(name, arguments)| (name.to_string(), arguments))
        .collect();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(Script(calls))
        .mount(&server)
        .await;
    server
}

fn edit(path: &str, old: &str, new: &str) -> (&'static str, Value) {
    (
        "edit_file",
        json!({"path": path, "old_string": old, "new_string": new}),
    )
}

/// `rusta` in `work`, isolated from the user's config, data and keys, under
/// the common umask 022, with `input` on its standard input (none: no one to
/// answer an approval).
async fn rusta(home: &Path, work: &Path, args: &[&str], input: Option<&str>) -> Output {
    let binary = assert_cmd::cargo::cargo_bin("rusta");
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("umask 022 && exec \"$0\" \"$@\"")
        .arg(binary)
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("RUSTA_API_KEY", "sk-test")
        .cleared_env_path("/usr/bin:/bin")
        .current_dir(work)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let input = input.map(str::to_string);
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    tokio::task::spawn_blocking(move || {
        let mut child = command.spawn().expect("rusta runs");
        if let Some(input) = input {
            use std::io::Write;
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(input.as_bytes())
                .expect("the input is written");
        }
        child.wait_with_output().expect("rusta's output")
    })
    .await
    .expect("the run completes")
}

/// A headless `--yes` run of `calls`: bypass mode, where only the built-in
/// guardrails still ask, and no one can answer.
async fn bypass(home: &Path, work: &Path, calls: Vec<(&str, Value)>) -> Output {
    let server = model(calls).await;
    let uri = server.uri();
    rusta(
        home,
        work,
        &[
            "--provider",
            "kimi",
            "--base-url",
            &uri,
            "--yes",
            "-p",
            "go",
        ],
        None,
    )
    .await
}

/// A project rule named like the secrets guardrail asks about its own file;
/// "yes to all" for it leaves the guardrail asking about `.env`, and the
/// prompts say which rule asks.
#[tokio::test]
async fn a_project_rule_named_like_a_guardrail_cannot_answer_for_it() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("safe.txt"), "A\n").unwrap();
    std::fs::write(work.path().join(".env"), "SECRET=original\n").unwrap();
    std::fs::write(
        work.path().join(".rusta.toml"),
        "[[permissions.rules]]\nname = \"sensitive-files\"\ntool = \"edit_file\"\n\
         path = \"safe.txt\"\naction = \"prompt\"\nscope = \"session\"\n",
    )
    .unwrap();
    let server = model(vec![
        edit("safe.txt", "A", "B"),
        edit(".env", "SECRET=original", "SECRET=changed"),
    ])
    .await;
    let uri = server.uri();
    let output = rusta(
        home.path(),
        work.path(),
        &["--provider", "kimi", "--base-url", &uri],
        Some("go\na\nn\n/exit\n"),
    )
    .await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        std::fs::read_to_string(work.path().join("safe.txt")).unwrap(),
        "B\n"
    );
    assert_eq!(
        std::fs::read_to_string(work.path().join(".env")).unwrap(),
        "SECRET=original\n",
        "{stdout}"
    );
    assert_eq!(stdout.matches("[approve]").count(), 2, "{stdout}");
    assert!(
        stdout.contains("asked by rule \"sensitive-files\" in project config"),
        "{stdout}"
    );
    assert!(stdout.contains("a write to a secrets file"), "{stdout}");
}

/// The git guardrail holds a history-rewriting command in every form, even
/// with permissions bypassed: none of them runs.
#[tokio::test]
async fn a_destructive_git_command_is_held_in_any_form() {
    for command in [
        "git reset --hard",
        "git -C . reset --hard",
        "git --git-dir=.git --work-tree=. reset --hard",
        "git -c core.pager=cat reset --hard",
        "g''it reset --hard",
        "timeout 30 git reset --hard",
        "sh -c 'git reset --hard'",
    ] {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(work.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["init", "-q"]);
        std::fs::write(work.path().join("a.txt"), "committed\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "c"]);
        std::fs::write(work.path().join("a.txt"), "uncommitted\n").unwrap();

        let output = bypass(
            home.path(),
            work.path(),
            vec![("bash", json!({"command": command}))],
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(work.path().join("a.txt")).unwrap(),
            "uncommitted\n",
            "{command}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

/// The secrets guardrail holds an edit through a symlink to `.env`, even with
/// permissions bypassed.
#[tokio::test]
async fn a_symlinked_secret_is_held() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join(".env"), "SECRET=original\n").unwrap();
    std::os::unix::fs::symlink(".env", work.path().join("alias.txt")).unwrap();
    bypass(
        home.path(),
        work.path(),
        vec![edit("alias.txt", "SECRET=original", "SECRET=changed")],
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(work.path().join(".env")).unwrap(),
        "SECRET=original\n"
    );
}

/// Under the common umask 022, everything a session keeps — records, their
/// sidecars, the index, blobs and spilled tool output — is the user's alone.
#[tokio::test]
async fn what_a_session_keeps_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("notes.txt"), "a note\n").unwrap();
    bypass(
        home.path(),
        work.path(),
        vec![
            ("read_file", json!({"path": "notes.txt"})),
            (
                "bash",
                json!({"command": "head -c 200000 /dev/zero | tr '\\0' x"}),
            ),
        ],
    )
    .await;
    let data = home.path().join(".local/share/rusta");
    let mut seen = Vec::new();
    let mut pending = vec![data.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::metadata(&path).unwrap();
            let mode = metadata.permissions().mode() & 0o777;
            let relative = path.strip_prefix(&data).unwrap().display().to_string();
            if metadata.is_dir() {
                assert_eq!(mode, 0o700, "{relative}");
                pending.push(path);
            } else {
                assert_eq!(mode, 0o600, "{relative}");
            }
            seen.push(relative);
        }
    }
    assert_eq!(
        std::fs::metadata(&data).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let all = seen.join(" ");
    for kind in ["records", ".jsonl", "index.json", "blobs", "spill"] {
        assert!(all.contains(kind), "no {kind} among {all}");
    }
}
