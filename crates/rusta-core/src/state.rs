//! Phase-gated state machine — development plan §6.4 (R5).
//!
//! Four states, six scaffold events, ten canonical tools. The transition
//! table is a closed, exhaustive `match`: invalid transitions are rejected
//! loudly (never silently ignored), and mutation tools are simply not
//! registered in read-only states — `fs::write` is unreachable there by
//! construction, not by prompt instruction (SmallCTL `PhaseContract`).
//!
//! A tool request that is unavailable in the current state yields a 1–2
//! line corrective note naming the current state and the transition that
//! unlocks the tool ([`corrective_note`]) — cheap context; small models
//! learn the phase within one turn.
//!
//! Note on [`PhaseEvent::PlanDrafted`]: §6.4's exit-gate table is normative
//! — `Exploring` exits on "plan drafted" and `Planning` exits on "plan
//! approval", which requires two distinct events; the prose event list is
//! illustrative. A read-only Q&A turn (no plan drafted) simply ends with
//! the state unchanged.

use std::collections::VecDeque;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// The four phases — plan §6.4 state table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Research phase: read-only tools; exit gate is a drafted plan.
    Exploring,
    /// A plan exists and awaits approval (user y/n; `/auto` approves).
    Planning,
    /// Plan approved: all ten tools; mutation tools live here only.
    Editing,
    /// Edit batch applied: validators run; green finishes, red returns to
    /// `Editing`.
    Verifying,
}

/// All states, in lifecycle order.
pub static STATES: [State; 4] = [
    State::Exploring,
    State::Planning,
    State::Editing,
    State::Verifying,
];

