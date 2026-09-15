//! §8 M7 acceptance tests — sub-coders on a mock OpenAI-compatible backend:
//! four parallel tasks with labeled ≤400-token reports, embedded-mode
//! serialization, the six-turn cap with forced wrap-up, and backend failure
//! degrading to a labeled failed report. The server is hand-rolled tokio
//! TCP (ADR §9: no heavyweight mock frameworks in the tree) with an
//! in-flight gauge for concurrency assertions.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

use rusta_dispatch::{ExecMode, REPORT_TOKEN_CAP, RunTool, TaskSet, run};
use rusta_llm::tokens::estimate_tokens;
use rusta_llm::{Backend, HttpConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the scripted server answers for one request.
enum Reply {
    /// 200 JSON `{"choices":[{"message":{"content": ...}}]}`.
    Content(String),
    /// 500 — exercises retry exhaustion.
    ServerError,
}

/// A scripted completion server with delay + in-flight accounting.
struct Mock {
    port: u16,
    hits: Arc<AtomicU32>,
    max_inflight: Arc<AtomicI32>,
}

impl Mock {
    fn start<F>(delay: Duration, behavior: F) -> Self
    where
        F: Fn(&str) -> Reply + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let listener = TcpListener::from_std(listener).expect("async listener");
        let hits = Arc::new(AtomicU32::new(0));
        let inflight = Arc::new(AtomicI32::new(0));
        let max_inflight = Arc::new(AtomicI32::new(0));
        let (hits_handle, max_handle) = (Arc::clone(&hits), Arc::clone(&max_inflight));
        let behavior: Arc<dyn Fn(&str) -> Reply + Send + Sync> = Arc::new(behavior);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let parts = (
                    Arc::clone(&behavior),
                    Arc::clone(&hits),
                    Arc::clone(&inflight),
                    Arc::clone(&max_inflight),
                    delay,
                );
                tokio::spawn(async move {
                    let (behavior, hits, inflight, max_inflight, delay) = parts;
                    let (mut stream, body) = read_request(stream).await;
                    hits.fetch_add(1, Ordering::SeqCst);
                    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_inflight.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    match behavior(&body) {
                        Reply::Content(content) => {
                            let payload = json!({"choices": [{"message": {"content": content}}]});
                            write_json(&mut stream, 200, &payload.to_string()).await;
                        }
                        Reply::ServerError => {
                            write_json(&mut stream, 500, "{\"error\": \"boom\"}").await;
                        }
                    }
                    inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            port,
            hits: hits_handle,
            max_inflight: max_handle,
        }
    }

    fn hits(&self) -> u32 {
        self.hits.load(Ordering::SeqCst)
    }

    fn max_inflight(&self) -> i32 {
        self.max_inflight.load(Ordering::SeqCst)
    }
}

/// Reads one full HTTP request (headers + Content-Length body); returns the
/// stream and the request body as lossy text.
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

async fn write_json(stream: &mut TcpStream, code: u16, body: &str) {
    let head = format!(
        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.expect("head");
    stream.write_all(body.as_bytes()).await.expect("body");
    let _ = stream.shutdown().await;
}

fn backend(port: u16) -> Arc<Backend> {
    Arc::new(
        Backend::http(HttpConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            ..HttpConfig::default()
        })
        .expect("backend"),
    )
}

/// A `RunTool` recording every call and returning a canned observation.
#[derive(Clone, Default)]
struct Recorder {
    calls: Arc<Mutex<Vec<String>>>,
}

impl RunTool for Recorder {
    async fn run(&self, name: String, input: Value) -> String {
        self.calls
            .lock()
            .expect("recorder")
            .push(format!("{name}: {input}"));
        format!("TOOL RESULT {name} (ok)\nsrc/config.rs:12: fn load()")
    }
}

/// Research turns answer with a tool call; once an observation is in the
/// context, the next completion is the report.
fn research_then_report(body: &str) -> Reply {
    if body.contains("TOOL RESULT") {
        Reply::Content("the config loader lives at src/config.rs:12".to_owned())
    } else {
        Reply::Content(
            "```tool\n{\"name\": \"read\", \"input\": {\"path\": \"src/config.rs\"}}\n```"
                .to_owned(),
        )
    }
}

fn four_tasks() -> TaskSet {
    TaskSet::from_items(&[
        json!({"label": "auth", "task": "trace the auth flow"}),
        json!({"label": "db", "task": "list the migrations"}),
        json!({"label": "api", "task": "map the http routes"}),
        json!({"label": "tests", "task": "find coverage gaps"}),
    ])
    .expect("valid task set")
}

#[tokio::test]
async fn four_parallel_sub_coders_return_labeled_reports() {
    let mock = Mock::start(Duration::from_millis(25), research_then_report);
    let runner = Recorder::default();
    let reports = run(
        backend(mock.port),
        runner.clone(),
        ExecMode::Parallel,
        four_tasks(),
    )
    .await;

    assert_eq!(reports.len(), 4);
    let tasks = four_tasks();
    for report in &reports {
        let own = tasks
            .tasks()
            .iter()
            .find(|t| t.label == report.label)
            .expect("label preserved");
        assert!(!report.failed, "task {} failed", report.label);
        assert_eq!(report.report, "the config loader lives at src/config.rs:12");
        assert!(!report.truncated);
        assert!(estimate_tokens(&report.report) <= REPORT_TOKEN_CAP);
        // §6.8 isolation — fresh context per sub-coder: core prompt,
        // addendum, brief, assistant tool call, observation, report.
        assert_eq!(report.transcript.len(), 6);
        assert!(report.transcript[2].content.contains(&own.brief));
        let foreign = tasks.tasks().iter().find(|t| t.label != own.label).unwrap();
        assert!(
            !report
                .transcript
                .iter()
                .any(|m| m.content.contains(&foreign.brief))
        );
    }
    // One research turn + one report turn per task; requests overlapped.
    assert_eq!(mock.hits(), 8);
    assert!(
        mock.max_inflight() >= 2,
        "parallel mode must overlap requests"
    );
    assert_eq!(runner.calls.lock().expect("recorder").len(), 4);
}

#[tokio::test]
async fn serialized_mode_serves_one_request_at_a_time() {
    // §6.8 embedded constraint: one gate, no overlapping completions.
    let mock = Mock::start(Duration::from_millis(15), |_| {
        Reply::Content("immediate report".to_owned())
    });
    let set = TaskSet::from_items(&[
        json!({"label": "a", "task": "one"}),
        json!({"label": "b", "task": "two"}),
        json!({"label": "c", "task": "three"}),
    ])
    .expect("valid");
    let reports = run(
        backend(mock.port),
        Recorder::default(),
        ExecMode::Serialized,
        set,
    )
    .await;

    assert_eq!(reports.len(), 3);
    assert!(reports.iter().all(|r| !r.failed));
    assert_eq!(mock.hits(), 3);
    assert_eq!(mock.max_inflight(), 1, "serialized mode must never overlap");
}

#[tokio::test]
async fn turn_cap_of_six_forces_a_wrapup_report() {
    // Every research turn asks for another tool; only the wrap-up prompt
    // (§6.8) elicits the report. 6 research turns + 1 wrap-up = 7 requests.
    let mock = Mock::start(Duration::ZERO, |body| {
        if body.contains("Turn budget reached") {
            Reply::Content("wrap-up report".to_owned())
        } else {
            Reply::Content(
                "```tool\n{\"name\": \"glob\", \"input\": {\"pattern\": \"*.rs\"}}\n```".to_owned(),
            )
        }
    });
    let runner = Recorder::default();
    let reports = run(
        backend(mock.port),
        runner.clone(),
        ExecMode::Parallel,
        TaskSet::single("endless research"),
    )
    .await;

    let report = &reports[0];
    assert_eq!(report.report, "wrap-up report");
    assert!(!report.failed);
    assert_eq!(mock.hits(), 7);
    assert_eq!(runner.calls.lock().expect("recorder").len(), 6);
}

#[tokio::test]
async fn persistent_backend_failure_degrades_to_a_failed_report() {
    let mock = Mock::start(Duration::ZERO, |_| Reply::ServerError);
    let reports = run(
        backend(mock.port),
        Recorder::default(),
        ExecMode::Parallel,
        TaskSet::single("doomed research"),
    )
    .await;

    let report = &reports[0];
    assert!(report.failed);
    assert!(report.report.starts_with("RESEARCH FAILED:"));
    let text = rusta_dispatch::labeled(&reports);
    assert!(text.contains("SUB-CODER \"research\" REPORT:\nRESEARCH FAILED:"));
}
