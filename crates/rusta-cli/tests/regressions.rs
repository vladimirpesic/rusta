//! Regressions for defects found in the 2026-09-13 audit.

use rusta_cli::config::Overrides;
use rusta_cli::render::Reporter;
use rusta_cli::{App, Config, Mode};

/// C4c: a target repo with an unrelated `skills/` directory used to abort
/// startup with a skill-card parse error.
#[test]
fn a_foreign_skills_directory_does_not_block_startup() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("skills")).expect("dirs");
    std::fs::write(
        dir.path().join("skills/onboarding.md"),
        "# Onboarding\n\nWelcome to the project.\n",
    )
    .expect("write");

    let app = App::new(
        Config::default(),
        &Overrides::default(),
        dir.path().to_path_buf(),
        dir.path().join("s.jsonl"),
        Reporter::new(Box::new(Vec::new())),
        Mode::Repl,
    )
    .expect("startup must not depend on the repo's skills/ directory");

    // And the shipped deck is present even though this repo has no cards.
    assert_eq!(app.deck.cards().len(), 4);
}

/// M4: the session log and its diffs sidecar carry whole file contents, so
/// they must not be created group- or world-readable.
#[cfg(unix)]
#[test]
fn session_logs_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::TempDir::new().expect("tempdir");
    let session = dir.path().join("s.jsonl");
    let _app = App::new(
        Config::default(),
        &Overrides::default(),
        dir.path().to_path_buf(),
        session.clone(),
        Reporter::new(Box::new(Vec::new())),
        Mode::Repl,
    )
    .expect("app");

    let mode = std::fs::metadata(&session)
        .expect("session file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "session log mode was {mode:o}");
}

// ---------------------------------------------------- /add selection dialect

/// Shared captured output — what the user would have seen.
#[derive(Clone, Default)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
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

/// `/add` keeps shell / Aider path semantics: `*.rs` selects at the repo
/// root, `**/*.rs` recurses, and a bare directory expands to its subtree
/// (Aider's `expand_subdir`). Normalizing slash-free patterns to `**/` here
/// — as the model-facing `glob` tool does — would silently pull in whole
/// trees from a pattern the user typed expecting shell behaviour.
///
/// The one thing that must never happen is the old silent dead end: a
/// slash-free pattern that selects nothing names the recursive form and how
/// many files it would select.
#[tokio::test]
async fn add_uses_path_semantics_and_offers_the_recursive_form() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src/deep")).expect("dirs");
    std::fs::write(dir.path().join("root.rs"), "// root\n").expect("write");
    std::fs::write(dir.path().join("src/lib.rs"), "// lib\n").expect("write");
    std::fs::write(dir.path().join("src/deep/mod.rs"), "// deep\n").expect("write");

    let capture = Capture::default();
    let mut app = App::new(
        Config::default(),
        &Overrides::default(),
        dir.path().to_path_buf(),
        dir.path().join("s.jsonl"),
        Reporter::new(Box::new(capture.clone())),
        Mode::Repl,
    )
    .expect("app");

    // Root-level only, as in a shell — and the dead end now teaches the fix.
    app.handle_line("/add *.rs").await;
    let shown = capture.text();
    assert!(shown.contains("added 1 file"), "{shown}");
    assert!(shown.contains("root.rs"), "{shown}");
    assert!(
        !shown.contains("src/lib.rs"),
        "*.rs must not recurse: {shown}"
    );

    // A pattern that selects nothing at the root names the recursive form.
    app.handle_line("/add *.toml").await;
    let shown = capture.text();
    assert!(shown.contains("no files match *.toml"), "{shown}");

    // The recursive form works and is what the hint points at.
    app.handle_line("/add **/*.rs").await;
    assert!(
        capture.text().contains("added 3 file"),
        "{}",
        capture.text()
    );

    // A bare directory expands to its whole subtree.
    app.handle_line("/drop **/*.rs").await;
    app.handle_line("/add src").await;
    let shown = capture.text();
    assert!(
        shown.contains("added 2 file"),
        "directory expansion: {shown}"
    );
    assert!(shown.contains("src/deep/mod.rs"), "{shown}");

    // G5: `.` is the repo root, and a leading `./` is not part of the path.
    // Walk output carries no `./`, so both built a prefix that matched
    // nothing — in the very command whose directory form had just been added.
    app.handle_line("/drop **/*.rs").await;
    let mark = capture.text().len();
    app.handle_line("/add .").await;
    let shown = capture.text()[mark..].to_string();
    for source in ["root.rs", "src/lib.rs", "src/deep/mod.rs"] {
        assert!(
            shown.contains(source),
            "/add . must reach the whole repo, missing {source}: {shown}"
        );
    }

    app.handle_line("/drop **/*.rs").await;
    let mark = capture.text().len();
    app.handle_line("/add ./src").await;
    let shown = capture.text()[mark..].to_string();
    assert!(
        shown.contains("added 2 file"),
        "/add ./src == /add src: {shown}"
    );
}

