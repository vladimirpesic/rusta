mod cleared_env;

use std::process::Command;

use assert_cmd::assert::OutputAssertExt;
use assert_cmd::cargo::CommandCargoExt;
use cleared_env::ClearedEnv;
use predicates::str::contains;

fn rusta() -> Command {
    Command::cargo_bin("rusta").expect("rusta binary not found")
}

/// `rusta`, isolated from the user's config, data, keys and PATH, for a test
/// that gets as far as reading them. Run on a developer's machine, such a test
/// otherwise finds their configured key, spends it on a real model call, and
/// writes the turn into their session history.
fn isolated_rusta(home: &std::path::Path) -> Command {
    let mut cmd = rusta();
    cmd.env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .cleared_env_path("")
        .current_dir(home);
    cmd
}

#[test]
fn test_version_command() {
    let mut cmd = rusta();
    cmd.arg("version");
    cmd.assert().success().stdout(contains("rusta"));
}

#[test]
fn test_completions_bash() {
    let mut cmd = rusta();
    cmd.args(["completions", "bash"]);
    cmd.assert().success().stdout(contains("rusta"));
}

#[test]
fn test_doctor_rejects_unknown_provider() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = isolated_rusta(home.path());
    cmd.args(["--provider", "unknown", "doctor"]);
    cmd.assert().failure().stdout(contains("unknown provider"));
}

#[test]
fn test_help_lists_stream_json_flag() {
    let mut cmd = rusta();
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(contains("--stream-json"))
        .stdout(contains("--plan"));
}

/// Logs must never reach stdout.
///
/// stdout carries the assistant's answer and, under `--stream-json`, a strict
/// one-JSON-object-per-line stream. A tracing line there makes that stream
/// unparseable — exactly what happened while the subscriber defaulted to stdout.
#[test]
fn test_logs_go_to_stderr_not_stdout() {
    let mut cmd = rusta();
    cmd.env("RUST_LOG", "trace").arg("version");

    let output = cmd.output().expect("rusta version runs");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    for level in ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"] {
        assert!(
            !stdout.contains(level),
            "tracing level `{level}` leaked onto stdout:\n{stdout}"
        );
    }

    // Whatever remains on stdout must still be the plain version output.
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        assert!(line.contains("rusta"), "unexpected stdout line: {line:?}");
    }
}

/// `--stream-json` stdout must be strictly one JSON object per line.
///
/// Runs without an API key, so the turn fails — but any output that *does* reach
/// stdout must still parse: logs go to stderr, and a single stray line (an rmcp
/// `ERROR`, say) would break every downstream JSON parser.
#[test]
fn test_stream_json_stdout_is_pure_json() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = isolated_rusta(home.path());
    cmd.env("RUST_LOG", "trace")
        .args(["--stream-json", "--yes", "-p", "hi"]);

    let output = cmd.output().expect("rusta runs");
    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("non-JSON line on --stream-json stdout: {line:?} ({e})"));
    }
}
