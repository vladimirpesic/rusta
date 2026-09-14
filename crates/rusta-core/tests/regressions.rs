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
