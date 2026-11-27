//! The language server's word on the files the agent just wrote or edited.
//!
//! After `write_file` or `edit_file` changes a file, the session's
//! diagnostics tool — Devscriptor's `get_diagnostics`, when the session has
//! it ([`crate::agent_loop::types::ExecutableToolContext::diagnostics_tool`])
//! — is asked for the file's errors, and the agent hears them with the call's
//! result, so a change that broke the build is fixed at once instead of after
//! a test run. It costs nothing when there is nothing to say: a file without
//! errors adds no words, unless it had some the agent was told of, when it
//! hears they are gone.
//!
//! The check waits [`EDIT_CHECK_WAIT`] at most. A language server that takes
//! longer — one starting, one analysing a file for the first time — answers
//! with the next step instead. When the model ends its turn while a check
//! still runs, the end waits for it, [`TURN_END_WAIT`] at most, and an error
//! it finds keeps the turn going one more step: otherwise the turn would end
//! with the file broken and nobody told. A check still running when the next
//! turn starts, or when the file is changed again, is dropped. Errors only, never
//! warnings, and never an answer the server gave for an older text of the file
//! (`stale`). The tool is called only where the agent could call it without
//! asking: it reads only, and no rule or answer of the session refuses it or
//! asks about it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::agent_loop::types::{ExecutableTool, ExecutableToolContext};
use crate::permissions::{PermissionEngine, PolicyCheck};
use crate::sync::LockExt;
use crate::tools::ToolRegistry;

/// The longest a call's result waits for the check of the file it changed.
pub const EDIT_CHECK_WAIT: Duration = Duration::from_secs(3);
/// The longest the end of a turn waits for the checks still running.
pub const TURN_END_WAIT: Duration = Duration::from_secs(15);
/// How long a check lets a server that pushes its diagnostics take, where
/// the tool takes the bound.
const PUSH_WAIT_MS: u64 = 10_000;
/// The most errors one file's words list.
const ERRORS_SHOWN: usize = 20;

/// A check's answer: the file's errors, each as a line to show, or `None`
/// when the server's answer says nothing of the file as it is now.
type Answer = Option<Vec<String>>;

/// One agent's checks: those still running, and what it was told.
#[derive(Debug, Default)]
pub struct EditChecks {
    /// The checks still running, by file.
    pending: HashMap<PathBuf, JoinHandle<Answer>>,
    /// The files the agent was told have errors.
    told: HashMap<PathBuf, usize>,
}

impl EditChecks {
    /// Drop the checks still running: their answers would come after the turn
    /// they were for.
    pub fn drop_pending(&mut self) {
        for (_, check) in self.pending.drain() {
            check.abort();
        }
    }

    /// Keep `check` of `path` running, for a later step; an earlier check of
    /// the file is dropped.
    fn keep(&mut self, path: PathBuf, check: JoinHandle<Answer>) {
        if let Some(earlier) = self.pending.insert(path, check) {
            earlier.abort();
        }
    }

    /// Drop the check of `path` still running: the file changed again.
    fn drop_check(&mut self, path: &Path) {
        if let Some(check) = self.pending.remove(path) {
            check.abort();
        }
    }

    /// Every check still running, taken out.
    fn take_all(&mut self) -> Vec<(PathBuf, JoinHandle<Answer>)> {
        self.pending.drain().collect()
    }

    /// The checks that have finished, taken out of those running.
    fn take_finished(&mut self) -> Vec<(PathBuf, JoinHandle<Answer>)> {
        let finished: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, check)| check.is_finished())
            .map(|(path, _)| path.clone())
            .collect();
        finished
            .into_iter()
            .filter_map(|path| self.pending.remove_entry(&path))
            .collect()
    }

    /// What the agent is to hear of `path` (shown as `shown`) from `answer`:
    /// its errors; that they are gone, when it was told of some; or nothing.
    fn words(&mut self, path: &Path, shown: &str, answer: Answer) -> Option<String> {
        let errors = answer?;
        if errors.is_empty() {
            return self
                .told
                .remove(path)
                .map(|_| format!("The language server reports no errors in {shown} now."));
        }
        self.told.insert(path.to_path_buf(), errors.len());
        let noun = if errors.len() == 1 { "error" } else { "errors" };
        let mut words = format!(
            "The language server reports {} {noun} in {shown} as it is now:",
            errors.len()
        );
        for error in errors.iter().take(ERRORS_SHOWN) {
            words.push('\n');
            words.push_str(error);
        }
        if errors.len() > ERRORS_SHOWN {
            words.push_str(&format!("\n… and {} more.", errors.len() - ERRORS_SHOWN));
        }
        Some(words)
    }
}

