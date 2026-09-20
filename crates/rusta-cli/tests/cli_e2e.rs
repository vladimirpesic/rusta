//! §8 M8 acceptance tests — scripted sessions on a mock OpenAI-compatible
//! streaming server (ADR §9: hand-rolled tokio TCP, no mock frameworks).
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

/// G9 (fourth audit): §6.4 allows at most one pending `ask` per turn.
/// Nothing enforced it — `ask.rs` named the agent loop as the owner and the
/// loop had no such guard — so a completion carrying several asks put that
/// many consecutive blocking prompts in front of the user inside one turn.
///
/// Driven through the real loop with a counting responder: only the first
/// ask reaches the user, and the rest come back as one corrective note, never
/// a silent drop.
#[tokio::test]
async fn only_the_first_ask_of_a_turn_reaches_the_user() {
    use std::sync::Mutex;

    struct Counting(Arc<Mutex<Vec<String>>>);
    impl rusta_tools::Responder for Counting {
        fn reply(&mut self, question: &str) -> String {
            self.0.lock().expect("lock").push(question.to_owned());
            "answered".to_owned()
        }
    }

    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
    std::fs::write(dir.path().join("src/lib.rs"), "fn two() {}\n").expect("seed");

    let asked = Arc::new(Mutex::new(Vec::new()));
    let mock = Mock::start(move |n| match n {
        // One completion, three asks.
        1 => "```tool\n{\"name\": \"ask\", \"input\": {\"question\": \"first?\"}}\n```\n\
              ```tool\n{\"name\": \"ask\", \"input\": {\"question\": \"second?\"}}\n```\n\
              ```tool\n{\"name\": \"ask\", \"input\": {\"question\": \"third?\"}}\n```\n"
            .to_owned(),
        _ => "All clear.\n".to_owned(),
    });

    let capture = Capture::default();
    let mut app = app_for(dir.path(), &mock, 8, &capture, Mode::Repl);
    // Same backend the app would build, with a counting responder installed.
    let backend = Arc::new(
        rusta_llm::Backend::http(rusta_llm::HttpConfig {
            base_url: format!("http://127.0.0.1:{}/v1", mock.port),
            ..rusta_llm::HttpConfig::default()
        })
        .expect("backend"),
    );
    app.tools = Arc::new(
        rusta_tools::Tools::new(
            dir.path(),
            backend,
            rusta_tools::ShellPolicy::standard().expect("policy"),
        )
        .expect("tools")
        .with_responder(Box::new(Counting(Arc::clone(&asked)))),
    );

    app.handle_line("please check the thing").await;

    let questions = asked.lock().expect("lock").clone();
    assert_eq!(
        questions,
        vec!["first?".to_owned()],
        "only the first ask of a turn may reach the user"
    );
    let shown = capture.text();
    assert!(
        shown.contains("ask (error)") || shown.contains("* ask"),
        "the extra asks must surface, not vanish: {shown}"
    );
}

/// Round 9, from a real run: Qwen3-Coder 30B spent 45 minutes and 40 tool
/// calls in `Exploring` and applied nothing. Five times it attempted an edit
/// and five times it was told "edit is not available in Exploring. Draft a
/// plan to enter Planning" — and it never produced prose that `is_plan`
/// recognises.
///
/// The livelock is structural, not a detection-tuning problem: plan
/// detection runs in `handle_prose_turn`, which is only reached by a
/// completion with **no actionable items**. A completion that carries an
/// edit therefore never reaches the gate at all, so no amount of loosening
/// `is_plan` can help it.
///
/// An attempted edit *is* an intent to change, which is exactly what the
/// §6.4 gate exists to put in front of the user. It now drafts the plan, so
/// the user still rules on it and the model is not asked to guess a phrase.
#[tokio::test]
async fn an_edit_attempted_in_exploring_drafts_the_plan_instead_of_looping() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = init_repo();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/lib.rs"), "fn one() {}\n").expect("write");

    // No plan prose anywhere — the completion is an edit block and nothing
    // else, which is what the real model produced.
    let mock = Mock::start(|hit| match hit {
        1 => "src/lib.rs\n\
              <<<<<<< SEARCH\n\
              fn one() {}\n\
              =======\n\
              fn two() {}\n\
              >>>>>>> REPLACE\n"
            .to_owned(),
        _ => "Done — the rename is applied.".to_owned(),
    });
    let capture = Capture::default();
    let mut app = app_for(root, &mock, 16, &capture, Mode::Repl);

    app.handle_line("rename fn one() to fn two() in src/lib.rs")
        .await;

    assert_eq!(
        std::fs::read_to_string(root.join("src/lib.rs")).expect("read"),
        "fn two() {}\n",
        "an edit offered in Exploring must reach the file once the gate \
         approves, not loop against the phase note: {}",
        capture.text()
    );
    let text = capture.text();
    assert!(
        text.contains("(Exploring → Planning)") && text.contains("(Planning → Editing)"),
        "the machine must walk the real §6.4 arc, not skip it: {text}"
    );
    assert!(
        !text.contains("is not available in Exploring"),
        "the model must not be handed the refusal it cannot act on: {text}"
    );
}

