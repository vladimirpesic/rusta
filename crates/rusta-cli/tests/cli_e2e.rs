//! §8 M8 acceptance tests — scripted sessions on a mock OpenAI-compatible
//! streaming server (plan §9: hand-rolled tokio TCP, no mock frameworks).
//!
//! Session 1 is the milestone's headline arc: plan → approval → read → edit
//! batch → auto-commit → validator failure → Reflexion repair → second batch
//! → commit → green gate, then `/undo` restores the file *and* reverts the
//! commit (§6.9). Session 2 pins the §6.1 turn cap with the wrap-up capsule.
//! Session 3 pins the `-c` shell-denied default (§6.12). Session 4 pins
//! `/resume`: reopening the log restores history, ledger, undo journal, and
//! the phase machine (§6.10).

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use rusta_cli::agent::AutoGate;
use rusta_cli::config::{Config, Overrides};
use rusta_cli::render::Reporter;
use rusta_cli::{App, Mode};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Shared captured output — what the user would have seen.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("capture")).into_owned()
    }
}

/// A scripted SSE completion server: the closure maps the 1-based request
/// number to the full completion text.
struct Mock {
    port: u16,
    hits: Arc<AtomicU32>,
}

impl Mock {
    fn start<F>(behavior: F) -> Self
    where
        F: Fn(u32) -> String + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let listener = TcpListener::from_std(listener).expect("async");
        let hits = Arc::new(AtomicU32::new(0));
        let behavior: Arc<dyn Fn(u32) -> String + Send + Sync> = Arc::new(behavior);
        let hits_handle = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let (behavior, hits) = (Arc::clone(&behavior), Arc::clone(&hits_handle));
                tokio::spawn(async move {
                    let mut stream = stream;
                    read_request(&mut stream).await;
                    let request_number = hits.fetch_add(1, Ordering::SeqCst) + 1;
                    let content = behavior(request_number);
                    write_sse(&mut stream, &content).await;
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

/// Serves one completion as delta chunks + finish + `[DONE]`.
async fn write_sse(stream: &mut TcpStream, content: &str) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\nConnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).await.expect("head");
    let delta = json!({"choices": [{"delta": {"content": content}}]});
    stream
        .write_all(format!("data: {delta}\n\n").as_bytes())
        .await
        .expect("delta");
    let finish = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
    stream
        .write_all(format!("data: {finish}\n\n").as_bytes())
        .await
        .expect("finish");
    stream.write_all(b"data: [DONE]\n\n").await.expect("done");
    stream.flush().await.expect("flush");
}

/// Reads one full HTTP request (headers + Content-Length body).
async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.expect("read");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = find(&request, b"\r\n\r\n") else {
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
    request
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn git_present() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let ok = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("spawn git")
            .status
            .success()
    };
    assert!(ok(&["init", "--quiet"]));
    assert!(ok(&["config", "user.email", "rusta@test"]));
    assert!(ok(&["config", "user.name", "Rusta Test"]));
    dir
}

/// A red-until-repair scripted backend: plan, read, failing edit, repair.
fn scripted_repair_flow() -> impl Fn(u32) -> String {
    move |request| match request {
        1 => "Plan:\n1. rename fn one to fn two in src/lib.rs".to_owned(),
        2 => "```tool\n{\"name\": \"read\", \"input\": {\"path\": \"src/lib.rs\"}}\n```".to_owned(),
        3 => "src/lib.rs\n<<<<<<< SEARCH\nfn one() {}\n=======\nfn one_renamed() {}\n\
              >>>>>>> REPLACE\n"
            .to_owned(),
        4 => "src/lib.rs\n<<<<<<< SEARCH\nfn one_renamed() {}\n=======\nfn two() {}\n\
              >>>>>>> REPLACE\n"
            .to_owned(),
        _ => "unexpected request".to_owned(),
    }
}

fn app_for(root: &Path, mock: &Mock, max_turns: u32, capture: &Capture, mode: Mode) -> App {
    let config = Config::parse(&format!(
        "[backend]\nbase_url = \"http://127.0.0.1:{}/v1\"\n\
         [agent]\nmax_turns = {max_turns}\nauto_approve = true\n\
         [validate]\ncommands = [\"grep -q \\\"fn two\\\" src/lib.rs\"]\n",
        mock.port
    ))
    .expect("config");
    let app = App::new(
        config,
        &Overrides::default(),
        root.to_path_buf(),
        root.join(".rusta-test-session.jsonl"),
        Reporter::new(Box::new(capture.clone())),
        mode,
    )
    .expect("app");
    // Tests script the plan gate; the auto flag above covers shell approval.
    app.with_plan_gate(Box::new(AutoGate))
}

