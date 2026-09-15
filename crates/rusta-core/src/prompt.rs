//! Core prompt compiler — ADR §6.6 (R7), milestone M3.
//!
//! The core system prompt is a CI invariant: **under 500 estimated tokens**
//! in every phase ([`CORE_PROMPT_TOKEN_BUDGET`]). Its only dynamic part is
//! the state line — persona, tool one-liners, call syntax, edit example,
//! output rules, and git footer are constant, and everything else a model
//! might need is injected JIT or not at all (ADR §6.6). Token counts use
//! rusta-llm's conservative `ceil(chars / 3)` estimator, so the measured
//! invariant is stricter than any real tokenizer.
//!
//! Tool one-liners come from `Tool::one_liner`, listed for exactly the
//! tools `State::tools()` registers in the current phase — the registry and
//! the prompt are the same list and cannot drift apart, and the model is
//! never offered a tool the §6.4 gate will refuse.

use rusta_llm::Message;
use rusta_llm::tokens::estimate_tokens;

use crate::session::Status;
use crate::state::State;

/// Hard cap for the compiled core prompt, in estimated tokens (§6.6).
pub const CORE_PROMPT_TOKEN_BUDGET: u64 = 500;

/// Compiles the core system prompt for `state` (§6.6 layout: persona ·
/// state + exit gate · tool one-liners + call syntax · SEARCH/REPLACE
/// example · output rules · git footer).
pub fn core_prompt(state: State) -> Message {
    Message::system(render(state))
}

/// Estimated token cost of the core prompt for `state` — the measure
/// behind the < 500 CI invariant.
pub fn core_prompt_tokens(state: State) -> u64 {
    estimate_tokens(&render(state))
}

/// Formats a tool result per the §6.1 observation contract: a `user`-role
/// block headed `TOOL RESULT <name> (ok|error)` followed by the already
/// capped content.
pub fn observation(name: &str, status: Status, content: &str) -> Message {
    let flag = match status {
        Status::Ok => "ok",
        Status::Error => "error",
    };
    Message::user(format!("TOOL RESULT {name} ({flag})\n{content}"))
}

/// Renders the core prompt. Built from concatenated sections; the layout
/// order is normative (§6.6).
fn render(state: State) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    writeln!(
        out,
        "You are Rusta, a careful coding agent: explore the repo, plan changes, apply exact edits, verify."
    )
    .expect("infallible");
    writeln!(
        out,
        "STATE: {}. Exit gate: {}.",
        state.name(),
        state.exit_gate()
    )
    .expect("infallible");
    writeln!(out, "TOOLS — call one per fenced block, e.g.:").expect("infallible");
    writeln!(out, "```tool").expect("infallible");
    writeln!(
        out,
        "{{\"name\": \"read\", \"input\": {{\"path\": \"src/main.rs\"}}}}"
    )
    .expect("infallible");
    writeln!(out, "```").expect("infallible");
    // Only the phase's registered tools (§6.4): naming `edit`/`write`/
    // `shell` while they are gated invites blocked calls and wastes turns.
    for tool in state.tools() {
        writeln!(out, "- {}", tool.one_liner()).expect("infallible");
    }
    write!(
        out,
        "EDITS are plain text, never JSON:
src/file.rs
<<<<<<< SEARCH
exact existing lines
=======
changed lines
>>>>>>> REPLACE
RULES:
- Read before editing; a SEARCH block must match the file exactly, whitespace included.
- For a change: draft a short numbered plan, then wait for approval before editing.
- Prefer map_drill to whole-file reads for large files.
- No tool or edit needed? Answer in plain prose and end your turn.
GIT: applied batches auto-commit as \"rusta: <summary>\"; never push or force-push."
    )
    .expect("infallible");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::STATES;

    #[test]
    fn core_prompt_is_under_budget_in_every_state() {
        for state in STATES {
            let tokens = core_prompt_tokens(state);
            assert!(
                tokens < CORE_PROMPT_TOKEN_BUDGET,
                "core prompt for {state} is {tokens} estimated tokens"
            );
        }
    }

    #[test]
    fn observation_follows_the_wire_contract() {
        let ok = observation("read", Status::Ok, "42 lines");
        assert_eq!(ok.content, "TOOL RESULT read (ok)\n42 lines");
        let error = observation("grep", Status::Error, "bad regex");
        assert_eq!(error.content, "TOOL RESULT grep (error)\nbad regex");
    }
}