/// The `edit` *tool call* half of the same livelock — and the form the real
/// 30B actually used five times. Gating only the text syntax would have left
/// the observed failure exactly where it was, and would have split "one edit
/// mechanism, two syntaxes" (§6.4) in two.
#[tokio::test]
async fn an_edit_tool_call_in_exploring_drafts_the_plan_too() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = init_repo();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/lib.rs"), "fn one() {}\n").expect("write");

    let mock = Mock::start(|hit| match hit {
        // Read first so the ledger is credited (§6.3 read-before-edit),
        // then an `edit` tool call with no plan prose anywhere.
        1 => "```tool\n{\"name\": \"read\", \"input\": {\"path\": \"src/lib.rs\"}}\n```".to_owned(),
        2 => "```tool\n{\"name\": \"edit\", \"input\": {\"path\": \"src/lib.rs\", \
              \"search\": \"fn one() {}\", \"replace\": \"fn two() {}\"}}\n```"
            .to_owned(),
        _ => "Done.".to_owned(),
    });
    let capture = Capture::default();
    let mut app = app_for(root, &mock, 16, &capture, Mode::Repl);

    app.handle_line("rename fn one() to fn two() in src/lib.rs")
        .await;

    assert_eq!(
        std::fs::read_to_string(root.join("src/lib.rs")).expect("read"),
        "fn two() {}\n",
        "the tool-call syntax must be gated the same way: {}",
        capture.text()
    );
    // Exactly one journalled ToolCall per call the model made — the gate
    // must not record the edit twice on its way through.
    let edits = app
        .session
        .events()
        .iter()
        .filter(|event| matches!(event, rusta_core::Event::ToolCall { name, .. } if name == "edit"))
        .count();
    assert_eq!(edits, 1, "the approved edit call is journalled once");
}

/// Round 9, from a real run: Qwen3-Coder ended a `-c` run with "The fix has
/// been applied to the source code" having applied nothing at all, and rusta
/// exited **0** with that sentence as the answer. §6.1 step 3 makes a
/// completion with no actionable items the user's answer, and nothing
/// compared that answer to what the session had actually done.
///
/// Rusta cannot check a model's prose. It can refuse to call a run that
/// offered edits and landed none a success, and say so in one line.
#[tokio::test]
async fn a_run_that_offers_edits_and_lands_none_does_not_report_success() {
    if !git_present() {
        eprintln!("skipping: git not available");
        return;
    }
    let dir = init_repo();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/lib.rs"), "fn one() {}\n").expect("write");

    let mock = Mock::start(|hit| match hit {
        1 => "```tool\n{\"name\": \"read\", \"input\": {\"path\": \"src/lib.rs\"}}\n```".to_owned(),
        // A SEARCH that does not occur in the file: offered, never applied.
        2 => "src/lib.rs\n\
              <<<<<<< SEARCH\n\
              fn nonexistent() {}\n\
              =======\n\
              fn two() {}\n\
              >>>>>>> REPLACE\n"
            .to_owned(),
        _ => "The fix has been applied to the source code.".to_owned(),
    });
    let capture = Capture::default();
    let mut app = app_for(root, &mock, 6, &capture, Mode::Oneshot);

    let ok = app.run_once("rename fn one() to fn two()").await;

    assert!(
        !ok,
        "a run that applied nothing must not report success: {}",
        capture.text()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/lib.rs")).expect("read"),
        "fn one() {}\n",
        "and the file is genuinely untouched"
    );
    assert!(
        capture.text().contains("0 of"),
        "the user is told how many offered edits landed: {}",
        capture.text()
    );

    // A no-op edit (REPLACE == SEARCH) journals an `EditApplied` with
    // identical hashes. Counting it as progress let a real 7B run exit 0
    // having changed nothing with validators red — the same false success
    // this test exists to prevent, one step further in.
    let dir3 = init_repo();
    let root3 = dir3.path();
    std::fs::create_dir_all(root3.join("src")).expect("mkdir");
    std::fs::write(root3.join("src/lib.rs"), "fn one() {}\n").expect("write");
    let mock3 = Mock::start(|hit| match hit {
        1 => "src/lib.rs\n\
              <<<<<<< SEARCH\n\
              fn one() {}\n\
              =======\n\
              fn one() {}\n\
              >>>>>>> REPLACE\n"
            .to_owned(),
        _ => "Done — applied.".to_owned(),
    });
    let capture3 = Capture::default();
    let mut app3 = app_for(root3, &mock3, 6, &capture3, Mode::Oneshot);
    assert!(
        !app3.run_once("rename it").await,
        "a no-op edit changes no file and is not success: {}",
        capture3.text()
    );

    // The control: a run that lands its edit reports success.
    let dir2 = init_repo();
    let root2 = dir2.path();
    std::fs::create_dir_all(root2.join("src")).expect("mkdir");
    std::fs::write(root2.join("src/lib.rs"), "fn one() {}\n").expect("write");
    let mock2 = Mock::start(|hit| match hit {
        1 => "src/lib.rs\n\
              <<<<<<< SEARCH\n\
              fn one() {}\n\
              =======\n\
              fn two() {}\n\
              >>>>>>> REPLACE\n"
            .to_owned(),
        _ => "Done.".to_owned(),
    });
    let capture2 = Capture::default();
    let mut app2 = app_for(root2, &mock2, 6, &capture2, Mode::Oneshot);
    assert!(
        app2.run_once("rename it").await,
        "a run that applied its edit is a success: {}",
        capture2.text()
    );
}