/// The hint only fires when it would actually help: it names a recursive
/// form that selects something, and never fires for a plain path typo.
#[tokio::test]
async fn the_recursive_hint_is_never_a_second_dead_end() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
    std::fs::write(dir.path().join("src/lib.rs"), "// lib\n").expect("write");

    let capture = Capture::default();
    let mut app = App::new(
        Config::default(),
        &Overrides::default(),
        dir.path().to_path_buf(),
        dir.path().join("s.jsonl"),
        Reporter::new(Box::new(capture.clone())),
        Mode::Repl,
    )
    .expect("app");

    app.handle_line("/add *.rs").await;
    let shown = capture.text();
    assert!(shown.contains("did you mean /add **/*.rs?"), "{shown}");
    assert!(shown.contains("(1 file(s))"), "{shown}");

    // No match anywhere: no hint, because the recursive form is empty too.
    app.handle_line("/add *.zzz").await;
    let tail = capture.text();
    let tail = tail
        .rsplit("no files match *.zzz")
        .next()
        .unwrap_or_default();
    assert!(!tail.contains("did you mean"), "{tail}");

    // A plain path (no metacharacters) never gets a glob hint.
    app.handle_line("/add nope.rs").await;
    let tail = capture.text();
    let tail = tail
        .rsplit("no files match nope.rs")
        .next()
        .unwrap_or_default();
    assert!(!tail.contains("did you mean"), "{tail}");
}

// ------------------------------------------- 2026-09-14 fourth-audit findings

/// G11: the §9 edit corpus is M2's acceptance evidence, and it drove
/// `parse_response` — while the agent drives `parse_items`, which splits on
/// ` ```tool ` fences first. That seam is where F1 lived, and no fixture
/// crossed it. Every fixture now runs through both entry points and must
/// agree, so the corpus covers the path the agent actually takes.
#[test]
fn the_edit_corpus_agrees_across_both_parser_entry_points() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/edit_corpus");
    let mut fixtures: Vec<std::path::PathBuf> = std::fs::read_dir(&corpus)
        .expect("corpus dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    fixtures.sort();
    assert!(
        fixtures.len() >= 15,
        "expected the §9 corpus, found {}",
        fixtures.len()
    );

    for path in fixtures {
        let text = std::fs::read_to_string(&path).expect("fixture");
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let direct = rusta_edit::parse_response(&text);
        let seam = rusta_cli::parse_items(&text, &[]);

        let via_seam: Vec<rusta_edit::EditBlock> = seam
            .items
            .iter()
            .filter_map(|item| match item {
                rusta_cli::Item::Blocks(blocks) => Some(blocks.clone()),
                rusta_cli::Item::Call { .. } => None,
            })
            .flatten()
            .collect();

        assert_eq!(
            via_seam, direct.blocks,
            "{name}: the agent's parser and the edit parser disagree about the blocks"
        );
        assert_eq!(
            seam.commands, direct.commands,
            "{name}: suggested commands differ across the seam"
        );
    }
}

/// G9: §6.4 allows at most one pending `ask` per turn. Nothing enforced it,
/// so a completion carrying several put that many consecutive blocking
/// prompts in front of the user inside one turn.
#[test]
fn one_ask_per_turn_is_parseable_as_such() {
    let completion = "```tool\n{\"name\": \"ask\", \"input\": {\"question\": \"a?\"}}\n```\n\
                      ```tool\n{\"name\": \"ask\", \"input\": {\"question\": \"b?\"}}\n```\n\
                      ```tool\n{\"name\": \"ask\", \"input\": {\"question\": \"c?\"}}\n```\n";
    let parsed = rusta_cli::parse_items(completion, &[]);
    let asks = parsed
        .items
        .iter()
        .filter(|i| matches!(i, rusta_cli::Item::Call { name, .. } if name == "ask"))
        .count();
    // The parser still reports all three — the *loop* is what bounds them,
    // which is where §6.4 puts the bound and where `submit` now enforces it.
    assert_eq!(asks, 3, "the parser reports what the model wrote");
}