/// The diagnostics tool of `ctx`'s agent, when it has one it may call for
/// `path` unasked: in its `registry`, reading only, and allowed by
/// `permissions` as a call of it would be, without a question.
fn usable_tool(
    ctx: &ExecutableToolContext,
    registry: &std::sync::Mutex<ToolRegistry>,
    permissions: &PermissionEngine,
    path: &Path,
) -> Option<Arc<dyn ExecutableTool>> {
    let name = ctx.diagnostics_tool.as_deref()?;
    let tool = registry.lock_or_recover().get(name)?;
    if !tool.is_read_only() {
        return None;
    }
    let arguments = arguments(tool.as_ref(), path);
    let allowed = match permissions.check_policy(name, &arguments, ctx) {
        PolicyCheck::Allow => true,
        PolicyCheck::Deny | PolicyCheck::Prompt { .. } => false,
        PolicyCheck::NoMatch => {
            let rule = tool.approval_rule(&arguments);
            rule == "silent" || permissions.is_allowed(name, rule, &arguments, ctx)
        }
    };
    allowed.then_some(tool)
}

/// The arguments the check passes: the file, and, where the tool takes them,
/// errors alone and a bound on the wait.
fn arguments(tool: &dyn ExecutableTool, path: &Path) -> Value {
    let properties = &tool.tool_definition().parameters["properties"];
    let mut arguments = json!({ "file_path": path.display().to_string() });
    if properties.get("severity").is_some() {
        arguments["severity"] = json!("error");
    }
    if properties.get("wait_ms").is_some() {
        arguments["wait_ms"] = json!(PUSH_WAIT_MS);
    }
    arguments
}

/// Ask `tool` for the errors of `path`.
async fn check(tool: Arc<dyn ExecutableTool>, ctx: ExecutableToolContext, path: PathBuf) -> Answer {
    let arguments = arguments(tool.as_ref(), &path);
    let result = tool.execute(arguments, &ctx, "edit-check").await;
    if result.is_error {
        return None;
    }
    errors_of(&result.text_content(), &shown(&path, &ctx.cwd))
}

/// The errors in a diagnostics tool's answer, each as `path:line:column:
/// message [source code]`; `None` when the answer is not one, or is `stale`,
/// or is `checking` without errors: the server's check of the file had yet to
/// end, so no errors is not a clean file.
/// A diagnostic without a severity counts as an error, as editors take it.
fn errors_of(answer: &str, shown: &str) -> Answer {
    let answer: Value = serde_json::from_str(answer.trim()).ok()?;
    if answer["stale"] == Value::Bool(true) {
        return None;
    }
    let diagnostics = answer["diagnostics"].as_array()?;
    let errors: Vec<String> = diagnostics
        .iter()
        .filter(|d| matches!(d["severity"].as_str(), None | Some("error")))
        .map(|d| {
            let start = &d["range"]["start"];
            let line = start["line"].as_u64().map_or(1, |line| line + 1);
            let column = start["character"].as_u64().map_or(1, |c| c + 1);
            let message = d["message"].as_str().unwrap_or_default().trim();
            let label: Vec<&str> = [d["source"].as_str(), d["code"].as_str()]
                .into_iter()
                .flatten()
                .collect();
            if label.is_empty() {
                format!("{shown}:{line}:{column}: {message}")
            } else {
                format!("{shown}:{line}:{column}: {message} [{}]", label.join(" "))
            }
        })
        .collect();
    (!errors.is_empty() || answer["checking"] != Value::Bool(true)).then_some(errors)
}

