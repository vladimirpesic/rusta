//! Rusta's keys, end to end. No process a session starts sees a variable Rusta
//! reads a key from: not a `bash` command, a background task, a hook or an MCP
//! server. Every other variable still reaches them, and so does a key that an
//! MCP server is given in its own `env`.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The variables Rusta reads keys from, the config's `api_key_env` among them.
/// Each is set to `withheld-<name>`.
const WITHHELD: &[&str] = &[
    "RUSTA_API_KEY",
    "RUSTA_ZAI_API_KEY",
    "RUSTA_CUSTOM_HEADERS",
    "KIMI_API_KEY",
    "ZAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "OPENAI_API_KEY",
    "WORK_KIMI_KEY",
];

/// A variable Rusta has no use for, which every process inherits.
const ORDINARY: (&str, &str) = ("RUSTA_TEST_ORDINARY", "passed-on");

/// What the MCP server is given in its `env`, under a withheld name.
const GIVEN: (&str, &str) = ("OPENAI_API_KEY", "given-to-the-server");

fn withheld_value(name: &str) -> String {
    format!("withheld-{name}")
}

/// One streamed generation: `delta`, then `finish`.
fn sse(delta: Value, finish: &str) -> String {
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
        chunk(delta, Value::Null),
        chunk(serde_json::json!({}), serde_json::json!(finish)),
    )
}

/// A generation that calls `bash` with `arguments`.
fn bash_call(id: &str, arguments: &Value) -> String {
    sse(
        serde_json::json!({"role": "assistant", "tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": "bash", "arguments": arguments.to_string()},
        }]}),
        "tool_calls",
    )
}

/// A mock model that runs `env` in the foreground, then in the background,
/// and answers `done` from then on.
async fn endpoint(dir: &Path) -> MockServer {
    let server = MockServer::start().await;
    let foreground = format!("env | tee {}", dir.join("foreground.env").display());
    let background = format!("env > {}", dir.join("background.env").display());
    for body in [
        bash_call("call_fg", &serde_json::json!({"command": foreground})),
        bash_call(
            "call_bg",
            &serde_json::json!({
                "command": background,
                "run_in_background": true,
                "description": "print the environment",
            }),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(
                    serde_json::json!({"role": "assistant", "content": "done"}),
                    "stop",
                )),
        )
        .mount(&server)
        .await;
    server
}

/// A headless session in `dir` with every key variable set. Its user config
/// names the key variable `WORK_KIMI_KEY`, and adds a SessionStart hook and an
/// MCP server that write their environments to `dir`.
async fn run_session(home: &Path, dir: &Path, server: &MockServer) -> Output {
    let config_dir = home.join(".config/rusta");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"api_key_env = "WORK_KIMI_KEY"

[[hooks]]
event = "SessionStart"
command = "env > {hook}"

[[mcp_servers]]
name = "env-probe"
command = "sh"
args = ["-c", "env > {mcp}"]
env = {{ {given} = "{given_value}" }}
startup_timeout_ms = 10000
"#,
            hook = dir.join("hook.env").display(),
            mcp = dir.join("mcp.env").display(),
            given = GIVEN.0,
            given_value = GIVEN.1,
        ),
    )
    .unwrap();

    let mut command = Command::cargo_bin("rusta").expect("rusta binary");
    command
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("PATH", "/usr/bin:/bin")
        .env(ORDINARY.0, ORDINARY.1)
        .current_dir(dir)
        .args([
            "--provider",
            "kimi",
            "--base-url",
            &server.uri(),
            "--yes",
            "-p",
            "print the environment",
        ]);
    for name in WITHHELD {
        let value = withheld_value(name);
        if *name == "RUSTA_CUSTOM_HEADERS" {
            command.env(name, format!("X-Gateway-Token: {value}"));
        } else {
            command.env(name, value);
        }
    }
    tokio::task::spawn_blocking(move || command.output().expect("rusta runs"))
        .await
        .expect("the run completes")
}

/// What a process wrote to `file`, failing with the run's output when it
/// wrote nothing.
fn written(file: &Path, output: &Output) -> String {
    std::fs::read_to_string(file).unwrap_or_else(|e| {
        panic!(
            "{} was not written: {e}\nstdout: {}\nstderr: {}",
            file.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

/// `text`, which a process wrote, carries no withheld value, and carries the
/// ordinary variable.
fn assert_keys_withheld(what: &str, text: &str) {
    for name in WITHHELD {
        let value = withheld_value(name);
        assert!(!text.contains(&value), "{what} saw {name}:\n{text}");
    }
    let ordinary = format!("{}={}", ORDINARY.0, ORDINARY.1);
    assert!(
        text.contains(&ordinary),
        "{what} lost {}:\n{text}",
        ORDINARY.0
    );
}

/// No `bash` command, background task, hook or MCP server sees a key
/// variable, and so neither the model nor the records do. Everything else is
/// inherited, and an MCP server's own `env` applies.
#[tokio::test]
async fn no_process_a_session_starts_sees_its_keys() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let server = endpoint(dir.path()).await;

    let output = run_session(home.path(), dir.path(), &server).await;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for file in ["foreground.env", "background.env", "hook.env"] {
        assert_keys_withheld(file, &written(&dir.path().join(file), &output));
    }
    let mcp = written(&dir.path().join("mcp.env"), &output);
    assert_keys_withheld("the MCP server", &mcp);
    assert!(
        mcp.contains(&format!("{}={}", GIVEN.0, GIVEN.1)),
        "the MCP server lost its own env:\n{mcp}"
    );

    // The foreground command's output went to the model and into the records.
    let requests: String = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect();
    assert!(
        requests.contains(ORDINARY.1),
        "the command's output reached the model"
    );
    let records = home.path().join(".local/share/rusta/records");
    let recorded: String = std::fs::read_dir(&records)
        .unwrap_or_else(|e| panic!("{}: {e}", records.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .collect();
    assert!(recorded.contains(ORDINARY.1), "the records hold the output");
    for name in WITHHELD {
        let value = withheld_value(name);
        assert!(!requests.contains(&value), "the model was sent {name}");
        assert!(!recorded.contains(&value), "the records hold {name}");
    }
}