/// G12: a ` ```tool ` fence inside a ` ```bash ` block executes. Recorded
/// rather than fixed — markdown fences do not nest, so the inner ``` closes
/// the outer block in any reading, and `parse_response`'s own shell-block
/// collector ends the command there too. Both parsers agree the bash block
/// ended, and the input is textually identical to a legitimate suggested
/// command followed by a legitimate tool call. Pinned so the behaviour is a
/// known property rather than an accident.
#[test]
fn a_tool_fence_after_a_shell_fence_is_a_call_by_design() {
    let text = "```bash\necho hello\n```tool\n{\"name\": \"read\", \"input\": {\"path\": \"a.rs\"}}\n```\n";
    let parsed = rusta_cli::parse_items(text, &[]);
    assert_eq!(parsed.commands, ["echo hello".to_owned()]);
    assert!(
        parsed
            .items
            .iter()
            .any(|i| matches!(i, rusta_cli::Item::Call { name, .. } if name == "read")),
        "the fence after the closed bash block is a call"
    );
}

// ------------------------------------- 2026-09-15 consolidated-audit findings

/// A3(a): a batch must never claim more entries than the journal holds for
/// it. Replay drops workspace-escaping paths; counting them here made
/// `/undo`'s `for _ in 0..batch.entries` loop walk past the batch boundary
/// and revert a previous, unrelated commit.
///
/// A3(b): batch boundaries come from a journaled `BatchBoundary`, not from
/// whether a git commit happened to land — in no-git mode consecutive
/// requests used to merge into one batch.
#[tokio::test]
async fn undo_batches_survive_resume_without_over_consuming() {
    use rusta_core::{Event, Session};

    let repo = tempfile::TempDir::new().expect("repo");
    let outside = tempfile::TempDir::new().expect("outside");
    std::fs::write(repo.path().join("x.txt"), "X_NEW\n").expect("seed");
    std::fs::write(repo.path().join("y.txt"), "Y_NEW\n").expect("seed");
    let log = repo.path().join("s.jsonl");
    {
        let mut session = Session::open(&log).expect("open");
        // Request 1 — one in-repo edit, no commit at all (the no-git path).
        session.record(Event::BatchBoundary).expect("boundary");
        session
            .record_edit("x.txt", true, "X_OLD\n", "X_NEW\n")
            .expect("edit");
        // Request 2 — an escaping edit (replay drops it) plus an in-repo one.
        session.record(Event::BatchBoundary).expect("boundary");
        session
            .record_edit(
                outside.path().join("v.txt").to_str().expect("utf8"),
                true,
                "V\n",
                "V2\n",
            )
            .expect("edit");
        session
            .record_edit("y.txt", true, "Y_OLD\n", "Y_NEW\n")
            .expect("edit");
    }

    let capture = Capture::default();
    let mut app = App::new(
        Config::default(),
        &Overrides::default(),
        repo.path().to_path_buf(),
        log,
        Reporter::new(Box::new(capture.clone())),
        Mode::Repl,
    )
    .expect("app");

    assert_eq!(
        app.batches.iter().map(|b| b.entries).collect::<Vec<_>>(),
        vec![1, 1],
        "two requests, one journal entry each after the escaping edit is dropped"
    );
    assert_eq!(app.tools.editor().undo_stack().len(), 2);

    app.handle_line("/undo").await;
    assert_eq!(
        std::fs::read_to_string(repo.path().join("y.txt")).expect("read"),
        "Y_OLD\n",
        "the last batch is reverted"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("x.txt")).expect("read"),
        "X_NEW\n",
        "the previous batch must NOT be touched"
    );
}

