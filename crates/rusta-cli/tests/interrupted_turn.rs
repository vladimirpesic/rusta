//! A headless run stopped by SIGTERM — what `timeout`, `kill` and a stopped
//! container or CI job send — ends its turn and records it, so the session can
//! be resumed, rather than dying with the turn unwritten.

#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn sigterm_ends_a_headless_run_with_its_turn_recorded() {
    // A model that does not answer in time: the run is mid-request when stopped.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(120)))
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let records = home.path().join("interrupted.jsonl");

    let child = Command::cargo_bin("rusta")
        .expect("rusta binary")
        .env_clear()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env("PATH", "")
        .current_dir(repo.path())
        .args([
            "--provider",
            "kimi",
            "--base-url",
            &server.uri(),
            "--api-key",
            "sk-test",
            "--records",
            records.to_str().unwrap(),
            "-p",
            "count the files",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rusta starts");

    // Wait for the turn's request: the model catalogue is fetched at
    // startup too, before the run is ready for a signal.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .any(|request| request.url.path().ends_with("/chat/completions"))
    {
        assert!(Instant::now() < deadline, "the run never called the model");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = child.id().to_string();
    let sent = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("kill runs");
    assert!(sent.success());

    let output = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .unwrap()
        .expect("the run exits");
    assert_eq!(
        output.status.signal(),
        None,
        "the run ends on its own rather than dying of the signal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = std::fs::read_to_string(&records).expect("the turn is recorded");
    assert!(recorded.contains("count the files"), "{recorded}");
}
