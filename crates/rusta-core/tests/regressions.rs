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