/// A3(c): `/undo` journals a tombstone, so replay cannot resurrect entries
/// for edits that were already undone. Without it: apply A→B, undo to A,
/// edit A→C, restart — and the next `/undo` wrote B over C, silently
/// rewinding the tree to content the user had rejected.
#[tokio::test]
async fn a_resumed_session_does_not_resurrect_undone_edits() {
    use rusta_core::{Event, Session};

    let repo = tempfile::TempDir::new().expect("repo");
    let file = repo.path().join("f.txt");
    let log = repo.path().join("s.jsonl");
    {
        let mut session = Session::open(&log).expect("open");
        session.record(Event::BatchBoundary).expect("boundary");
        session
            .record_edit("f.txt", true, "A\n", "B\n")
            .expect("edit");
        // …the user undid it, which is now journaled.
        session
            .record(Event::UndoApplied { entries: 1 })
            .expect("undo");
        // …then edited by hand to C.
    }
    std::fs::write(&file, "C\n").expect("user edit");

    let capture = Capture::default();
    let mut app = App::new(
        Config::default(),
        &Overrides::default(),
        repo.path().to_path_buf(),
        log,
        Reporter::new(Box::new(capture.clone())),
        Mode::Repl,
    )
    .expect("app");

    assert_eq!(
        app.tools.editor().undo_stack().len(),
        0,
        "the undone entry must not be resurrected"
    );
    assert!(app.batches.is_empty(), "nor its batch");

    app.handle_line("/undo").await;
    assert_eq!(
        std::fs::read_to_string(&file).expect("read"),
        "C\n",
        "the user's own content must survive"
    );
    assert!(
        capture.text().contains("nothing to undo"),
        "{}",
        capture.text()
    );
}

/// A14: `/add`'s chat-set must survive `/resume`. Replay credits the ledger
/// only from `read`/`map_drill` results, and `/add` journalled under a name
/// replay ignored — so the set silently vanished across a restart while the
/// model's note about it replayed regardless.
#[tokio::test]
async fn the_add_chat_set_survives_a_resume() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
    std::fs::write(dir.path().join("src/a.rs"), "// a\n").expect("write");
    std::fs::write(dir.path().join("src/b.rs"), "// b\n").expect("write");
    let log = dir.path().join("s.jsonl");

    {
        let capture = Capture::default();
        let mut app = App::new(
            Config::default(),
            &Overrides::default(),
            dir.path().to_path_buf(),
            log.clone(),
            Reporter::new(Box::new(capture.clone())),
            Mode::Repl,
        )
        .expect("app");
        app.handle_line("/add src").await;
        assert_eq!(app.tools.editor().ledger().len(), 2);
    }

    // A fresh process over the same log.
    let capture = Capture::default();
    let resumed = App::new(
        Config::default(),
        &Overrides::default(),
        dir.path().to_path_buf(),
        log,
        Reporter::new(Box::new(capture.clone())),
        Mode::Repl,
    )
    .expect("resume");

    let ledger = resumed.tools.editor();
    assert_eq!(
        ledger.ledger().len(),
        2,
        "the /add chat-set must survive the restart"
    );
    for path in ["src/a.rs", "src/b.rs"] {
        assert!(
            ledger.ledger().has_read(std::path::Path::new(path)),
            "{path} missing from the resumed chat-set"
        );
    }
}

/// A17 follow-up: a turn abandoned by transport failure keeps its phase —
/// a dropped connection is not a user decision and must not discard an
/// approved plan — *except* in `Verifying`, whose only exits are the two
/// validation verdicts and `UserInterrupt`, and where `edit` is not
/// registered. Keeping the phase there left the model with no reachable
/// move at all; an abandoned verification is not a passed one, so the
/// honest verdict returns it to `Editing`.
#[tokio::test]
async fn an_abandoned_turn_never_strands_the_phase_machine() {
    use rusta_core::{PHASE_EVENTS, STATES, State, Tool};

    // Verifying is the phase with no model-reachable exit: assert that
    // property directly, so this test fails if the table ever changes.
    let model_can_edit_in = |state: State| Tool::Edit.available_in(state);
    assert!(
        !model_can_edit_in(State::Verifying),
        "if `edit` becomes available in Verifying, this hazard is gone"
    );

    // Every other phase has an exit the model itself can cause, so keeping
    // the phase there is safe.
    for state in STATES {
        if state == State::Verifying {
            continue;
        }
        let reachable = PHASE_EVENTS.iter().any(|event| {
            matches!(event, rusta_core::PhaseEvent::PlanDrafted)
                || matches!(event, rusta_core::PhaseEvent::PlanApproved)
                || matches!(event, rusta_core::PhaseEvent::EditsApplied)
        });
        assert!(reachable, "{state} must have a model-drivable exit");
    }

    // And end to end: a dead backend in Verifying must not strand the run.
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("seed");
    let config = Config::parse(
        "[backend]\nbase_url = \"http://127.0.0.1:1/v1\"\n\
         [agent]\nauto_approve = true\n[validate]\ncommands = []\n",
    )
    .expect("config");
    let capture = Capture::default();
    let mut app = App::new(
        config,
        &Overrides::default(),
        dir.path().to_path_buf(),
        dir.path().join("s.jsonl"),
        Reporter::new(Box::new(capture.clone())),
        Mode::Oneshot,
    )
    .expect("app");

    app.handle_line("do something").await;
    assert!(
        capture.text().contains("turn abandoned"),
        "the dead backend must report, not panic: {}",
        capture.text()
    );
    assert_ne!(
        app.machine.state(),
        State::Verifying,
        "an abandoned turn must never leave the machine in Verifying"
    );
}

