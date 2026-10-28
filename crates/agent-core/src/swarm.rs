//! Swarm mode, and the rule that `agent_swarm` runs alone.
//!
//! Swarm mode is a standing instruction: while it is on, the agent splits the
//! work it is given across `agent_swarm` rather than doing it itself. `/swarm`
//! turns it on until turned off; `/swarm <task>` turns it on for one task. A
//! reminder in the conversation says so when it starts, and another when it
//! ends.

use lm_bridge::message::{Message, ToolCall};

use crate::tools::builtin::agent_swarm::AGENT_SWARM_TOOL_NAME;

/// What turned swarm mode on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwarmTrigger {
    /// `/swarm` or `/swarm on`: until `/swarm off`.
    Manual,
    /// `/swarm <task>`: for that task's prompt only.
    Task,
}

/// The reminder that starts swarm mode.
pub const ENTER_REMINDER: &str = "## Swarm Mode

You are now in \"agent swarm\" mode. The user may send tasks that require a large number of parallel subagents.

## Workflow

You do not need to use todo_list to record this workflow.

1. First, you may need to do a small amount of exploratory work before deciding how to divide the task across subagents. You may not need subagents during this exploratory phase.

2. After exploring, if you are convinced no subagent is needed to complete the task, tell the user why and wait for further instructions; otherwise, continue with the appropriate delegation.

3. Once you have enough context, do not handle the main work yourself. Use agent_swarm with a `prompt_template` containing the `{{item}}` placeholder and an `items` array for the requested or appropriate number of subagents, partitioning the problem so each item gives one subagent a distinct part of the work. Pass `subagent_type` when the whole swarm should use a non-default subagent profile.

## Coordination

- Give each subagent a distinct scope of work.
- Avoid duplicating work across subagents.
- Avoid assigning conflicting changes or responsibilities to different subagents.
- Remember that subagents have your full capabilities. Do not overload their prompts with excessive detail; only describe the necessary background and each subagent's specific task.
- Unless the user explicitly specifies a lower limit, do not try to conserve the number of agents. agent_swarm supports up to 128 subagents and queues launches automatically, so decompose work as finely as possible while keeping subagent responsibilities non-conflicting; combine tasks only when they are genuinely inseparable. If the subagents only need to read, inspect, or report back without making changes, their scopes may overlap slightly.";

/// The reminder that ends swarm mode.
pub const EXIT_REMINDER: &str = "## Swarm Mode Ended

Swarm Mode has ended. You are no longer required to follow the Swarm Mode workflow.

The user's next request is likely to be a regular request. agent_swarm is unavailable until the user turns Swarm Mode on again, so do not call it. Handle the request yourself, and launch subagents with agent only if it explicitly asks for them.";

/// The first line of each reminder, by which the conversation shows which of
/// them it saw last.
const ENTER_MARKER: &str = "## Swarm Mode\n\nYou are now in \"agent swarm\" mode.";
const EXIT_MARKER: &str = "## Swarm Mode Ended";

/// `reminder` as the conversation carries it.
#[must_use]
pub fn reminder_message(reminder: &str) -> Message {
    Message::user(format!("<system-reminder>\n{reminder}\n</system-reminder>"))
}

/// Whether the last swarm reminder in `history` started swarm mode: `Some(true)`
/// for the entry reminder, `Some(false)` for the exit one, `None` for neither.
///
/// Compared by position, not just presence: a history can merge adjacent
/// messages, putting both reminders in one.
#[must_use]
pub fn last_reminder_was_entry(history: &[Message]) -> Option<bool> {
    history.iter().rev().find_map(|message| {
        let text = message.text_content();
        match (text.rfind(ENTER_MARKER), text.rfind(EXIT_MARKER)) {
            (Some(entry), Some(exit)) => Some(entry > exit),
            (Some(_), None) => Some(true),
            (None, Some(_)) => Some(false),
            (None, None) => None,
        }
    })
}

