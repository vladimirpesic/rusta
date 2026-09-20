//! Regressions for defects found in the 2026-09-13 audit. Each test pins a
//! behaviour that was wrong at commit 8ec71c6.

use rusta_core::{
    CardDeck, Event, LoopGuard, Machine, PhaseEvent, Session, SkillCard, State, Status, error_cues,
};

/// C3: a FAMA escalation used to re-seed the machine without journaling the
/// regression, so the *next* `StateChange` failed replay validation — the
/// session became unresumable, and because `App::new` auto-resumes the day's
/// log, `rusta` refused to start in that repo at all.
#[test]
fn escalation_regression_is_journaled_and_replays() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");

    let mut machine = Machine::new();
    let mut journal = Vec::new();
    let fire = |machine: &mut Machine, event: PhaseEvent, journal: &mut Vec<Event>| {
        let transition = machine
            .fire(event)
            .unwrap_or_else(|e| panic!("{event} must be legal: {e}"))
            .expect("state change");
        journal.push(Event::StateChange {
            from: transition.from,
            to: transition.to,
            reason: transition.reason.to_owned(),
        });
    };

    fire(&mut machine, PhaseEvent::PlanDrafted, &mut journal);
    fire(&mut machine, PhaseEvent::PlanApproved, &mut journal);
    assert_eq!(machine.state(), State::Editing);
    // The escalation is now a real scaffold event, so it journals like any
    // other transition instead of bypassing the table.
    fire(&mut machine, PhaseEvent::LoopEscalated, &mut journal);
    assert_eq!(machine.state(), State::Planning);
    fire(&mut machine, PhaseEvent::PlanApproved, &mut journal);

    {
        let mut session = Session::open(&path).expect("open");
        for event in journal {
            session.record(event).expect("record");
        }
    }
    let session = Session::open(&path).expect("reopen");
    let replayed = session
        .replay_context()
        .expect("session must stay resumable");
    assert_eq!(replayed.state, State::Editing);
}

/// C3 corollary: the escalation edge exists only where §6.6 defines it.
#[test]
fn escalation_is_legal_only_out_of_editing() {
    for state in [State::Exploring, State::Planning, State::Verifying] {
        let mut machine = Machine::resume_at(state);
        assert!(
            machine.fire(PhaseEvent::LoopEscalated).is_err(),
            "LoopEscalated must be illegal in {state}"
        );
        assert_eq!(machine.state(), state, "a rejected event must not move it");
    }
}

/// C4a: the shipped recovery cards declared trigger kinds that no code path
/// emitted, so they could never fire. `error_cues` is now the one place
/// observation text becomes a cue vocabulary.
#[test]
fn shipped_recovery_cards_fire_on_cues_the_agent_emits() {
    let deck = CardDeck::shipped();

    let failed_edit = error_cues(
        "edit",
        "# 1 SEARCH/REPLACE block failed to exactly match lines in src/lib.rs",
    );
    let cues: Vec<&str> = failed_edit.iter().map(String::as_str).collect();
    let fired: Vec<&str> = deck.select(&cues).iter().map(|c| c.name()).collect();
    assert!(fired.contains(&"edit-recovery"), "{fired:?} from {cues:?}");

    let red_validator = error_cues("validation", "test result: FAILED. 1 failed; 3 passed");
    let cues: Vec<&str> = red_validator.iter().map(String::as_str).collect();
    let fired: Vec<&str> = deck.select(&cues).iter().map(|c| c.name()).collect();
    assert!(fired.contains(&"verify-focus"), "{fired:?} from {cues:?}");
}

/// C4c: the starter deck must travel with the binary, not be read out of
/// whatever repository the user happens to be standing in.
#[test]
fn shipped_deck_loads_without_any_repo() {
    let deck = CardDeck::shipped();
    assert_eq!(deck.cards().len(), 4, "the shipped starter deck");
}