/// `path` as the agent's words name it: relative to `cwd` when inside it.
fn shown(path: &Path, cwd: &Path) -> String {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    path.strip_prefix(&cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Check the files the calls `changed` (each call's id and the file it
/// wrote) changed, with the diagnostics tool in `registry` that
/// `permissions` lets run unasked, waiting [`EDIT_CHECK_WAIT`] at most: the
/// words for each call's result, by call id. A check not done by then keeps
/// running, for [`later_words`].
pub async fn check_changed(
    ctx: &ExecutableToolContext,
    registry: &std::sync::Mutex<ToolRegistry>,
    permissions: &PermissionEngine,
    changed: Vec<(String, PathBuf)>,
) -> HashMap<String, String> {
    let mut checks = Vec::new();
    for (id, path) in changed {
        ctx.edit_checks.lock_or_recover().drop_check(&path);
        let Some(tool) = usable_tool(ctx, registry, permissions, &path) else {
            continue;
        };
        let check = tokio::spawn(check(tool, ctx.clone(), path.clone()));
        checks.push((id, path, check));
    }
    let deadline = tokio::time::Instant::now() + EDIT_CHECK_WAIT;
    let mut said = HashMap::new();
    for (id, path, mut check) in checks {
        match tokio::time::timeout_at(deadline, &mut check).await {
            Ok(answer) => {
                let words = ctx.edit_checks.lock_or_recover().words(
                    &path,
                    &shown(&path, &ctx.cwd),
                    answer.ok().flatten(),
                );
                if let Some(words) = words {
                    said.insert(id, words);
                }
            }
            Err(_) => ctx.edit_checks.lock_or_recover().keep(path, check),
        }
    }
    said
}

/// At the end of a turn, the checks still running, waited for
/// [`TURN_END_WAIT`] at most, or until `cancel`: their note, when one found
/// errors, which is worth one more step of the turn. A check that only says
/// errors are gone is told with them, and is not worth a step alone.
pub async fn errors_at_turn_end(
    ctx: &ExecutableToolContext,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<String> {
    let pending = ctx.edit_checks.lock_or_recover().take_all();
    if pending.is_empty() {
        return None;
    }
    let deadline = tokio::time::Instant::now() + TURN_END_WAIT;
    let mut said = Vec::new();
    let mut errors_found = false;
    for (path, mut check) in pending {
        let answer = tokio::select! {
            finished = tokio::time::timeout_at(deadline, &mut check) => {
                finished.ok().and_then(Result::ok).flatten()
            }
            () = cancel.cancelled() => None,
        };
        if !check.is_finished() {
            check.abort();
        }
        errors_found |= answer.as_ref().is_some_and(|errors| !errors.is_empty());
        let words = ctx
            .edit_checks
            .lock_or_recover()
            .words(&path, &shown(&path, &ctx.cwd), answer);
        said.extend(words);
    }
    errors_found.then(|| note(&said))
}

/// The note that carries `said`, the words of checks that answered after
/// their calls' results went out.
fn note(said: &[String]) -> String {
    format!(
        "<system-reminder>\nThe language server answered after the results of your latest \
         changes:\n{}\n</system-reminder>",
        said.join("\n\n")
    )
}

/// The words of the checks that finished since their calls' results went
/// out, as a note for the next request; `None` when none has anything to say.
pub async fn later_words(ctx: &ExecutableToolContext) -> Option<String> {
    let finished = ctx.edit_checks.lock_or_recover().take_finished();
    let mut said = Vec::new();
    for (path, check) in finished {
        let answer = check.await.ok().flatten();
        let words = ctx
            .edit_checks
            .lock_or_recover()
            .words(&path, &shown(&path, &ctx.cwd), answer);
        said.extend(words);
    }
    (!said.is_empty()).then(|| note(&said))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_read_from_the_answer_and_a_stale_answer_says_nothing() {
        let answer = json!({ "diagnostics": [
            { "range": { "start": { "line": 11, "character": 4 } }, "severity": "error",
              "message": "mismatched types", "source": "rustc", "code": "E0308" },
            { "range": { "start": { "line": 2, "character": 0 } }, "severity": "warning",
              "message": "unused variable" },
            { "range": { "start": { "line": 20, "character": 0 } }, "message": "no severity" }
        ]})
        .to_string();
        assert_eq!(
            errors_of(&answer, "src/main.rs"),
            Some(vec![
                "src/main.rs:12:5: mismatched types [rustc E0308]".to_string(),
                "src/main.rs:21:1: no severity".to_string(),
            ])
        );
        let stale = json!({ "diagnostics": [], "stale": true }).to_string();
        assert_eq!(errors_of(&stale, "a.rs"), None);
        assert_eq!(errors_of("not json", "a.rs"), None);
        // A check still running: its errors so far are told, no errors is
        // nothing known.
        let checking = |diagnostics: Value| {
            json!({ "diagnostics": diagnostics, "checking": true }).to_string()
        };
        assert_eq!(errors_of(&checking(json!([])), "a.rs"), None);
        let broken = json!([{
            "range": { "start": { "line": 0, "character": 0 } },
            "severity": "error", "message": "broken"
        }]);
        assert_eq!(
            errors_of(&checking(broken), "a.rs"),
            Some(vec!["a.rs:1:1: broken".to_string()])
        );
        assert_eq!(
            errors_of(&json!({ "diagnostics": [] }).to_string(), "a.rs"),
            Some(vec![])
        );
    }

    /// Errors are told; their going is told once, and only after errors were.
    #[test]
    fn errors_and_their_going_are_told() {
        let mut checks = EditChecks::default();
        let path = Path::new("/w/a.rs");
        assert_eq!(checks.words(path, "a.rs", Some(Vec::new())), None);
        let told = checks
            .words(path, "a.rs", Some(vec!["a.rs:1:1: broken".to_string()]))
            .unwrap();
        assert_eq!(
            told,
            "The language server reports 1 error in a.rs as it is now:\na.rs:1:1: broken"
        );
        assert_eq!(
            checks.words(path, "a.rs", None),
            None,
            "unknown says nothing"
        );
        assert_eq!(
            checks.words(path, "a.rs", Some(Vec::new())).as_deref(),
            Some("The language server reports no errors in a.rs now.")
        );
        assert_eq!(checks.words(path, "a.rs", Some(Vec::new())), None);

        let many: Vec<String> = (0..25).map(|i| format!("e{i}")).collect();
        let told = checks.words(path, "a.rs", Some(many)).unwrap();
        assert!(told.starts_with("The language server reports 25 errors in a.rs"));
        assert!(told.ends_with("e19\n… and 5 more."), "{told}");
    }
}