/// The reminder a prompt needs, given whether swarm mode is `active` and which
/// reminder the conversation last carried: the entry when it is on and not yet
/// said, the exit when it is off after having been said.
#[must_use]
pub fn reminder_due(active: bool, last_entry: Option<bool>) -> Option<&'static str> {
    match (active, last_entry) {
        (true, Some(true)) => None,
        (true, _) => Some(ENTER_REMINDER),
        (false, Some(true)) => Some(EXIT_REMINDER),
        (false, _) => None,
    }
}

/// The refusal for a step that pairs `agent_swarm` with other calls or makes
/// several; `None` when the step may run. A sub-agent's refusal adds that it
/// should change approach.
#[must_use]
pub fn step_denial(calls: &[ToolCall], subagent: bool) -> Option<String> {
    let swarms = calls
        .iter()
        .filter(|call| call.name == AGENT_SWARM_TOOL_NAME)
        .count();
    if swarms == 0 || (swarms == 1 && calls.len() == 1) {
        return None;
    }
    let message = if swarms > 1 {
        let suffix = if calls.len() > swarms {
            " agent_swarm also must not be combined with other tools in the same response."
        } else {
            ""
        };
        format!(
            "agent_swarm must be called one swarm at a time. Multiple agent_swarm calls are not \
             forbidden, but issue them sequentially: call one agent_swarm, wait for its result, \
             then call the next; or merge the work into a single agent_swarm when one swarm can \
             cover it.{suffix}"
        )
    } else {
        "agent_swarm must be the only tool call in a model response. Retry with a single \
         agent_swarm call by itself, then call any other tools after it returns."
            .to_string()
    };
    Some(if subagent {
        format!(
            "{message} Try a different approach — don't retry the same call, don't attempt to \
             bypass the restriction."
        )
    } else {
        message
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> ToolCall {
        ToolCall::new(format!("id-{name}"), name)
    }

    #[test]
    fn a_swarm_runs_alone() {
        assert_eq!(step_denial(&[call("agent_swarm")], false), None);
        assert_eq!(step_denial(&[call("read_file"), call("bash")], false), None);
        assert_eq!(
            step_denial(&[call("agent_swarm"), call("read_file")], false).unwrap(),
            "agent_swarm must be the only tool call in a model response. Retry with a single \
             agent_swarm call by itself, then call any other tools after it returns."
        );
        let twice = step_denial(&[call("agent_swarm"), call("agent_swarm")], false).unwrap();
        assert!(twice.starts_with("agent_swarm must be called one swarm at a time."));
        assert!(!twice.contains("also must not be combined"));
        let mixed = step_denial(
            &[call("agent_swarm"), call("agent_swarm"), call("bash")],
            true,
        )
        .unwrap();
        assert!(
            mixed.contains("also must not be combined with other tools"),
            "{mixed}"
        );
        assert!(
            mixed.ends_with("don't attempt to bypass the restriction."),
            "{mixed}"
        );
    }

    #[test]
    fn reminders_follow_the_mode() {
        assert_eq!(reminder_due(true, None), Some(ENTER_REMINDER));
        assert_eq!(reminder_due(true, Some(false)), Some(ENTER_REMINDER));
        assert_eq!(reminder_due(true, Some(true)), None);
        assert_eq!(reminder_due(false, Some(true)), Some(EXIT_REMINDER));
        assert_eq!(reminder_due(false, Some(false)), None);
        assert_eq!(reminder_due(false, None), None);

        let entered = vec![Message::user("hi"), reminder_message(ENTER_REMINDER)];
        assert_eq!(last_reminder_was_entry(&entered), Some(true));
        let mut ended = entered.clone();
        ended.push(reminder_message(EXIT_REMINDER));
        assert_eq!(last_reminder_was_entry(&ended), Some(false));
        let merged = Message::user(format!("{EXIT_REMINDER}\n\n{ENTER_REMINDER}"));
        assert_eq!(
            last_reminder_was_entry(&[merged]),
            Some(true),
            "the later one counts"
        );
        assert_eq!(last_reminder_was_entry(&[Message::user("hi")]), None);
    }
}