#[tokio::test]
async fn edit_loop_validates_repairs_and_commits_then_undo_reverts() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = init_repo();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/lib.rs"), "fn one() {}\n").expect("write");

    let mock = Mock::start(scripted_repair_flow());
    let capture = Capture::default();
    let mut app = app_for(root, &mock, 16, &capture, Mode::Repl);

    app.handle_line("rename fn one() to fn two() in src/lib.rs")
        .await;

    // The arc: plan → read → edit → red → repair → green (4 model requests).
    assert_eq!(mock.hits(), 4, "unexpected completion count");
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read");
    assert_eq!(lib, "fn two() {}\n", "repair landed");

    // §6.9: two batches, two auto-commits, both `rusta: <request summary>`.
    let log = app.git.log(5);
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(
        log.iter()
            .all(|line| line.contains("rusta: rename fn one() to fn two() in src/lib.rs"))
    );

    // The machine walked the full §6.4 arc and landed back at Exploring.
    assert_eq!(app.machine.state(), rusta_core::State::Exploring);
    let text = capture.text();
    assert!(text.contains("* applied src/lib.rs"), "{text}");
    assert!(text.contains("(Exploring → Planning)"), "{text}");
    assert!(text.contains("(Verifying → Editing)"), "{text}");
    assert!(text.contains("(Verifying → Exploring)"), "{text}");

    // §6.7: the red feedback reached the model first (a journaled error
    // observation), the green note closed the task — the user only sees the
    // streamed edits and the phase lines above.
    let observations = app.session.events().iter().filter_map(|event| match event {
        rusta_core::Event::ToolResult {
            status, summary, ..
        } => Some((*status, summary)),
        _ => None,
    });
    let mut saw_red = false;
    let mut saw_green = false;
    for (status, summary) in observations {
        if status == rusta_core::Status::Error && summary.contains("validation failed") {
            saw_red = true;
        }
        if summary.contains("Repair attempts left") {
            saw_green = true; // the Reflexion repair round was offered
        }
    }
    assert!(saw_red, "validator feedback journaled for the model");
    assert!(saw_green, "repair budget surfaced to the model");

    // §6.10: the journal carries both edits, both commits, both validations.
    let commits: Vec<&rusta_core::Event> = app
        .session
        .events()
        .iter()
        .filter(|event| matches!(event, rusta_core::Event::Commit { .. }))
        .collect();
    assert_eq!(commits.len(), 2);
    let validations = app
        .session
        .events()
        .iter()
        .filter(|event| matches!(event, rusta_core::Event::ValidationRun { .. }))
        .count();
    assert_eq!(validations, 2, "one per round: red then green");
    assert_eq!(app.batches.len(), 2);
    assert_eq!(app.batches[1].entries, 1);
    assert!(app.batches[1].sha.is_some());

    // `/undo` (§6.9): restore the file via the journal, revert the commit —
    // exactly the acceptance criterion.
    app.handle_line("/undo").await;
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read after undo");
    assert_eq!(lib, "fn one_renamed() {}\n", "journal restore");
    assert_eq!(app.git.log(5).len(), 1, "commit reverted");
    assert!(
        app.git
            .head_message()
            .as_deref()
            .is_some_and(|m| m.starts_with("rusta:"))
    );
    let undo_text = capture.text();
    assert!(undo_text.contains("reverted commit"), "{undo_text}");
}

/// F1 wiring: every user request earns its own §6.7 repair bound — after a
/// surfaced (budget-exhausted) failure, the next request repairs again.
#[tokio::test]
async fn every_request_earns_a_fresh_repair_budget() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = init_repo();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\n").expect("write");

    // The validator always fails, but its output echoes the file's current
    // line — so §6.6 detector (c) never sees the same fingerprint twice.
    let script = |request: u32| -> String {
        let edit = |from: &str, to: &str| {
            format!(
                "src/lib.rs\n<<<<<<< SEARCH\nfn {from}() {{}}\n=======\nfn {to}() {{}}\n>>>>>>> REPLACE\n"
            )
        };
        match request {
            1 => "Plan:\n1. rename fn a step by step".to_owned(),
            2 => edit("a", "b"),
            3 => edit("b", "c"),
            4 => edit("c", "d"),
            5 => edit("d", "e"),
            6 => edit("e", "f"),
            _ => "giving up on the rename for now".to_owned(),
        }
    };
    let mock = Mock::start(script);
    let capture = Capture::default();
    let config = Config::parse(&format!(
        "[backend]\nbase_url = \"http://127.0.0.1:{}/v1\"\n\
         [agent]\nmax_turns = 16\nauto_approve = true\n\
         [validate]\ncommands = [\"grep fn src/lib.rs; exit 1\"]\n",
        mock.port
    ))
    .expect("config");
    let mut app = App::new(
        config,
        &Overrides::default(),
        root.to_path_buf(),
        root.join(".rusta-test-session.jsonl"),
        Reporter::new(Box::new(capture.clone())),
        Mode::Repl,
    )
    .expect("app")
    .with_plan_gate(Box::new(AutoGate));

    // Request 1 burns the whole bound: red → repair(2) → repair(1) →
    // repair(0) → surface. Request 2 starts on a fresh budget.
    app.handle_line("rename fn a to fn f in src/lib.rs").await;
    app.handle_line("keep trying the rename").await;

    assert_eq!(
        mock.hits(),
        7,
        "plan + 4 reds, then fresh-budget repair + wrap-up"
    );
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read");
    assert_eq!(lib, "fn f() {}\n", "the fresh-budget repair landed");

    // Both requests got a *first* repair round ("attempts left: 2").
    let first_rounds = app
        .session
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event,
                rusta_core::Event::ToolResult { summary, .. }
                    if summary.contains("Repair attempts left: 2.")
            )
        })
        .count();
    assert_eq!(first_rounds, 2, "each request earned its own bound");

    // Exhaustion surfaced to the user once — during the first request.
    let text = capture.text();
    assert_eq!(text.matches("repair budget exhausted").count(), 1, "{text}");
}