/// A29 (round 8): a disk failure part-way through `/undo` left the tree
/// half-restored, destroyed the journal entries for the rest of the batch,
/// popped the batch off the stack, and reverted the commit anyway — so the
/// working tree and git history disagreed and the next `/undo` answered
/// "nothing to undo". Three separate mistakes composing into an
/// unrecoverable state.
///
/// The failure is injected by making the target file read-only, which is the
/// reason this path shipped untested: it cannot be reached by scripting the
/// REPL alone. Note the test fails *loudly* rather than skipping if the
/// injection does not take (running as root, where mode bits are advisory) —
/// a test that quietly no-ops is the §16.1 shape this project keeps finding.
#[tokio::test]
async fn a_failed_undo_keeps_the_batch_the_journal_and_the_commit() {
    use rusta_core::{Event, Session};

    let repo = tempfile::TempDir::new().expect("repo");
    let a = repo.path().join("a.txt");
    let b = repo.path().join("b.txt");
    std::fs::write(&a, "A_NEW\n").expect("seed");
    std::fs::write(&b, "B_NEW\n").expect("seed");
    let log = repo.path().join("s.jsonl");
    {
        let mut session = Session::open(&log).expect("open");
        session.record(Event::BatchBoundary).expect("boundary");
        session
            .record_edit("a.txt", true, "A_OLD\n", "A_NEW\n")
            .expect("edit");
        session
            .record_edit("b.txt", true, "B_OLD\n", "B_NEW\n")
            .expect("edit");
        // A commit for the batch: the point is that it must survive.
        session
            .record(Event::Commit {
                sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_owned(),
                message: "rusta: two files".to_owned(),
            })
            .expect("commit");
    }

    let capture = Capture::default();
    let mut app = App::new(
        Config::default(),
        &Overrides::default(),
        repo.path().to_path_buf(),
        log,
        Reporter::new(Box::new(capture.clone())),
        Mode::Repl,
    )
    .expect("app");
    assert_eq!(
        app.batches.iter().map(|b| b.entries).collect::<Vec<_>>(),
        vec![2],
        "one batch holding both edits"
    );

    // Undo is LIFO, so `b.txt` restores first and `a.txt` is the one that
    // must fail. 0o444 makes the write fail with EACCES for a normal user.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o444)).expect("chmod");

    app.handle_line("/undo").await;
    let shown = capture.text();

    assert_eq!(
        std::fs::read_to_string(&b).expect("read b"),
        "B_OLD\n",
        "the entry that could be restored was restored"
    );
    assert_eq!(
        std::fs::read_to_string(&a).expect("read a"),
        "A_NEW\n",
        "the read-only file must NOT have been restored — if this fails the \
         injection did not take (are you running as root?), and the rest of \
         this test proves nothing"
    );
    assert!(
        shown.contains("undo failed on disk after 1 of 2"),
        "the partial failure must be reported honestly: {shown}"
    );
    assert!(
        !shown.contains("reverted commit"),
        "the commit must be kept when the restore did not finish: {shown}"
    );
    assert_eq!(
        app.batches.iter().map(|b| b.entries).collect::<Vec<_>>(),
        vec![1],
        "the batch stays on the stack, reduced to what is still unrestored"
    );
    assert_eq!(
        app.tools.editor().undo_stack().len(),
        1,
        "the unrestored journal entry must survive the failed write"
    );

    // …and the whole point: once the disk problem is fixed, a retry finishes.
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).expect("chmod back");
    app.handle_line("/undo").await;
    assert_eq!(
        std::fs::read_to_string(&a).expect("read a"),
        "A_OLD\n",
        "the retry completes the undo the disk error interrupted"
    );
    assert!(
        app.batches.is_empty(),
        "the batch is consumed once it is fully restored"
    );
}
