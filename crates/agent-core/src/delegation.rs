//! How a delegated sub-agent runs: where it works, and the check it must
//! pass.
//!
//! - **`isolation: "worktree"`** gives it a git worktree of its own
//!   ([`crate::worktree`]): a copy of the repository as it stands, its working
//!   directory and only allowed path, whose changes come back into the
//!   session's working tree when it finishes, however it finished.
//! - **`check`** is a shell command Rusta runs when the sub-agent has
//!   finished its task, in its working directory, through the sub-agent's own
//!   `bash` — so the session's hooks, rules, guards and approvals judge it as any
//!   command of the sub-agent's. When it fails, the sub-agent is given the
//!   output and sent back to fix the cause, up to [`CHECK_RETURNS`] times. Its
//!   report ends with how the check went.

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::Agent;
use crate::agent_loop::Llm;
use crate::agent_loop::types::ExecutableToolContext;
use crate::tools::builtin::agent::{RunOutcome, run_bounded};
use crate::worktree::Worktree;

/// How many times a sub-agent whose check failed is sent back to fix it.
pub const CHECK_RETURNS: usize = 2;
/// The most characters of a failed check's output its report quotes.
const CHECK_OUTPUT_CHARS: usize = 3_000;

/// The schema of the `isolation` parameter.
#[must_use]
pub fn isolation_schema(of: &str) -> Value {
    json!({
        "type": "string",
        "enum": ["worktree"],
        "description": format!(
            "\"worktree\": run {of} in a git worktree of its own, a copy of the repository as it stands, uncommitted changes included. What it changes there comes back into the working tree when it finishes: a file changed meanwhile is merged, and a conflict is marked in the file for you to resolve. Use it for sub-agents that write while you or other sub-agents do; it needs a git repository."
        ),
    })
}

/// The schema of the `check` parameter.
#[must_use]
pub fn check_schema(of: &str) -> Value {
    json!({
        "type": "string",
        "description": format!(
            "A shell command Rusta runs when {of} has finished, in its working directory — the tests its work must pass, a build, a linter. If it fails, the sub-agent is given its output and sent back to fix the cause, up to {CHECK_RETURNS} times; the report says how the check ended. It is judged like any of the sub-agent's commands, and runs at most five minutes."
        ),
    })
}

/// How one delegation runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delegation {
    /// Whether the sub-agent works in a worktree of its own.
    pub isolated: bool,
    /// The command Rusta runs when it has finished.
    pub check: Option<String>,
}

impl Delegation {
    /// The delegation `input` asks for, from its `isolation` and `check`.
    ///
    /// # Errors
    /// When either is not a value it takes.
    pub fn parse(input: &Value) -> Result<Self, String> {
        let isolated = match input.get("isolation") {
            None | Some(Value::Null) => false,
            Some(Value::String(mode)) if mode == "worktree" => true,
            Some(other) => {
                return Err(format!(
                    "isolation takes \"worktree\" alone, not {other}; leave it out to work in the \
                     shared working directory."
                ));
            }
        };
        let check = match input.get("check") {
            None | Some(Value::Null) => None,
            Some(Value::String(command)) if !command.trim().is_empty() => {
                Some(command.trim().to_string())
            }
            Some(other) => {
                return Err(format!(
                    "check is a shell command to run, not {other}; leave it out to run none."
                ));
            }
        };
        Ok(Self { isolated, check })
    }

    /// This delegation with `{{item}}` in its check replaced by `item`, for
    /// one member of a swarm.
    #[must_use]
    pub fn for_item(&self, placeholder: &str, item: &str) -> Self {
        Self {
            isolated: self.isolated,
            check: self
                .check
                .as_ref()
                .map(|check| check.replace(placeholder, item)),
        }
    }
}

/// Where a sub-agent works: the context it is spawned from, and its worktree
/// when it has one.
pub struct Place {
    /// The context to spawn the sub-agent from: its parent's, rooted in the
    /// worktree when it has one.
    pub ctx: ExecutableToolContext,
    /// Its worktree, whose work comes back when it finishes.
    pub worktree: Option<Worktree>,
}

impl Place {
    /// Where the sub-agent `id` delegated by the agent of `ctx` works, as
    /// `delegation` asks: a worktree of its own, made now, or its parent's
    /// working directory.
    ///
    /// # Errors
    /// The worktree cannot be made.
    pub async fn new(
        ctx: &ExecutableToolContext,
        delegation: &Delegation,
        id: &str,
    ) -> Result<Self, String> {
        if !delegation.isolated {
            return Ok(Self {
                ctx: ctx.clone(),
                worktree: None,
            });
        }
        let home = ctx.background.data_dir().join("worktrees");
        let worktree = Worktree::create(&ctx.cwd, &home, id).await?;
        let mut placed = ctx.clone();
        placed.cwd = worktree.cwd();
        placed.allowed_paths = std::collections::HashSet::new();
        Ok(Self {
            ctx: placed,
            worktree: Some(worktree),
        })
    }

    /// What the sub-agent is told before its task: where it works, when that
    /// is a worktree, and the check it must pass. Empty for neither.
    #[must_use]
    pub fn briefing(&self, delegation: &Delegation, session_cwd: &std::path::Path) -> String {
        let mut briefing = Vec::new();
        if self.worktree.is_some() {
            briefing.push(format!(
                "You work in a git worktree of your own, {}: a copy of the repository as it \
                 stood when you were launched, uncommitted changes included. Work there, with \
                 paths in it, not in {}, the session's working directory, which you cannot reach. \
                 What you change is brought back into the session's working tree when you \
                 finish; do not commit.",
                self.ctx.cwd.display(),
                session_cwd.display()
            ));
        }
        if let Some(check) = &delegation.check {
            briefing.push(format!(
                "When you have finished, Rusta runs `{check}` in {}. If it fails, you are given \
                 its output and sent back to fix the cause.",
                self.ctx.cwd.display()
            ));
        }
        if briefing.is_empty() {
            String::new()
        } else {
            format!(
                "<system-reminder>\n{}\n</system-reminder>\n\n",
                briefing.join("\n")
            )
        }
    }
}

