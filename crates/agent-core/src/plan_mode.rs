//! What the model is told about plan mode: plan mode is the user's to turn on,
//! a planning turn ends in a question or a plan for review, and a plain
//! question still gets a plain answer.
//!
//! A turn of the session's agent starts with the reminder it needs: the full
//! one when plan mode is on and the conversation no longer carries it, a
//! short one while it stays on, and the exit one once it is off.

use lm_bridge::message::Message;

/// The plan-mode note of the session's system prompt.
pub const GUIDANCE: &str = "Plan mode is a read-only planning state that only the user can turn \
     on, with `/plan` in the REPL or the TUI or `--plan` at startup; you cannot enter it \
     yourself. If the user asks for plan mode, tell them how to turn it on. When they ask you to \
     plan first, wait for their go-ahead before changing code.";

/// The reminder that starts plan mode.
pub const FULL_REMINDER: &str = "Plan mode is active: the user turned it on to review a plan \
before anything changes. You MUST NOT make any edits or otherwise make changes to the system \
unless a tool request is explicitly approved. Prefer read-only tools. Use `bash` only when \
needed; it follows the normal permission mode and rules. This supersedes any other instructions \
you have received. `cron_create` and `cron_delete` are also refused in plan mode.

Workflow:
  1. Understand — explore the codebase with `glob`, `grep` and `read_file`. When you are unsure of the codebase structure or the relevant code paths, investigate with `agent` (`subagent_type: \"explore\"`) first.
  2. Design — converge on the best approach; consider trade-offs but aim for a single recommendation.
  3. Review — re-read key files to verify understanding.
  4. Present — call `exit_plan_mode` with the plan, using the structure in its description. The user reviews the plan before plan mode ends, in every permission mode.

## Questions
Settle what the environment can answer before asking anything: never ask the user for facts you can find in the code, configuration, or docs — look them up. Explore at least once before your first question, unless the request contradicts itself or is unclear in a way the code cannot resolve.
Ask only about preferences, constraints, or trade-offs that the code cannot settle and that would change the plan, and give your recommendation with each question.
When nobody can answer a question, and whenever the user dismisses one, decide from context and record the decision under Assumptions in the plan.

## Handling multiple approaches
Keep it focused: at most 2-3 meaningfully different approaches. Do NOT pad with minor variations — if one approach is clearly superior, just propose that one.
When the choice between them is the user's, ask with `ask_user_question` before you write the plan, then plan the chosen approach only.

Never ask about plan approval via text or `ask_user_question`.
End a planning turn with either `ask_user_question` (to clarify a preference that changes the plan) or `exit_plan_mode` (to request plan approval). When the user's message calls for an answer rather than a plan, such as a question about the code, answer in text; plan mode stays on until the user approves a plan or turns it off.";

/// The reminder for each later turn while plan mode stays on.
pub const SPARSE_REMINDER: &str = "Plan mode still active (see full instructions earlier). \
Read-only: make no edits or other changes unless a tool request is explicitly approved. Use \
`bash` only when needed; it follows the normal permission mode and rules. Use \
`ask_user_question` only for preferences the code cannot settle; when nobody can answer, decide \
and list the decision under Assumptions. End planning turns with `ask_user_question` (for \
clarifications) or `exit_plan_mode` (for approval); answer in text when the user's message calls \
for an answer rather than a plan. Never ask about plan approval via text or `ask_user_question`.";

/// The reminder that ends plan mode.
pub const EXIT_REMINDER: &str = "Plan mode is no longer active. The read-only restrictions from \
plan mode no longer apply. If the user approved a plan, carry it out using the normal tool and \
permission rules; otherwise follow the user's latest message.";

/// The first words of each reminder, by which the conversation shows which
/// of them it carries.
const FULL_MARKER: &str = "Plan mode is active: the user turned it on";
const SPARSE_MARKER: &str = "Plan mode still active (see full instructions earlier).";
const EXIT_MARKER: &str = "Plan mode is no longer active.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Full,
    Sparse,
    Exit,
}

/// `reminder` as the conversation carries it.
#[must_use]
pub fn reminder_message(reminder: &str) -> Message {
    Message::user(format!("<system-reminder>\n{reminder}\n</system-reminder>"))
}