/// C4c: an unrelated `skills/` directory in the user's repo used to abort
/// startup. Only `.rusta/skills/` is Rusta's, and the shipped deck stands
/// on its own.
#[test]
fn foreign_skills_directory_does_not_break_the_deck() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("skills")).expect("dirs");
    std::fs::write(dir.path().join("skills/onboarding.md"), "# Onboarding\n").expect("write");

    let deck = CardDeck::load_for_repo(dir.path()).expect("foreign skills/ must be ignored");
    assert_eq!(deck.cards().len(), 4);
}

/// C4c: project cards extend the shipped deck, and a same-named card wins.
#[test]
fn project_cards_extend_and_override_the_shipped_deck() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cards = dir.path().join(".rusta").join("skills");
    std::fs::create_dir_all(&cards).expect("dirs");
    std::fs::write(
        cards.join("house-style.md"),
        "---\nname: house-style\ntype: knowledge\ntriggers: [style]\npriority: 5\ntoken_cost: 20\nuser-invocable: true\n---\n# Follow the house style.\n",
    )
    .expect("write");
    std::fs::write(
        cards.join("read-large-files.md"),
        "---\nname: read-large-files\ntype: tool\ntriggers: [read]\npriority: 9\ntoken_cost: 20\nuser-invocable: false\n---\n# Project override.\n",
    )
    .expect("write");

    let deck = CardDeck::load_for_repo(dir.path()).expect("load");
    assert_eq!(deck.cards().len(), 5, "4 shipped + 1 project");
    let overridden = deck
        .cards()
        .iter()
        .find(|c| c.name() == "read-large-files")
        .expect("present");
    assert_eq!(overridden.body(), "# Project override.");
}

/// M3: detector (c) kept one slot, so with §7's three default validators the
/// counter reset on every call and `mutation_required` could never trip.
#[test]
fn validator_detector_trips_with_several_validators_configured() {
    let mut guard = LoopGuard::new();
    let commands = [
        "cargo check --workspace",
        "cargo clippy --workspace -- -D warnings",
        "cargo test --workspace",
    ];
    let output = "error[E0308]: mismatched types";

    for command in commands {
        assert!(
            guard
                .observe_validation(command, output)
                .capsules
                .is_empty(),
            "first round must not trip"
        );
    }
    // The identical second round is the model re-verifying without changing
    // anything — exactly what the detector exists to catch.
    let mut tripped = false;
    for command in commands {
        tripped |= guard
            .observe_validation(command, output)
            .capsules
            .contains(&"mutation_required");
    }
    assert!(tripped, "identical second round must trip the capsule");
}

/// M3 corollary: a validator whose output *changed* is progress, not a loop.
#[test]
fn changed_validator_output_does_not_trip() {
    let mut guard = LoopGuard::new();
    guard.observe_validation("cargo test", "3 failed");
    let trip = guard.observe_validation("cargo test", "1 failed");
    assert!(trip.capsules.is_empty(), "{:?}", trip.capsules);
}

/// L9: CRLF cards failed to parse with a misleading message.
#[test]
fn crlf_skill_cards_parse() {
    let lf = "---\nname: x\ntype: tool\ntriggers: [read]\npriority: 3\ntoken_cost: 10\nuser-invocable: false\n---\n# Body line.\n";
    let crlf = lf.replace('\n', "\r\n");
    let card = SkillCard::parse(&crlf).expect("CRLF card must parse");
    assert_eq!(card.name(), "x");
    assert_eq!(card.body(), "# Body line.");
    assert_eq!(SkillCard::parse(lf).expect("LF").body(), card.body());
}

/// L10: replay credited the ledger from the tool *call*, so a failed read
/// granted a read-credit and weakened the read-before-edit rule.
#[test]
fn replay_credits_only_successful_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");
    {
        let mut session = Session::open(&path).expect("open");
        for (target, status) in [("missing.rs", Status::Error), ("real.rs", Status::Ok)] {
            session
                .record(Event::ToolCall {
                    name: "read".into(),
                    input: serde_json::json!({ "path": target }),
                })
                .expect("record");
            session
                .record(Event::ToolResult {
                    status,
                    summary: "…".into(),
                    truncated: false,
                })
                .expect("record");
        }
    }
    let session = Session::open(&path).expect("reopen");
    let replayed = session.replay_context().expect("replay");
    let credited: Vec<String> = replayed
        .ledger
        .read_set()
        .map(|p| p.display().to_string())
        .collect();
    assert_eq!(credited, vec!["real.rs".to_owned()]);
}

