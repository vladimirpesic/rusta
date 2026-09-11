//! The `dispatch` tool end-to-end through the registry on a mock backend
//! (§8 M7): labeled reports re-enter as one observation, and — the §6.8
//! isolation decision — sub-coder reads do not credit the main ledger.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rusta_core::State;
use rusta_core::session::Status;
use rusta_llm::{Backend, HttpConfig};
use rusta_tools::{ShellPolicy, Tools};
use serde_json::json;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Minimal scripted completion server: research turns get a `read` tool
/// call; once an observation is in context, the report.
struct Mock {
    port: u16,
    hits: Arc<AtomicU32>,
}

impl Mock {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let listener = TcpListener::from_std(listener).expect("async listener");
        let hits = Arc::new(AtomicU32::new(0));
        let hits_handle = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let hits = Arc::clone(&hits_handle);
                tokio::spawn(async move {
                    let (mut stream, body) = read_request(stream).await;
                    hits.fetch_add(1, Ordering::SeqCst);
                    let content = if body.contains("TOOL RESULT") {
                        "the loader is registered in src/boot.rs:7".to_owned()
                    } else {
                        "```tool\n{\"name\": \"read\", \"input\": {\"path\": \"src/boot.rs\"}}\n```"
                            .to_owned()
                    };
                    let payload =
                        json!({"choices": [{"message": {"content": content}}]}).to_string();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    stream.write_all(head.as_bytes()).await.expect("head");
                    stream.write_all(payload.as_bytes()).await.expect("body");
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { port, hits }
    }

    fn hits(&self) -> u32 {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn read_request(mut stream: TcpStream) -> (TcpStream, String) {
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.expect("read");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    let body = request
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| String::from_utf8_lossy(&request[i + 4..]).to_string())
        .unwrap_or_default();
    (stream, body)
}

fn registry(dir: &TempDir, port: u16) -> Tools {
    Tools::new(
        dir.path(),
        Arc::new(
            Backend::http(HttpConfig {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                ..HttpConfig::default()
            })
            .expect("backend"),
        ),
        ShellPolicy::standard().expect("policy"),
    )
    .expect("registry")
}

#[tokio::test]
async fn dispatch_returns_labeled_reports_without_ledger_credit() {
    let dir = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
    std::fs::write(dir.path().join("src/boot.rs"), "fn boot() {}\n").expect("fixture");

    let mock = Mock::start();
    let tools = registry(&dir, mock.port);
    let outcome = tools
        .exec(
            State::Exploring,
            "dispatch",
            &json!({"tasks": [
                {"label": "boot", "task": "find the boot path"},
                {"label": "config", "task": "find the config loader"},
            ]}),
        )
        .await;

    assert_eq!(outcome.status, Status::Ok, "{}", outcome.content);
    assert_eq!(
        outcome.content,
        "SUB-CODER \"boot\" REPORT:\nthe loader is registered in src/boot.rs:7\n\n\
         SUB-CODER \"config\" REPORT:\nthe loader is registered in src/boot.rs:7"
    );
    // Two tasks × (research turn + report turn).
    assert_eq!(mock.hits(), 4);
    // §6.8 isolation: sub-coder reads never credit the main ledger — the
    // auto-inject still protects a later edit.
    assert!(tools.editor().ledger().is_empty());

    let observation = outcome.observation("dispatch");
    assert!(
        observation
            .content
            .starts_with("TOOL RESULT dispatch (ok)\n")
    );
}

#[tokio::test]
async fn dispatch_rejects_bad_task_sets_before_spawning() {
    let dir = TempDir::new().expect("tempdir");
    let tools = registry(&dir, 9); // dead port: nothing may be spawned
    let cases = [
        (json!({}), "dispatch input"),
        (json!({"task": "x", "tasks": []}), "dispatch input"),
        (
            json!({"tasks": [{"label": "a", "task": "1"}, {"label": "a", "task": "2"}]}),
            "duplicate label",
        ),
        (
            json!({"tasks": [
                {"label": "1", "task": "a"}, {"label": "2", "task": "b"},
                {"label": "3", "task": "c"}, {"label": "4", "task": "d"},
                {"label": "5", "task": "e"},
            ]}),
            "too many tasks",
        ),
    ];
    for (input, fragment) in cases {
        let outcome = tools.exec(State::Exploring, "dispatch", &input).await;
        assert_eq!(outcome.status, Status::Error, "{}", outcome.content);
        assert!(outcome.content.contains(fragment), "{}", outcome.content);
        assert!(outcome.content.contains("Example:"), "{}", outcome.content);
    }
}