impl State {
    /// Display name (as used in prompts and notes).
    pub fn name(self) -> &'static str {
        match self {
            State::Exploring => "Exploring",
            State::Planning => "Planning",
            State::Editing => "Editing",
            State::Verifying => "Verifying",
        }
    }

    /// What unblocks the next phase — the exit gate (plan §6.4 table).
    pub fn exit_gate(self) -> &'static str {
        match self {
            State::Exploring => "plan drafted (a change-task needs a short numbered plan)",
            State::Planning => "plan approval (user y/n; /auto approves)",
            State::Editing => "edit batch applied (then validation runs)",
            State::Verifying => "all validators green (failure returns to Editing)",
        }
    }

    /// Tools registered in this phase — the phase-gated registry.
    pub fn tools(self) -> &'static [Tool] {
        match self {
            State::Exploring | State::Planning => &RESEARCH_TOOLS,
            State::Editing => &TOOLS,
            State::Verifying => &VERIFYING_TOOLS,
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The canonical tool registry — exactly ten tools, DECIDED (plan §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    /// `read(path, from?, to?)` — numbered file slice.
    Read,
    /// `grep(pattern, glob?)` — `file:line: text` matches.
    Grep,
    /// `glob(pattern)` — matching path list.
    Glob,
    /// `map_refresh()` — re-render the repo map.
    MapRefresh,
    /// `map_drill(path, name | from, to)` — definition span; ledger credit.
    MapDrill,
    /// `dispatch(task | tasks)` — read-only sub-coders.
    Dispatch,
    /// `ask(question)` — pause for a user reply.
    Ask,
    /// `edit(path, search, replace)` — tool-call form of an edit block.
    Edit,
    /// `write(path, content)` — full-file write; requires a prior read.
    Write,
    /// `shell(command)` — §6.12 safety policy, approval-gated.
    Shell,
}

/// All ten canonical tools, in registry order.
pub static TOOLS: [Tool; 10] = [
    Tool::Read,
    Tool::Grep,
    Tool::Glob,
    Tool::MapRefresh,
    Tool::MapDrill,
    Tool::Dispatch,
    Tool::Ask,
    Tool::Edit,
    Tool::Write,
    Tool::Shell,
];

/// `Exploring`/`Planning` share the read-only research toolset.
static RESEARCH_TOOLS: [Tool; 7] = [
    Tool::Read,
    Tool::Grep,
    Tool::Glob,
    Tool::MapRefresh,
    Tool::MapDrill,
    Tool::Dispatch,
    Tool::Ask,
];

/// `Verifying` may read and run commands, but neither dispatch nor mutate.
static VERIFYING_TOOLS: [Tool; 7] = [
    Tool::Read,
    Tool::Grep,
    Tool::Glob,
    Tool::MapRefresh,
    Tool::MapDrill,
    Tool::Shell,
    Tool::Ask,
];

impl Tool {
    /// Wire name (as the model spells it in tool calls).
    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Read => "read",
            Tool::Grep => "grep",
            Tool::Glob => "glob",
            Tool::MapRefresh => "map_refresh",
            Tool::MapDrill => "map_drill",
            Tool::Dispatch => "dispatch",
            Tool::Ask => "ask",
            Tool::Edit => "edit",
            Tool::Write => "write",
            Tool::Shell => "shell",
        }
    }

    /// Parses a tool name from a model request; unknown names yield `None`
    /// (the caller answers with the unknown-tool corrective note, §6.1).
    pub fn parse(name: &str) -> Option<Self> {
        TOOLS.iter().copied().find(|tool| tool.as_str() == name)
    }

    /// Whether the tool is registered in `state`.
    pub fn available_in(self, state: State) -> bool {
        state.tools().contains(&self)
    }

    /// One-line description with input keys. Shared by the registry and the
    /// core prompt (§6.6) so the two can never drift apart.
    pub fn one_liner(self) -> &'static str {
        match self {
            Tool::Read => "read(path, from?, to?): numbered file slice",
            Tool::Grep => "grep(pattern, glob?): file:line: matching lines",
            Tool::Glob => "glob(pattern): matching file paths",
            Tool::MapRefresh => "map_refresh(): re-render the repo map",
            Tool::MapDrill => "map_drill(path, name | from, to): definition span; counts as a read",
            Tool::Dispatch => {
                "dispatch(task | tasks): read-only sub-coder research, labeled reports"
            }
            Tool::Ask => "ask(question): ask the user; the reply returns as your next observation",
            Tool::Edit => "edit(path, search, replace): tool form of an edit block, same rules",
            Tool::Write => {
                "write(path, content): full-file write; the file must have been read first"
            }
            Tool::Shell => "shell(command): run a command (approval-gated)",
        }
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Scaffold events — the only things that can move the machine (plan §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseEvent {
    /// The model drafted a plan for a change-task (`Exploring → Planning`).
    PlanDrafted,
    /// The user approved the plan, or `/auto` did (`Planning → Editing`).
    PlanApproved,
    /// An edit batch was applied (`Editing → Verifying`).
    EditsApplied,
    /// All validators are green (`Verifying → Exploring`: task done).
    ValidationPassed,
    /// A validator failed (`Verifying → Editing`: fix and re-verify).
    ValidationFailed,
    /// Ctrl-C / user abort: abandon the task, return to `Exploring`.
    UserInterrupt,
}

/// All scaffold events, in declaration order. The order is the deterministic
/// tie-break for [`corrective_note`]'s path search.
pub static PHASE_EVENTS: [PhaseEvent; 6] = [
    PhaseEvent::PlanDrafted,
    PhaseEvent::PlanApproved,
    PhaseEvent::EditsApplied,
    PhaseEvent::ValidationPassed,
    PhaseEvent::ValidationFailed,
    PhaseEvent::UserInterrupt,
];

impl PhaseEvent {
    /// Canonical name (used in errors and notes).
    pub fn name(self) -> &'static str {
        match self {
            PhaseEvent::PlanDrafted => "PlanDrafted",
            PhaseEvent::PlanApproved => "PlanApproved",
            PhaseEvent::EditsApplied => "EditsApplied",
            PhaseEvent::ValidationPassed => "ValidationPassed",
            PhaseEvent::ValidationFailed => "ValidationFailed",
            PhaseEvent::UserInterrupt => "UserInterrupt",
        }
    }
}

impl fmt::Display for PhaseEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One executed transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    /// Phase before the event.
    pub from: State,
    /// Phase after the event.
    pub to: State,
    /// Short journal reason (the `StateChange` event's reason, §6.10).
    pub reason: &'static str,
}