// ------------------------------------------- 2026-09-14 fourth-audit findings

/// G1: `/undo` on a resumed session must not write or delete outside the
/// repo. Replay builds undo entries from `EditApplied` paths, and
/// `root.join(absolute)` *replaces* the base — so an absolute path in a log
/// (which logs written before the §6.12 fence legitimately carry) turned the
/// journal's restore step into an arbitrary write, and its create-undo step
/// into an arbitrary delete.
#[test]
fn resumed_undo_cannot_escape_the_workspace() {
    let repo = tempfile::tempdir().expect("repo");
    let outside = tempfile::tempdir().expect("outside");
    let victim = outside.path().join("victim.txt");
    let doomed = outside.path().join("doomed.txt");
    std::fs::write(&victim, "ORIGINAL\n").expect("seed");
    std::fs::write(&doomed, "STILL HERE\n").expect("seed");

    let log = repo.path().join("s.jsonl");
    {
        let mut session = Session::open(&log).expect("open");
        // An edit of an existing file outside the repo, and a "created" file
        // outside the repo — the write and the delete arms of undo.
        session
            .record_edit(
                victim.to_str().expect("utf8"),
                true,
                "RESTORED\n",
                "after\n",
            )
            .expect("record");
        session
            .record_edit(doomed.to_str().expect("utf8"), false, "", "after\n")
            .expect("record");
        // A legitimate in-repo edit after them: the sidecar must stay in
        // lockstep, so this one must still replay correctly.
        std::fs::write(repo.path().join("inside.txt"), "NEW\n").expect("seed");
        session
            .record_edit("inside.txt", true, "OLD\n", "NEW\n")
            .expect("record");
    }

    let replayed = Session::open(&log)
        .expect("open")
        .replay_context()
        .expect("replay");
    assert_eq!(
        replayed.skipped_edits.len(),
        2,
        "both escaping edits must be reported: {:?}",
        replayed.skipped_edits
    );
    // Only the in-repo edit survives into the journal, and it is intact —
    // proving the sidecar stayed in lockstep past the two dropped entries.
    assert_eq!(replayed.undo.pending().len(), 1);
    assert_eq!(replayed.undo.pending()[0].before, "OLD\n");

    let mut undo = replayed.undo;
    for _ in 0..3 {
        let _ = undo.undo_last(repo.path());
    }
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read"),
        "ORIGINAL\n",
        "a file outside the repo was overwritten by /undo"
    );
    assert!(
        doomed.exists(),
        "a file outside the repo was deleted by /undo"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("inside.txt")).expect("read"),
        "OLD\n",
        "the legitimate in-repo undo must still work"
    );
}

/// G2: compression could not fire with three turns or fewer, whatever their
/// size. Three `read` observations at the §6.1 cap measured 65,574 estimated
/// tokens against a 32,768-token window — twice the whole context — returned
/// unchanged, and nothing downstream measured the request.
///
/// §6.6 keeps the last three turns verbatim, so the fix is not to drop them:
/// oversized observations are clipped instead, as a sub-coder clips its own
/// (§6.8). Every turn stays present; the evidence in each is bounded.
#[test]
fn few_but_huge_turns_are_clipped_to_fit() {
    use rusta_core::Compressor;
    use rusta_llm::Message;
    use rusta_llm::tokens::estimate_tokens;

    let window = 32_768u64;
    let compressor = Compressor::new(window);
    let big = "x".repeat(64 * 1024); // the §6.1 read cap
    let mut history = Vec::new();
    for turn in 0..3 {
        history.push(Message::assistant(format!("reading file {turn}")));
        history.push(Message::user(format!("TOOL RESULT read (ok)\n{big}")));
    }
    let before: u64 = history.iter().map(|m| estimate_tokens(&m.content)).sum();
    assert!(
        before > window,
        "fixture must start over the window: {before}"
    );

    let out = compressor.compress(history, 400);
    let after: u64 = out
        .messages
        .iter()
        .map(|m| estimate_tokens(&m.content))
        .sum();

    assert!(
        after + 400 <= compressor.history_budget(),
        "clipped history is {after} tokens, over the {} budget",
        compressor.history_budget()
    );
    // All three turns survive — clipping, not dropping.
    assert_eq!(out.messages.len(), 6, "no message may be removed");
    assert!(
        out.summary.is_none(),
        "no episodic summary at this turn count"
    );
    assert!(
        out.messages
            .iter()
            .any(|m| m.content.contains("clipped to fit")),
        "a clipped observation must say so"
    );
    // The assistant turns are untouched: §6.6 keeps model output verbatim.
    for turn in 0..3 {
        assert!(
            out.messages
                .iter()
                .any(|m| m.content == format!("reading file {turn}")),
            "assistant turn {turn} was modified"
        );
    }
}