#[tokio::test]
async fn turn_cap_forces_a_wrap_up_and_returns_control() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    // Endless tool calls; only the wrap-up prompt elicits prose.
    let mock = Mock::start(|request| {
        if request <= 2 {
            "```tool\n{\"name\": \"read\", \"input\": {\"path\": \"README.md\"}}\n```".to_owned()
        } else {
            "wrapped up: read twice, did nothing else".to_owned()
        }
    });
    std::fs::write(root.join("README.md"), "hello\n").expect("write");
    let capture = Capture::default();
    let mut app = app_for(root, &mock, 2, &capture, Mode::Repl);

    app.handle_line("loop forever").await;

    assert_eq!(mock.hits(), 3, "2 capped turns + 1 wrap-up");
    let text = capture.text();
    assert!(text.contains("(turn budget reached"), "{text}");
    assert!(text.contains("wrapped up"), "{text}");
}

#[tokio::test]
async fn oneshot_mode_denies_shell_by_default() {
    // §6.12: non-interactive `-c` runs deny shell without asking — but only
    // once the phase registers it; in read-only states the §6.4 corrective
    // note fires first (both are error observations with remedies).
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mock = Mock::start(|request| match request {
        1 => "Plan:\n1. try the shell".to_owned(),
        2 => {
            "```tool\n{\"name\": \"shell\", \"input\": {\"command\": \"echo hi\"}}\n```".to_owned()
        }
        _ => "proceeded without the shell".to_owned(),
    });
    let capture = Capture::default();
    let mut app = app_for(root, &mock, 16, &capture, Mode::Oneshot);

    app.handle_line("run echo hi").await;

    let text = capture.text();
    assert!(text.contains("* shell (error)"), "{text}");
    let denied = app.session.events().iter().any(|event| match event {
        rusta_core::Event::ToolResult {
            status,
            summary,
            truncated: _,
        } => *status == rusta_core::Status::Error && summary.contains("not approved"),
        _ => false,
    });
    assert!(denied, "shell denial journaled with a remedy");
    assert_eq!(mock.hits(), 3, "plan → denied shell → wrap-up answer");
}

#[tokio::test]
async fn resume_restores_history_ledger_undo_and_phase() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = init_repo();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/lib.rs"), "fn one() {}\n").expect("write");
    let mock = Mock::start(scripted_repair_flow());
    let capture = Capture::default();
    {
        let mut app = app_for(root, &mock, 16, &capture, Mode::Repl);
        app.handle_line("rename fn one() to fn two() in src/lib.rs")
            .await;
        assert_eq!(mock.hits(), 4);
    }

    // A fresh process reopens the same log: §6.10 replay reconstructs
    // history, ledger, undo journal, and the final phase.
    let mock2 = Mock::start(|_| "fresh answer".to_owned());
    let capture2 = Capture::default();
    let mut resumed = app_for(root, &mock2, 16, &capture2, Mode::Repl);
    assert!(!resumed.history.is_empty(), "history replayed");
    assert!(
        resumed
            .tools
            .editor()
            .ledger()
            .has_read(std::path::Path::new("src/lib.rs")),
        "ledger replayed"
    );
    assert_eq!(
        resumed.tools.editor().undo_stack().len(),
        2,
        "journal replayed"
    );
    assert_eq!(resumed.batches.len(), 2, "batch stack rebuilt from events");

    // `/undo` still works across the resume boundary.
    resumed.handle_line("/undo").await;
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read");
    assert_eq!(lib, "fn one_renamed() {}\n", "post-resume undo restores");
    assert_eq!(resumed.git.log(5).len(), 1, "post-resume undo reverts");
}