/// The transition table — every `(state, event)` combination, exhaustive by
/// construction. `Ok(None)` means a legal no-op (the state is unchanged).
pub(crate) fn pure_transition(from: State, event: PhaseEvent) -> Result<Option<Transition>, Error> {
    use PhaseEvent as E;
    use State as S;

    let (to, reason) = match (from, event) {
        (S::Exploring, E::PlanDrafted) => (Some(S::Planning), "plan drafted"),
        (S::Planning, E::PlanApproved) => (Some(S::Editing), "plan approved"),
        (S::Editing, E::EditsApplied) => (Some(S::Verifying), "edit batch applied"),
        (S::Verifying, E::ValidationPassed) => (Some(S::Exploring), "validation green"),
        (S::Verifying, E::ValidationFailed) => (Some(S::Editing), "validation failed"),
        (S::Planning | S::Editing | S::Verifying, E::UserInterrupt) => {
            (Some(S::Exploring), "user interrupt")
        }
        // Interrupting while already at rest has nothing to abandon.
        (S::Exploring, E::UserInterrupt) => (None, "already exploring"),
        // Every remaining combination is illegal in its phase — impossible
        // by construction in the agent loop, and rejected loudly here
        // (never silently ignored).
        (
            S::Exploring,
            E::PlanApproved | E::EditsApplied | E::ValidationPassed | E::ValidationFailed,
        )
        | (
            S::Planning,
            E::PlanDrafted | E::EditsApplied | E::ValidationPassed | E::ValidationFailed,
        )
        | (
            S::Editing,
            E::PlanDrafted | E::PlanApproved | E::ValidationPassed | E::ValidationFailed,
        )
        | (S::Verifying, E::PlanDrafted | E::PlanApproved | E::EditsApplied) => {
            return Err(Error::InvalidTransition {
                from: from.to_string(),
                event: event.name().to_owned(),
            });
        }
    };
    Ok(to.map(|to| Transition { from, to, reason }))
}

/// A phase machine instance. Transitions fire only on scaffold events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    state: State,
}

impl Default for Machine {
    fn default() -> Self {
        Self {
            state: State::Exploring,
        }
    }
}

impl Machine {
    /// A fresh machine at rest (`Exploring`).
    pub fn new() -> Self {
        Self::default()
    }

    /// A machine seeded at `state` — the `/resume` path: session replay
    /// reconstructs the final phase and the caller continues from there.
    pub fn resume_at(state: State) -> Self {
        Self { state }
    }

    /// Current phase.
    pub fn state(&self) -> State {
        self.state
    }

    /// Tools registered in the current phase.
    pub fn tools(&self) -> &'static [Tool] {
        self.state.tools()
    }

    /// Fires a scaffold event. Returns the [`Transition`] when the state
    /// changed, `Ok(None)` for a legal no-op, and an error for an illegal
    /// `(state, event)` combination (the machine is left untouched).
    pub fn fire(&mut self, event: PhaseEvent) -> Result<Option<Transition>, Error> {
        let transition = pure_transition(self.state, event)?;
        if let Some(transition) = &transition {
            self.state = transition.to;
        }
        Ok(transition)
    }
}

/// The 1–2 line corrective note for a tool unavailable in `state` (plan
/// §6.4): names the current state and the transition(s) that unlock the
/// tool, derived from the transition table itself so it can never go
/// stale. Deterministic: shortest path, events enumerated in declaration
/// order.
pub fn corrective_note(state: State, tool: Tool) -> String {
    if tool.available_in(state) {
        return format!("{tool} is available in {state}.");
    }
    let Some(path) = unlocking_path(state, tool) else {
        return format!(
            "{tool} is not available in {state}; no transition reaches a phase that registers it."
        );
    };
    let phrases: Vec<&str> = path.iter().map(|&event| event_phrase(event)).collect();
    let first = phrases[0];
    let mut note = format!(
        "{tool} is not available in {state}. {}{}",
        first[0..1].to_uppercase(),
        &first[1..]
    );
    for phrase in &phrases[1..] {
        note.push_str("; then ");
        note.push_str(phrase);
    }
    note.push('.');
    note
}

/// The imperative phrase for one edge of the unlocking path. Every event
/// has exactly one destination phase, so the phrase is keyed by event.
fn event_phrase(event: PhaseEvent) -> &'static str {
    match event {
        PhaseEvent::PlanDrafted => "draft a plan to enter Planning",
        PhaseEvent::PlanApproved => "get plan approval (y/n, or /auto) to enter Editing",
        PhaseEvent::EditsApplied => "apply the edit batch to enter Verifying",
        PhaseEvent::ValidationPassed => "pass validation to finish and return to Exploring",
        PhaseEvent::ValidationFailed => {
            "validation failure returns you to Editing to fix the errors"
        }
        PhaseEvent::UserInterrupt => "interrupt the task to return to Exploring",
    }
}