/// G2 control: a history that already fits is returned byte-for-byte.
#[test]
fn a_history_within_budget_is_never_clipped() {
    use rusta_core::Compressor;
    use rusta_llm::Message;

    let compressor = Compressor::new(32_768);
    let history = vec![
        Message::user("please fix the parser"),
        Message::assistant("looking now"),
        Message::user("TOOL RESULT read (ok)\nfn main() {}"),
    ];
    let out = compressor.compress(history.clone(), 400);
    assert_eq!(out.messages, history);
    assert!(out.summary.is_none());
}

/// G2 follow-up, found by self-review of the G2 fix: the clip loop must make
/// strict progress. `CLIP_MARKER` costs tokens of its own, so an observation
/// only just above `MIN_KEPT_OBSERVATION` clipped to something *no smaller*,
/// and since the loop always picks the largest candidate it spun on that
/// input forever — a hang in the agent's hot path.
#[test]
fn the_clip_loop_always_terminates() {
    use rusta_core::{Compressor, MIN_KEPT_OBSERVATION};
    use rusta_llm::Message;
    use rusta_llm::tokens::estimate_tokens;

    // A window so small that no amount of clipping can satisfy it, with
    // every observation sitting just above the floor — the worst case.
    let compressor = Compressor::new(100);
    let just_above = "z".repeat((MIN_KEPT_OBSERVATION as usize + 8) * 3);
    let history: Vec<Message> = (0..3)
        .flat_map(|turn| {
            [
                Message::assistant(format!("turn {turn}")),
                Message::user(format!("TOOL RESULT read (ok)\n{just_above}")),
            ]
        })
        .collect();
    for message in &history {
        if message.role == rusta_llm::Role::User {
            assert!(
                estimate_tokens(&message.content) > MIN_KEPT_OBSERVATION,
                "fixture must sit above the floor to exercise the no-progress path"
            );
        }
    }

    // The assertion is that this returns at all.
    let out = compressor.compress(history, 0);
    assert_eq!(out.messages.len(), 6, "clipping never drops messages");
    assert!(out.summary.is_none());
}

// ------------------------------------- 2026-09-15 consolidated-audit findings

/// A2: a torn tail must be *repaired* on open, not merely tolerated on read.
///
/// `record` appends through `O_APPEND`, so without repair the next write
/// fuses its JSON onto the unterminated remnant. One append is survivable;
/// the second puts that fused line mid-file and the session refuses to open
/// for the rest of the repo-day — the exact failure the torn-write erratum
/// eliminated, re-created by the read-side-only fix for it.
#[test]
fn a_torn_tail_is_repaired_so_later_appends_stay_parseable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user_message\",\"content\":\"hi\"}\n{\"type\":\"assistant_message\",\"cont",
    )
    .expect("write");

    {
        let mut session = Session::open(&path).expect("torn tail must open");
        assert_eq!(session.events().len(), 1, "the partial event is dropped");
        session
            .record(Event::UserMessage {
                content: "second request".to_owned(),
            })
            .expect("append 1");
        session.record(Event::SessionEnd).expect("append 2");
    }

    // The remnant is gone, so both appends are their own lines.
    let raw = std::fs::read_to_string(&path).expect("read");
    assert!(
        !raw.contains("\"cont{"),
        "the torn remnant was fused to a later event: {raw}"
    );

    let session = Session::open(&path).expect("reopen must not be corrupt");
    assert_eq!(
        session.events().len(),
        3,
        "the surviving event plus both appends: {:?}",
        session.events()
    );
}

