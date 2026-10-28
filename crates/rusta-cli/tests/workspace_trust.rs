//! Workspace trust, end to end: a repository's `.rusta.toml` cannot start
//! its MCP servers until the folder is trusted.
//!
//! The server here is a shell command that leaves a marker file, so whether it
//! started is plain to see.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A mock endpoint that answers every chat completion with `done`.
async fn mock_endpoint() -> MockServer {
    let server = MockServer::start().await;
    let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "kimi-for-coding",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            serde_json::json!({"role": "assistant", "content": "done"}),
            serde_json::Value::Null
        ),
        chunk(serde_json::json!({}), serde_json::json!("stop")),
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server
}

/// Runs `rusta` in `repo` with `args` and `envs`, isolated from the user's
/// config, data, keys and PATH.
async fn rusta(home: &Path, repo: &Path, envs: &[(&str, &str)], args: &[&str]) -> Output {
    let home = home.to_path_buf();
    let repo = repo.to_path_buf();
    let envs: Vec<(String, String)> = envs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    tokio::task::spawn_blocking(move || {
        Command::cargo_bin("rusta")
            .expect("rusta binary")
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("PATH", "")
            .envs(envs)
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The server command a hostile repository might ship, as `.rusta.toml`.
fn server_config(marker: &Path, word: &str) -> String {
    format!(
        "[[mcp_servers]]\nname = \"probe\"\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"echo {word} > {}\"]\nstartup_timeout_ms = 2000\n",
        marker.display()
    )
}

#[tokio::test]
async fn a_repository_cannot_start_its_mcp_servers_until_trusted() {
    let server = mock_endpoint().await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let marker = repo.path().join("server-started");
    let config_path = repo.path().join(".rusta.toml");
    std::fs::write(&config_path, server_config(&marker, "started")).unwrap();
    let base_url = server.uri();
    let session = [
        "--provider",
        "kimi",
        "--base-url",
        base_url.as_str(),
        "--api-key",
        "sk-test",
        "-p",
        "hi",
    ];

    // Untrusted: the run goes ahead without the server, and says so.
    let output = rusta(home.path(), repo.path(), &[], &session).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("done"));
    let text = stderr(&output);
    assert!(text.contains("rusta: leaving out 1 setting from"), "{text}");
    assert!(
        text.contains("MCP server \"probe\" runs `/bin/sh -c 'echo started > "),
        "{text}"
    );
    assert!(
        !marker.exists(),
        "an untrusted repository's server must not start"
    );

    // `doctor` does not start it either, and says why.
    let output = rusta(home.path(), repo.path(), &[], &["doctor"]).await;
    assert!(
        stdout(&output).contains("warn   trust: .rusta.toml: 1 setting left out"),
        "{}",
        stdout(&output)
    );
    assert!(!marker.exists(), "doctor must not start it");

    // Trusted for one run by the environment, and only for that run.
    let trusted_run = [("RUSTA_TRUST_WORKSPACE", "1")];
    let output = rusta(home.path(), repo.path(), &trusted_run, &session).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stderr(&output).contains("leaving out"));
    assert!(marker.exists(), "trusted for the run, the server starts");
    std::fs::remove_file(&marker).unwrap();
    rusta(home.path(), repo.path(), &[], &session).await;
    assert!(
        !marker.exists(),
        "the environment's trust is not remembered"
    );

    // Trusted with `rusta trust`, which shows what it trusts.
    let output = rusta(home.path(), repo.path(), &[], &["trust", "-y"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("MCP server \"probe\" runs `/bin/sh"),
        "{text}"
    );
    assert!(text.contains("Trusted "), "{text}");
    let output = rusta(home.path(), repo.path(), &[], &session).await;
    assert!(
        !stderr(&output).contains("leaving out"),
        "{}",
        stderr(&output)
    );
    assert!(marker.exists(), "a trusted folder starts its server");
    std::fs::remove_file(&marker).unwrap();

    // A different command is not what was trusted.
    std::fs::write(&config_path, server_config(&marker, "changed")).unwrap();
    let output = rusta(home.path(), repo.path(), &[], &session).await;
    let text = stderr(&output);
    assert!(
        text.contains("They changed since you trusted this folder on"),
        "{text}"
    );
    assert!(!marker.exists(), "a changed server needs trust again");

    // Revoked.
    let output = rusta(home.path(), repo.path(), &[], &["trust", "--revoke"]).await;
    assert!(
        stdout(&output).contains("is no longer trusted"),
        "{}",
        stdout(&output)
    );
    let output = rusta(home.path(), repo.path(), &[], &["trust", "--list"]).await;
    assert_eq!(stdout(&output).trim(), "No folder is trusted.");
}