/// Breadth-first shortest path from `state` to any phase registering
/// `tool`, over the legal transition graph. Neighbor enumeration follows
/// [`PHASE_EVENTS`] order, so ties break deterministically.
fn unlocking_path(state: State, tool: Tool) -> Option<Vec<PhaseEvent>> {
    let mut parents = [None; 4]; // (predecessor index, event) per state
    let mut visited = [false; 4];
    let start = state as usize;
    visited[start] = true;
    let mut queue = VecDeque::from([state]);
    let mut goal = None;
    while let Some(current) = queue.pop_front() {
        if tool.available_in(current) {
            goal = Some(current);
            break;
        }
        for event in PHASE_EVENTS {
            let Ok(Some(transition)) = pure_transition(current, event) else {
                continue;
            };
            let next = transition.to as usize;
            if !visited[next] {
                visited[next] = true;
                parents[next] = Some((current as usize, event));
                queue.push_back(transition.to);
            }
        }
    }
    let goal = goal?;
    // Walk parents back to the start; the start is never the goal because
    // the tool was unavailable there.
    let mut path = Vec::new();
    let mut cursor = goal as usize;
    while cursor != start {
        let (previous, event) = parents[cursor]?;
        path.push(event);
        cursor = previous;
    }
    path.reverse();
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_lifecycle_walks_the_planned_phases() {
        let mut machine = Machine::new();
        assert_eq!(machine.state(), State::Exploring);

        let draft = machine.fire(PhaseEvent::PlanDrafted).expect("draft");
        assert_eq!(draft.map(|t| t.to), Some(State::Planning));
        assert_eq!(draft.map(|t| t.reason), Some("plan drafted"));

        machine.fire(PhaseEvent::PlanApproved).expect("approve");
        assert_eq!(machine.state(), State::Editing);
        machine.fire(PhaseEvent::EditsApplied).expect("apply");
        assert_eq!(machine.state(), State::Verifying);

        let failed = machine.fire(PhaseEvent::ValidationFailed).expect("fail");
        assert_eq!(failed.map(|t| t.to), Some(State::Editing));
        machine.fire(PhaseEvent::EditsApplied).expect("apply");
        let done = machine.fire(PhaseEvent::ValidationPassed).expect("pass");
        assert_eq!(done.map(|t| t.to), Some(State::Exploring));
    }

    #[test]
    fn illegal_fire_is_rejected_and_leaves_state_untouched() {
        let mut machine = Machine::new();
        let error = machine.fire(PhaseEvent::EditsApplied).expect_err("illegal");
        assert!(error.to_string().contains("EditsApplied"), "{error}");
        assert!(error.to_string().contains("Exploring"), "{error}");
        assert_eq!(machine.state(), State::Exploring);
    }

    #[test]
    fn interrupt_at_rest_is_a_legal_no_op() {
        let mut machine = Machine::new();
        assert_eq!(
            machine.fire(PhaseEvent::UserInterrupt).expect("legal"),
            None
        );
        assert_eq!(machine.state(), State::Exploring);
    }

    #[test]
    fn resume_at_seeds_the_phase() {
        let machine = Machine::resume_at(State::Verifying);
        assert_eq!(machine.state(), State::Verifying);
        assert!(machine.tools().contains(&Tool::Shell));
    }

    #[test]
    fn corrective_note_names_state_and_shortest_unlock_path() {
        let note = corrective_note(State::Planning, Tool::Edit);
        assert_eq!(
            note,
            "edit is not available in Planning. Get plan approval (y/n, or /auto) to enter Editing."
        );
        let note = corrective_note(State::Exploring, Tool::Shell);
        assert!(note.contains("Draft a plan to enter Planning"), "{note}");
        assert!(note.contains("get plan approval"), "{note}");
        // Deterministic across calls.
        assert_eq!(note, corrective_note(State::Exploring, Tool::Shell));
    }

    /// Plan §8 M3 acceptance: the full transition table, all 24
    /// `(State × PhaseEvent)` cells pinned — every legal edge, the one
    /// legal no-op, and all sixteen illegal combinations. If a new event
    /// or state is added, this test forces a conscious table update.
    #[test]
    fn transition_table_is_exhaustive_and_pinned() {
        #[derive(Debug)]
        enum Outcome {
            NoOp,
            Moves(State),
            Illegal,
        }
        let table: [(State, PhaseEvent, Outcome); 24] = [
            // Exploring: only drafting a plan moves; interrupting at rest is a no-op.
            (
                State::Exploring,
                PhaseEvent::PlanDrafted,
                Outcome::Moves(State::Planning),
            ),
            (State::Exploring, PhaseEvent::PlanApproved, Outcome::Illegal),
            (State::Exploring, PhaseEvent::EditsApplied, Outcome::Illegal),
            (
                State::Exploring,
                PhaseEvent::ValidationPassed,
                Outcome::Illegal,
            ),
            (
                State::Exploring,
                PhaseEvent::ValidationFailed,
                Outcome::Illegal,
            ),
            (State::Exploring, PhaseEvent::UserInterrupt, Outcome::NoOp),
            // Planning: approval starts editing; a second draft is illegal.
            (State::Planning, PhaseEvent::PlanDrafted, Outcome::Illegal),
            (
                State::Planning,
                PhaseEvent::PlanApproved,
                Outcome::Moves(State::Editing),
            ),
            (State::Planning, PhaseEvent::EditsApplied, Outcome::Illegal),
            (
                State::Planning,
                PhaseEvent::ValidationPassed,
                Outcome::Illegal,
            ),
            (
                State::Planning,
                PhaseEvent::ValidationFailed,
                Outcome::Illegal,
            ),
            (
                State::Planning,
                PhaseEvent::UserInterrupt,
                Outcome::Moves(State::Exploring),
            ),
            // Editing: edits hand over to verification; nothing else is legal.
            (State::Editing, PhaseEvent::PlanDrafted, Outcome::Illegal),
            (State::Editing, PhaseEvent::PlanApproved, Outcome::Illegal),
            (
                State::Editing,
                PhaseEvent::EditsApplied,
                Outcome::Moves(State::Verifying),
            ),
            (
                State::Editing,
                PhaseEvent::ValidationPassed,
                Outcome::Illegal,
            ),
            (
                State::Editing,
                PhaseEvent::ValidationFailed,
                Outcome::Illegal,
            ),
            (
                State::Editing,
                PhaseEvent::UserInterrupt,
                Outcome::Moves(State::Exploring),
            ),
            // Verifying: green finishes, red returns to Editing.
            (State::Verifying, PhaseEvent::PlanDrafted, Outcome::Illegal),
            (State::Verifying, PhaseEvent::PlanApproved, Outcome::Illegal),
            (State::Verifying, PhaseEvent::EditsApplied, Outcome::Illegal),
            (
                State::Verifying,
                PhaseEvent::ValidationPassed,
                Outcome::Moves(State::Exploring),
            ),
            (
                State::Verifying,
                PhaseEvent::ValidationFailed,
                Outcome::Moves(State::Editing),
            ),
            (
                State::Verifying,
                PhaseEvent::UserInterrupt,
                Outcome::Moves(State::Exploring),
            ),
        ];
        for (from, event, expected) in &table {
            let result = pure_transition(*from, *event);
            match expected {
                Outcome::Moves(to) => {
                    let transition = result.expect("legal transition").expect("state change");
                    assert_eq!(transition.from, *from);
                    assert_eq!(transition.to, *to);
                    assert!(!transition.reason.is_empty());
                }
                Outcome::NoOp => {
                    assert_eq!(result.expect("legal no-op"), None, "({from}, {event})");
                }
                Outcome::Illegal => {
                    assert!(result.is_err(), "({from}, {event})");
                }
            }
        }
        // And the flip side: every combination has a pinned cell — 4 × 6.
        assert_eq!(table.len(), STATES.len() * PHASE_EVENTS.len());
        for from in STATES {
            for event in PHASE_EVENTS {
                assert!(
                    table.iter().any(|&(f, e, _)| f == from && e == event),
                    "({from}, {event}) missing from the pinned table"
                );
            }
        }
    }
}