/// A2 control: corruption anywhere *other* than the tail is still fatal —
/// the events after it would replay against the wrong state, so the log must
/// not be silently truncated at the first bad line.
#[test]
fn mid_file_corruption_is_still_rejected_loudly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user_message\",\"content\":\"a\"}\nnot json at all\n{\"type\":\"session_end\"}\n",
    )
    .expect("write");

    let before = std::fs::read_to_string(&path).expect("read");
    let error = Session::open(&path).expect_err("mid-file corruption is fatal");
    assert!(error.to_string().contains("line 2"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        before,
        "a mid-file corrupt log must not be truncated"
    );
}

/// A2: the diffs sidecar gets the same repair. Its records carry whole file
/// contents, so a torn write there is likelier than in the log.
#[test]
fn a_torn_sidecar_tail_is_repaired_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");
    {
        let mut session = Session::open(&path).expect("open");
        session
            .record_edit("a.rs", true, "before\n", "after\n")
            .expect("edit");
    }
    let sidecar = path.with_extension("diffs.jsonl");
    let mut text = std::fs::read_to_string(&sidecar).expect("read");
    text.push_str("{\"hash\":123,\"cont");
    std::fs::write(&sidecar, text).expect("write");

    {
        let mut session = Session::open(&path).expect("open");
        session
            .record_edit("b.rs", true, "x\n", "y\n")
            .expect("append after the torn sidecar tail");
    }
    let replayed = Session::open(&path)
        .expect("reopen")
        .replay_context()
        .expect("sidecar must still be in lockstep");
    assert_eq!(replayed.undo.pending().len(), 2);
    assert_eq!(replayed.undo.pending()[1].before, "x\n");
}

/// A6: §6.8 says "the full sub-transcript goes to the session log only".
/// `Report.transcript` was built and documented as session-log material,
/// but the §6.10 schema had nowhere to put it and no code path wrote it —
/// so it was dropped. The event now carries it, and old logs without the
/// field still replay.
#[test]
fn dispatch_events_carry_the_sub_transcript() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");
    {
        let mut session = Session::open(&path).expect("open");
        session
            .record(Event::Dispatch {
                label: "auth".to_owned(),
                report: "login lives in src/auth.rs:12".to_owned(),
                transcript: vec![
                    rusta_core::TranscriptLine {
                        role: "user".to_owned(),
                        content: "where is login handled?".to_owned(),
                    },
                    rusta_core::TranscriptLine {
                        role: "assistant".to_owned(),
                        content: "login lives in src/auth.rs:12".to_owned(),
                    },
                ],
            })
            .expect("record");
    }
    let session = Session::open(&path).expect("reopen");
    let Some(Event::Dispatch { transcript, .. }) = session.events().first() else {
        panic!("expected a Dispatch event, got {:?}", session.events());
    };
    assert_eq!(
        transcript.len(),
        2,
        "the transcript must survive the round trip"
    );
    assert_eq!(transcript[1].content, "login lives in src/auth.rs:12");
}

/// A6 corollary: a log written before the field existed still opens.
#[test]
fn a_dispatch_event_without_a_transcript_still_replays() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"dispatch\",\"label\":\"a\",\"report\":\"findings\"}\n",
    )
    .expect("write");
    let session = Session::open(&path).expect("an older log must still open");
    assert_eq!(session.events().len(), 1);
}