/// Run the delegated `agent` on `prompt` within `limit`, then its check, then
/// bring its work back from its `worktree`: how the run ended, and the lines
/// its report ends with.
pub(crate) async fn run(
    agent: &mut Agent,
    llm: &dyn Llm,
    prompt: &str,
    cancel: &CancellationToken,
    limit: Option<std::time::Duration>,
    delegation: &Delegation,
    worktree: Option<Worktree>,
) -> (RunOutcome, Vec<String>) {
    let mut outcome = run_bounded(agent, llm, prompt, cancel, limit).await;
    let mut notes = Vec::new();
    if let Some(check) = &delegation.check {
        let note = if finished(&outcome) {
            check_until_it_passes(agent, llm, check, cancel, limit, &mut outcome).await
        } else {
            format!("Check `{check}`: not run, as the sub-agent did not finish.")
        };
        notes.push(note);
    }
    if let Some(worktree) = worktree {
        notes.push(match worktree.bring_back().await {
            Ok(brought) => brought.describe(),
            Err(e) => format!("Its work could not be brought back from its worktree: {e}"),
        });
    }
    (outcome, notes)
}

/// Whether a run ended with the sub-agent's answer.
fn finished(outcome: &RunOutcome) -> bool {
    outcome.timed_out.is_none() && outcome.result.is_ok()
}

/// Run `check` and send the sub-agent back to fix what fails, up to
/// [`CHECK_RETURNS`] times: how the check ended, in a line or a few.
async fn check_until_it_passes(
    agent: &mut Agent,
    llm: &dyn Llm,
    check: &str,
    cancel: &CancellationToken,
    limit: Option<std::time::Duration>,
    outcome: &mut RunOutcome,
) -> String {
    let mut returns = 0;
    loop {
        let (passed, output) = match agent.run_check(check, cancel).await {
            Ok(ran) => ran,
            Err(why) => return format!("Check `{check}`: not run: {}", why.trim()),
        };
        if passed {
            return match returns {
                0 => format!("Check `{check}`: passed."),
                1 => format!("Check `{check}`: passed after the sub-agent was sent back once."),
                n => {
                    format!("Check `{check}`: passed after the sub-agent was sent back {n} times.")
                }
            };
        }
        if returns == CHECK_RETURNS {
            return format!(
                "Check `{check}`: failed, after the sub-agent was sent back {CHECK_RETURNS} \
                 times. Its last output:\n{}",
                tail(&output)
            );
        }
        returns += 1;
        let follow_up = format!(
            "Rusta ran your check `{check}`, and it failed:\n{output}\n\nFind the cause and fix \
             it, then say what you changed."
        );
        *outcome = run_bounded(agent, llm, &follow_up, cancel, limit).await;
        if !finished(outcome) {
            return format!(
                "Check `{check}`: failed, and the sub-agent did not finish fixing it. Its output:\n{}",
                tail(&output)
            );
        }
    }
}

/// The end of a check's output, at most [`CHECK_OUTPUT_CHARS`] characters.
fn tail(output: &str) -> String {
    let count = output.chars().count();
    if count <= CHECK_OUTPUT_CHARS {
        return output.trim_end().to_string();
    }
    let cut: String = output.chars().skip(count - CHECK_OUTPUT_CHARS).collect();
    format!("…{}", cut.trim_end())
}

/// A sub-agent's report with `notes` after it.
#[must_use]
pub fn with_notes(report: String, notes: &[String]) -> String {
    if notes.is_empty() {
        return report;
    }
    format!("{report}\n\n{}", notes.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_and_check_are_read_from_the_input() {
        assert_eq!(
            Delegation::parse(&json!({})).unwrap(),
            Delegation::default()
        );
        assert_eq!(
            Delegation::parse(&json!({ "isolation": "worktree", "check": " cargo test " }))
                .unwrap(),
            Delegation {
                isolated: true,
                check: Some("cargo test".to_string()),
            }
        );
        for wrong in [
            json!({ "isolation": "container" }),
            json!({ "isolation": true }),
            json!({ "check": "" }),
            json!({ "check": 3 }),
        ] {
            assert!(Delegation::parse(&wrong).is_err(), "{wrong}");
        }
        let each = Delegation {
            isolated: false,
            check: Some("pytest tests/test_{{item}}.py".to_string()),
        }
        .for_item("{{item}}", "cart");
        assert_eq!(each.check.as_deref(), Some("pytest tests/test_cart.py"));
    }

    #[test]
    fn a_long_output_is_cut_from_its_start() {
        let long = "x".repeat(CHECK_OUTPUT_CHARS + 10);
        let cut = tail(&long);
        assert!(cut.starts_with('…'));
        assert_eq!(cut.chars().count(), CHECK_OUTPUT_CHARS + 1);
        assert_eq!(tail("short\n"), "short");
        assert_eq!(with_notes("done".into(), &[]), "done");
        assert_eq!(
            with_notes("done".into(), &["Check `x`: passed.".into()]),
            "done\n\nCheck `x`: passed."
        );
    }
}
