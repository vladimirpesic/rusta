use std::io::Write;

use agent_core::{ApprovalHook, ApprovalRequest, ApprovalResponse};
use async_trait::async_trait;

/// Prompt the user on stdin/stdout for tool approval.
pub struct StdinApprovalHook {
    /// Ask with an `approval_request` event rather than a text prompt, for
    /// `--stream-json`, whose stdout carries nothing but JSON events.
    json: bool,
}

impl StdinApprovalHook {
    pub fn new() -> Self {
        Self { json: false }
    }

    /// Ask with an `approval_request` event on stdout; see
    /// [`crate::stream::emit_approval_request`].
    pub fn json() -> Self {
        Self { json: true }
    }
}

impl Default for StdinApprovalHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ApprovalHook for StdinApprovalHook {
    async fn request_approval(&self, request: ApprovalRequest) -> ApprovalResponse {
        // One question at a time, answered before the next is asked.
        let _asking = crate::user_interaction::ASKING.lock().await;
        if self.json {
            crate::stream::emit_approval_request(
                &request.tool_name,
                &request.arguments,
                request.reason(),
                request.requester.clone(),
            );
        } else {
            print!(
                "{}",
                prompt_text(&request, crate::terminal_sanitize::for_stdout)
            );
            let _ = std::io::stdout().flush();
        }

        match crate::user_interaction::read_answer().await {
            Ok(None) => ApprovalResponse::Cancel,
            Ok(Some(line)) => parse_response(line.trim()),
            Err(_) => ApprovalResponse::DenyOnce,
        }
    }
}

/// The question printed for `request`: the call and its arguments, or, for
/// a plan review, the plan itself, readable, through `clean` (the terminal
/// sanitizer).
fn prompt_text(request: &ApprovalRequest, clean: impl Fn(&str) -> String) -> String {
    let asker = request
        .requester
        .as_deref()
        .map_or_else(String::new, |id| format!("sub-agent {id}: "));
    if request.rule == agent_core::permissions::PLAN_REVIEW_RULE {
        let plan = request
            .arguments
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let plan = clean(plan.trim());
        return format!(
            "\n[plan review] {asker}The agent's plan:\n\n{plan}\n\n\
             Approve this plan and leave plan mode? [y=yes/n=no/c=cancel]? "
        );
    }
    let args = serde_json::to_string(&request.arguments).unwrap_or_default();
    let reason = request
        .reason()
        .map_or_else(String::new, |reason| format!(" — {reason}"));
    format!(
        "\n[approve] {asker}{}({}){reason} [y=yes/n=no/a=yes to all/s=deny all/c=cancel]? ",
        request.tool_name, args
    )
}

/// The response `line` names; anything else denies this one call.
fn parse_response(line: &str) -> ApprovalResponse {
    named_response(line).unwrap_or(ApprovalResponse::DenyOnce)
}

fn named_response(line: &str) -> Option<ApprovalResponse> {
    match line.to_lowercase().as_str() {
        "y" | "yes" => Some(ApprovalResponse::AllowOnce),
        "a" | "all" => Some(ApprovalResponse::AllowSession),
        "n" | "no" => Some(ApprovalResponse::DenyOnce),
        "s" | "skip" | "deny" => Some(ApprovalResponse::DenySession),
        "c" | "cancel" | "q" | "quit" => Some(ApprovalResponse::Cancel),
        _ => None,
    }
}

/// Whether `answer` names a response rather than falling back to a denial.
#[cfg(test)]
pub(crate) fn understands(answer: &str) -> bool {
    named_response(answer).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan is shown as the plan, for a person to read, with no escape
    /// sequence of the model's reaching the terminal.
    #[test]
    fn a_plan_review_shows_the_plan_itself() {
        let request = ApprovalRequest::new(
            "exit_plan_mode",
            agent_core::permissions::PLAN_REVIEW_RULE,
            serde_json::json!({ "plan": "## Context\nFix the parser.\x1b[2J" }),
        );
        let text = prompt_text(&request, crate::terminal_sanitize::sanitize_line);
        assert!(text.contains("[plan review]"), "{text}");
        assert!(text.contains("## Context\nFix the parser."), "{text}");
        assert!(!text.contains('\x1b'), "{text}");
        assert!(text.ends_with("[y=yes/n=no/c=cancel]? "), "{text}");

        let request = ApprovalRequest::new(
            "bash",
            "run_command",
            serde_json::json!({ "command": "ls" }),
        );
        assert!(
            prompt_text(&request, str::to_string)
                .starts_with("\n[approve] bash({\"command\":\"ls\"})")
        );
    }
}