/// A7: the report already replays inside the `dispatch` ToolResult
/// observation. Replaying the `Dispatch` event as a second user message
/// duplicated every report in a resumed context, breaking the byte-for-byte
/// replay fidelity §6.10 exists to provide.
#[test]
fn a_resumed_dispatch_report_appears_exactly_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");
    {
        let mut session = Session::open(&path).expect("open");
        session
            .record(Event::ToolCall {
                name: "dispatch".to_owned(),
                input: serde_json::json!({}),
            })
            .expect("call");
        session
            .record(Event::ToolResult {
                status: Status::Ok,
                summary: "SUB-CODER \"a\" REPORT:\nfindings".to_owned(),
                truncated: false,
            })
            .expect("result");
        session
            .record(Event::Dispatch {
                label: "a".to_owned(),
                report: "findings".to_owned(),
                transcript: Vec::new(),
            })
            .expect("dispatch");
    }
    let replayed = Session::open(&path)
        .expect("reopen")
        .replay_context()
        .expect("replay");
    let mentions = replayed
        .messages
        .iter()
        .filter(|message| message.content.contains("SUB-CODER \"a\" REPORT:"))
        .count();
    assert_eq!(mentions, 1, "the report must replay once, not twice");
}

/// Round 10: the cue vocabulary was the ceiling on what any skill deck could
/// do. Six cues shipped (`error`, `edit_failed`, `not_found`,
/// `duplicate_match`, `validation_failed`, `test_failure`), so a card could
/// only ever fire on those six situations however many cards existed —
/// little-coder ships 31 cards against Rusta's 4, and the gap was not only
/// the deck.
///
/// Each cue added here names a failure observed from a real model (§16.9),
/// not a hypothetical one.
#[test]
fn cues_cover_the_failures_real_models_actually_produce() {
    use rusta_core::error_cues;

    let cues = |tool: &str, content: &str| error_cues(tool, content);
    let has = |tool: &str, content: &str, cue: &str| cues(tool, content).iter().any(|c| c == cue);

    // Qwen2.5-Coder passed `{"from": "fn sum_even"}` — a string where a line
    // number belongs — on the first real tool call this project ever saw.
    assert!(
        has(
            "read",
            "\"from\" must be a positive integer",
            "bad_tool_args"
        ),
        "{:?}",
        cues("read", "\"from\" must be a positive integer")
    );
    assert!(has(
        "grep",
        "\"pattern\" must be a non-empty string",
        "bad_tool_args"
    ));

    // Qwen3-Coder drilled `map_drill(name: "eval_binary_op")` twice for an
    // identifier that does not exist anywhere in the file.
    assert!(has(
        "map_drill",
        "no definition named \"eval_binary_op\" in src/eval.rs",
        "wrong_path"
    ));
    assert!(has("read", "no such file — check the path", "wrong_path"));

    // A SEARCH block whose text the model invented: the failure that most
    // wants a whole-file rewrite rather than another SEARCH attempt.
    assert!(has(
        "edit",
        "# 1 SEARCH/REPLACE block failed to match!",
        "patch_target_not_found"
    ));

    // A window past EOF, and a drill that resolved nothing.
    assert!(has(
        "read",
        "lines 10-12 are past the end of the file, which has 3 line(s)",
        "past_eof"
    ));

    // The six original cues still fire — a wider vocabulary must not
    // displace the deck that already depends on it.
    assert!(has("edit", "failed to exactly match", "edit_failed"));
    assert!(has("edit", "failed to exactly match", "not_found"));
    assert!(has("edit", "did you mean", "duplicate_match"));
    assert!(has(
        "validation",
        "test result: FAILED",
        "validation_failed"
    ));
    assert!(has("validation", "test result: FAILED", "test_failure"));
    assert!(has("read", "anything at all", "error"));
}