/// The plan-mode reminders in `history`, in order. A message can hold more
/// than one when the history merges adjacent messages.
fn reminders(history: &[Message]) -> Vec<Kind> {
    history
        .iter()
        .flat_map(|message| {
            let text = message.text_content();
            let mut found: Vec<(usize, Kind)> = [
                (FULL_MARKER, Kind::Full),
                (SPARSE_MARKER, Kind::Sparse),
                (EXIT_MARKER, Kind::Exit),
            ]
            .into_iter()
            .flat_map(|(marker, kind)| {
                text.match_indices(marker)
                    .map(move |(at, _)| (at, kind))
                    .collect::<Vec<_>>()
            })
            .collect();
            found.sort_by_key(|(at, _)| *at);
            found.into_iter().map(|(_, kind)| kind)
        })
        .collect()
}

/// The reminder a turn needs, given whether plan mode is `active` and the
/// conversation so far: the full reminder when plan mode is on and the
/// conversation does not carry it since plan mode last ended (or since a
/// compaction took it), the sparse one when it does, and the exit reminder
/// when plan mode is off after having been said.
#[must_use]
pub fn reminder_due(active: bool, history: &[Message]) -> Option<&'static str> {
    let said = reminders(history);
    let since_exit = said
        .iter()
        .rposition(|kind| *kind == Kind::Exit)
        .map_or(&said[..], |exit| &said[exit + 1..]);
    match (active, said.last()) {
        (true, _) if since_exit.contains(&Kind::Full) => Some(SPARSE_REMINDER),
        (true, _) => Some(FULL_REMINDER),
        (false, Some(Kind::Full | Kind::Sparse)) => Some(EXIT_REMINDER),
        (false, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(reminders: &[&str]) -> Vec<Message> {
        reminders
            .iter()
            .flat_map(|r| [Message::user("a task"), reminder_message(r)])
            .collect()
    }

    #[test]
    fn plan_mode_turning_on_gets_the_full_reminder() {
        assert_eq!(reminder_due(true, &[]), Some(FULL_REMINDER));
        assert_eq!(
            reminder_due(true, &history(&[FULL_REMINDER, EXIT_REMINDER])),
            Some(FULL_REMINDER),
            "a second time on is said in full again"
        );
    }

    #[test]
    fn later_turns_in_plan_mode_get_the_short_reminder() {
        assert_eq!(
            reminder_due(true, &history(&[FULL_REMINDER])),
            Some(SPARSE_REMINDER)
        );
        assert_eq!(
            reminder_due(true, &history(&[FULL_REMINDER, SPARSE_REMINDER])),
            Some(SPARSE_REMINDER)
        );
    }

    /// A compaction can take the full reminder and keep a short one, which
    /// points at instructions no longer there.
    #[test]
    fn a_short_reminder_without_the_full_one_is_followed_by_the_full_one() {
        assert_eq!(
            reminder_due(true, &history(&[SPARSE_REMINDER])),
            Some(FULL_REMINDER)
        );
        assert_eq!(
            reminder_due(
                true,
                &history(&[FULL_REMINDER, EXIT_REMINDER, SPARSE_REMINDER])
            ),
            Some(FULL_REMINDER)
        );
    }

    #[test]
    fn plan_mode_ending_is_said_once() {
        assert_eq!(
            reminder_due(false, &history(&[FULL_REMINDER])),
            Some(EXIT_REMINDER)
        );
        assert_eq!(
            reminder_due(false, &history(&[FULL_REMINDER, SPARSE_REMINDER])),
            Some(EXIT_REMINDER)
        );
        assert_eq!(
            reminder_due(false, &history(&[FULL_REMINDER, EXIT_REMINDER])),
            None
        );
        assert_eq!(reminder_due(false, &[]), None);
    }

    /// Merged into one message, the reminders still count in order.
    #[test]
    fn reminders_merged_into_one_message_count_in_order() {
        let merged = vec![Message::user(format!(
            "{}\n{}",
            reminder_message(FULL_REMINDER).text_content(),
            reminder_message(EXIT_REMINDER).text_content()
        ))];
        assert_eq!(reminder_due(false, &merged), None);
        assert_eq!(reminder_due(true, &merged), Some(FULL_REMINDER));
    }
}
