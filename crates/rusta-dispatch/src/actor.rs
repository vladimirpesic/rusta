//! The sub-coder actor — development plan §6.8 (R9).
//!
//! One actor owns one research task: a fresh context (the `Exploring` core
//! prompt plus a short sub-coder addendum plus the task), its own turn cap
//! of six, and the five read-only tools executed through [`RunTool`]. The
//! first tool-free completion is the report; if the turn cap runs out first,
//! a wrap-up prompt forces one. The report is hard-capped at
//! [`REPORT_TOKEN_CAP`] estimated tokens regardless of what the model wrote
//! — the wrap-up prompt asks for the budget, the cap enforces it (§6.1
//! spirit: every observation is bounded).
//!
//! The full sub-transcript is returned for the session log (§6.10 sidecar
//! material) — it never enters the main context.

use std::sync::Arc;

use rusta_core::State;
use rusta_llm::tokens::estimate_tokens;
use rusta_llm::{Backend, ChatRequest, Message};
use serde_json::Value;

use crate::dag::Task;
use crate::toolcall::parse_tool_calls;

/// Maximum research turns per sub-coder (§6.8).
pub const SUB_CODER_TURN_CAP: usize = 6;

/// Report budget in estimated tokens (§6.8).
pub const REPORT_TOKEN_CAP: u64 = 400;

/// The sub-coder toolset (§6.8): read-only research — no shell (DECIDED), no
/// mutation, no dispatch (sub-coders never spawn sub-coders).
pub const SUB_CODER_TOOLS: [&str; 5] = ["read", "grep", "glob", "map_refresh", "map_drill"];

/// Per-observation cap for a sub-coder, in estimated tokens.
///
/// The main loop has a compressor; a sub-coder is a fresh short-lived
/// context and deliberately has none (§6.8). Without a cap, six turns of
/// `read` observations at the §6.1 limit (64 KiB each) is far more than any
/// 32k window holds, and the request fails wholesale — losing the research
/// rather than trimming it. Clipping each observation keeps every turn's
/// findings and bounds the transcript to roughly
/// `SUB_CODER_TURN_CAP × OBSERVATION_TOKEN_CAP`.
pub const OBSERVATION_TOKEN_CAP: u64 = 1_500;

/// Executes one sub-coder tool call. Implemented by the host tool registry
/// against the same handlers as the main loop — one implementation, two
/// entry points. The returned string is the observation text (§6.1 format,
/// `TOOL RESULT <name> (ok|error)\n…`) appended to the sub-coder's context.
///
/// Native `async fn` in trait (Rust 2024) — no `async-trait`, per plan §10;
/// implementations are `Send + Sync + 'static` so actors can be spawned.
pub trait RunTool: Send + Sync + 'static {
    /// Run `name(input)`; the string is the full observation message.
    fn run(&self, name: String, input: Value) -> impl Future<Output = String> + Send;
}

/// One sub-coder's result: the ≤400-token report plus its transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The task's distinct label (§6.8).
    pub label: String,
    /// The report text (already capped).
    pub report: String,
    /// Whether the report was clipped at [`REPORT_TOKEN_CAP`].
    pub truncated: bool,
    /// The actor could not finish (backend failure or panic). A failed
    /// report still re-enters the main context labeled, so the model knows
    /// which research thread produced nothing.
    pub failed: bool,
    /// The full sub-transcript — session-log material only (§6.8).
    pub transcript: Vec<Message>,
}

impl Report {
    /// A failed report: research produced nothing, with a reason.
    pub fn failed(label: &str, reason: &str) -> Self {
        Self {
            label: label.to_owned(),
            report: format!("RESEARCH FAILED: {reason}"),
            truncated: false,
            failed: true,
            transcript: Vec::new(),
        }
    }
}