/// Round 10: §6.6's detectors all work *across* turns — they compare a call
/// to the calls before it. Nothing watched a single completion, so a model
/// degenerating mid-stream ran to `max_tokens` every time. On CPU that is
/// the most expensive failure available: at ~8 tok/s a 2048-token
/// degenerate tail is four minutes of wall clock per turn, and the 7B runs
/// in §16.9 spent most of their budget exactly that way.
///
/// smallcode's `early_stop.js` is the reference: inspect only the tail of
/// the buffer so the check stays O(window) per token rather than O(n²).
#[test]
fn a_completion_that_degenerates_mid_stream_is_stopped() {
    use rusta_core::StreamGuard;

    // Ordinary prose and code never trip it, however long.
    let mut guard = StreamGuard::default();
    for i in 0..40 {
        let prose = format!(
            "Step {i}: the operands are popped in the wrong order here, so \
             the first pop is the right-hand side and the subtraction runs \
             backwards.\n"
        );
        assert!(
            guard.observe(&prose).is_none(),
            "varying prose must stream freely"
        );
    }

    // A repeated line — the shape a looping model actually emits.
    let mut guard = StreamGuard::default();
    let mut tripped = None;
    for turn in 0..12 {
        if let Some(reason) = guard.observe("    let lhs = stack.pop().unwrap();\n") {
            tripped = Some((turn, reason));
            break;
        }
    }
    let (turn, reason) = tripped.expect("a repeating line must trip the guard");
    assert!(
        turn >= 2,
        "never on the first repeat — that is legitimate code"
    );
    assert!(
        reason.contains("repeat"),
        "the reason reaches the user: {reason}"
    );

    // A repeated *block*, not just a line.
    let mut guard = StreamGuard::default();
    let block = "```tool\n{\"name\": \"read\", \"input\": {\"path\": \"a.rs\"}}\n```\n\
                 Let me check that file again.\n";
    let mut stopped = false;
    for _ in 0..10 {
        if guard.observe(block).is_some() {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "a repeating block must trip the guard too");

    // Real code with legitimately similar lines must survive.
    let mut guard = StreamGuard::default();
    for (i, line) in [
        "    let a = compute_first(input)?;\n",
        "    let b = compute_second(input)?;\n",
        "    let c = compute_third(input)?;\n",
        "    let d = compute_fourth(input)?;\n",
        "    let e = compute_fifth(input)?;\n",
        "    let f = compute_sixth(input)?;\n",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            guard.observe(line).is_none(),
            "similar-but-distinct line {i} must not trip it"
        );
    }
}

/// Round 10: Aider's default `edit_format` is **`"whole"`** — full-file
/// replacement — and it upgrades a model to `diff` only when that model is
/// known to handle SEARCH/REPLACE. Rusta has always been diff-only, with
/// `write` available but nothing ever routing to it.
///
/// That cost a measured run: the 7B's dominant failure in §16.9 was a SEARCH
/// block whose text it had invented, which can never match however many
/// times it retries. Aider's insight is that after repeated match failures
/// the format is the problem, not the attempt.
#[test]
fn repeated_search_failures_on_one_file_route_to_a_whole_file_rewrite() {
    use rusta_core::LoopGuard;

    let mut guard = LoopGuard::new();

    // One failure is ordinary — SEARCH/REPLACE is still the right tool.
    let first = guard.observe_edit_failure("src/eval.rs");
    assert!(
        first.capsules.is_empty(),
        "one miss must not change strategy: {first:?}"
    );

    // Two on the same file is the format failing, not the attempt.
    let second = guard.observe_edit_failure("src/eval.rs");
    assert!(
        second.capsules.contains(&"whole_file_rewrite"),
        "a second miss routes to `write`: {second:?}"
    );
    let capsule = rusta_core::capsule("whole_file_rewrite").expect("capsule is registered");
    assert!(
        capsule.text.contains("write"),
        "the capsule must name the tool that does not match text: {}",
        capsule.text
    );

    // Failures on *different* files are not the same problem.
    let mut guard = LoopGuard::new();
    assert!(guard.observe_edit_failure("a.rs").capsules.is_empty());
    assert!(
        guard.observe_edit_failure("b.rs").capsules.is_empty(),
        "each file gets its own budget"
    );

    // A success clears the count: the model recovered, so the next miss
    // starts over rather than inheriting a stale strike.
    let mut guard = LoopGuard::new();
    let _ = guard.observe_edit_failure("src/eval.rs");
    guard.observe_edit_success("src/eval.rs");
    assert!(
        guard
            .observe_edit_failure("src/eval.rs")
            .capsules
            .is_empty(),
        "a success resets the file's failure count"
    );
}