/// Run one sub-coder to completion.
///
/// `gate` serializes backend completions across actors (the embedded
/// backend's single inference thread, §6.8); `None` lets them overlap.
pub async fn run_actor<R: RunTool>(
    backend: Arc<Backend>,
    runner: R,
    gate: Option<Arc<tokio::sync::Mutex<()>>>,
    task: Task,
) -> Report {
    let mut transcript: Vec<Message> = Vec::with_capacity(2 * SUB_CODER_TURN_CAP + 4);
    transcript.push(rusta_core::prompt::core_prompt(State::Exploring));
    transcript.push(Message::system(addendum(&task)));
    transcript.push(Message::user(task.brief.clone()));

    for _ in 1..=SUB_CODER_TURN_CAP {
        let text = match complete(&backend, &gate, ChatRequest::new(transcript.clone())).await {
            Ok(text) => text,
            Err(err) => {
                return failed_report(task.label, transcript, &format!("backend error: {err}"));
            }
        };
        transcript.push(Message::assistant(text.clone()));
        let parsed = parse_tool_calls(&text);
        if parsed.calls.is_empty() {
            let (report, truncated) = cap_report(&text);
            return Report {
                label: task.label,
                report,
                truncated,
                failed: false,
                transcript,
            };
        }
        for note in parsed.notes {
            transcript.push(Message::user(format!(
                "TOOL RESULT tool_call (error)\n{note}"
            )));
        }
        for call in parsed.calls {
            let observation = if SUB_CODER_TOOLS.contains(&call.name.as_str()) {
                runner.run(call.name.clone(), call.input).await
            } else {
                format!(
                    "TOOL RESULT {} (error)\n\"{}\" is not available to sub-coders. Available tools: {}.",
                    call.name,
                    call.name,
                    SUB_CODER_TOOLS.join(", ")
                )
            };
            transcript.push(Message::user(clip_observation(&observation)));
        }
    }

    // Turn cap reached: one wrap-up completion produces the report (§6.8).
    transcript.push(Message::user(
        "Turn budget reached. Write your final report now: plain text, no tool calls, at most 400 tokens. Lead with findings and cite path:line.",
    ));
    match complete(&backend, &gate, ChatRequest::new(transcript.clone())).await {
        Ok(text) => {
            let (report, truncated) = cap_report(&text);
            Report {
                label: task.label,
                report,
                truncated,
                failed: false,
                transcript,
            }
        }
        Err(err) => failed_report(
            task.label,
            transcript,
            &format!("backend error during wrap-up: {err}"),
        ),
    }
}

/// Clips one sub-coder observation to [`OBSERVATION_TOKEN_CAP`] estimated
/// tokens, char-boundary safe, with an explicit marker so the sub-coder
/// knows to narrow its next read rather than assume it saw everything.
fn clip_observation(text: &str) -> String {
    if estimate_tokens(text) <= OBSERVATION_TOKEN_CAP {
        return text.to_owned();
    }
    let budget = OBSERVATION_TOKEN_CAP as usize * 3; // chars, per the §6.2 heuristic
    let head: String = text.chars().take(budget).collect();
    format!(
        "{head}\n[… clipped at {OBSERVATION_TOKEN_CAP} tokens — narrow the range and read again]"
    )
}

/// A failed report that keeps the transcript for the session log.
fn failed_report(label: String, transcript: Vec<Message>, reason: &str) -> Report {
    Report {
        transcript,
        ..Report::failed(&label, reason)
    }
}

/// One completion, serialized through `gate` when present.
async fn complete(
    backend: &Backend,
    gate: &Option<Arc<tokio::sync::Mutex<()>>>,
    request: ChatRequest,
) -> Result<String, rusta_llm::Error> {
    match gate {
        Some(lock) => {
            let _guard = lock.lock().await;
            backend.complete(request).await
        }
        None => backend.complete(request).await,
    }
}

/// The sub-coder addendum layered over the core prompt (~60 tokens): scope,
/// toolset restriction, turn budget, report contract.
fn addendum(task: &Task) -> String {
    format!(
        "You are a read-only research sub-coder for task {:?}. Tools: read, grep, glob, \
         map_refresh, map_drill — no others. At most {SUB_CODER_TURN_CAP} turns; edit nothing. \
         When research is done, reply with plain text (no tool call): your report, \
         at most {REPORT_TOKEN_CAP} tokens, findings first, cite path:line.",
        task.label
    )
}

/// Clip a report to [`REPORT_TOKEN_CAP`] estimated tokens, char-boundary
/// safe, with an explicit marker when clipping happened. The heuristic
/// overestimates slightly (§6.2), so the cap holds conservatively.
pub fn cap_report(text: &str) -> (String, bool) {
    let trimmed = text.trim();
    if estimate_tokens(trimmed) <= REPORT_TOKEN_CAP {
        return (trimmed.to_owned(), false);
    }
    let budget = REPORT_TOKEN_CAP as usize * 3; // chars per the §6.2 heuristic
    let head: String = trimmed.chars().take(budget).collect();
    (
        format!("{head}\n[report clipped at {REPORT_TOKEN_CAP} tokens]"),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_report_passes_unclipped() {
        let (report, truncated) = cap_report("found the bug at src/main.rs:42");
        assert_eq!(report, "found the bug at src/main.rs:42");
        assert!(!truncated);
    }

    #[test]
    fn oversized_report_is_clipped_with_marker() {
        let text = "x".repeat(4_000); // ≈1334 estimated tokens
        let (report, truncated) = cap_report(&text);
        assert!(truncated);
        assert!(report.ends_with("[report clipped at 400 tokens]"));
        let head = report
            .strip_suffix("\n[report clipped at 400 tokens]")
            .unwrap();
        assert_eq!(head.chars().count(), 400 * 3);
        assert!(estimate_tokens(head) <= REPORT_TOKEN_CAP);
    }

    #[test]
    fn clip_lands_on_char_boundaries_for_multibyte_text() {
        let text = "é".repeat(1_500);
        let (report, truncated) = cap_report(&text);
        assert!(truncated);
        let head = report
            .strip_suffix("\n[report clipped at 400 tokens]")
            .expect("marker present");
        assert!(head.chars().all(|c| c == 'é'));
    }
}
