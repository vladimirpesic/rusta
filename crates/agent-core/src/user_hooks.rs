//! User hooks: user-configured shell commands fired on session and turn
//! events (`[[hooks]]`).
//!
//! A hook rule names an event, an optional regex matcher, a command and an
//! optional timeout. When an event fires, every matching rule's command runs
//! in the project directory with a snake_case JSON payload on stdin. Exit 0
//! allows (stdout may join the context), exit 2 blocks with stderr as the
//! reason, and anything else — a crash, a timeout, another non-zero code —
//! fails open so a broken hook never blocks the user's work. Only
//! [`UserHookEvent::PreToolUse`], [`UserHookEvent::UserPromptSubmit`] and
//! [`UserHookEvent::Stop`] can block.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::process::Command;
use tokio::{io::AsyncWriteExt, process::Child};

/// Every hook event. `UserPromptQueued` fires when the TUI queues a prompt
/// for after the running turn (Enter while a turn runs): the TUI owns the
/// queue, so it fires the event there, through the session hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum UserHookEvent {
    UserPromptSubmit,
    UserPromptQueued,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Stop,
    TurnStarted,
    StopFailure,
    Interrupt,
    PermissionRequest,
    PermissionResult,
    SessionStart,
    SessionEnd,
    SessionHeartbeat,
    SubagentStart,
    SubagentStop,
    TaskStarted,
    PreCompact,
    PostCompact,
    Notification,
}

impl UserHookEvent {
    /// The event's config/wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        // The serde rename is "PascalCase", which leaves these names as written.
        match self {
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::UserPromptQueued => "UserPromptQueued",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::Stop => "Stop",
            Self::TurnStarted => "TurnStarted",
            Self::StopFailure => "StopFailure",
            Self::Interrupt => "Interrupt",
            Self::PermissionRequest => "PermissionRequest",
            Self::PermissionResult => "PermissionResult",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::SessionHeartbeat => "SessionHeartbeat",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::TaskStarted => "TaskStarted",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Notification => "Notification",
        }
    }

    /// Whether the event's hooks may block the main flow. Every other event is
    /// observation-only: its hooks fire and forget, whatever they return.
    #[must_use]
    pub fn is_blockable(self) -> bool {
        matches!(self, Self::PreToolUse | Self::UserPromptSubmit | Self::Stop)
    }
}

/// One `\[\[hooks\]\]` rule from the user config. Only these four fields exist; an
/// extra one fails the config load.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserHookRule {
    /// The event that triggers the hook.
    pub event: UserHookEvent,
    /// A regex the event's matcher input must match; absent matches all.
    pub matcher: Option<String>,
    /// The shell command to run.
    pub command: String,
    /// Seconds before the hook is terminated (1–600). Defaults to 30.
    pub timeout: Option<u64>,
}

/// Default hook timeout: 30 seconds.
pub const DEFAULT_HOOK_TIMEOUT_S: u64 = 30;

/// Ceiling a hook command's captured output, so a chatty script cannot flood
/// the context through its stdout.
const MAX_CAPTURED_OUTPUT: usize = 100_000;

/// Bytes kept of each output stream: room for [`MAX_CAPTURED_OUTPUT`]
/// characters of any UTF-8. The rest is read and discarded, so a hook that
/// never stops printing cannot grow Rusta's memory.
const MAX_CAPTURED_BYTES: usize = MAX_CAPTURED_OUTPUT * 4;

/// How long a hook's output is still read once the hook has exited. A
/// background process it started can hold the pipes open for good; the
/// hook's own exit is its verdict.
const EOF_GRACE: Duration = Duration::from_millis(200);

impl UserHookRule {
    /// Check the rule is runnable: a non-empty command, a timeout inside
    /// 1–600 s, and a matcher that compiles.
    pub fn validate(&self) -> Result<(), String> {
        if self.command.trim().is_empty() {
            return Err("hook command must not be empty".to_string());
        }
        if let Some(timeout) = self.timeout
            && !(1..=600).contains(&timeout)
        {
            return Err(format!(
                "hook timeout must be between 1 and 600 seconds, got {timeout}"
            ));
        }
        if let Some(matcher) = &self.matcher
            && let Err(e) = regex::Regex::new(matcher)
        {
            return Err(format!(
                "hook matcher {matcher:?} is not a valid regex: {e}"
            ));
        }
        Ok(())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT_S))
    }
}

/// A rule ready to fire, its matcher compiled once, when the hook set is
/// built, rather than on every firing.
struct CompiledRule {
    rule: UserHookRule,
    matcher: Option<regex::Regex>,
}

impl CompiledRule {
    fn new(rule: UserHookRule) -> Result<Self, String> {
        rule.validate()?;
        let matcher = rule
            .matcher
            .as_deref()
            .map(regex::Regex::new)
            .transpose()
            .map_err(|e| format!("hook matcher is not a valid regex: {e}"))?;
        Ok(Self { rule, matcher })
    }

    /// Whether the rule fires for `event` with this matcher input.
    fn matches(&self, event: UserHookEvent, matcher_input: &str) -> bool {
        self.rule.event == event
            && self
                .matcher
                .as_ref()
                .is_none_or(|matcher| matcher.is_match(matcher_input))
    }
}

/// The compiled, session-bound hook set: the user's rules plus the session
/// facts every payload carries. Built once per session from the user config;
/// [`crate::Agent::with_user_hooks`] hands it to the agent, the tool context
/// and the background manager.
pub struct UserHooks {
    rules: Vec<CompiledRule>,
    session_id: String,
    /// Set again when the session gets a title: `/title`, or one made for it.
    session_title: std::sync::Mutex<String>,
    client_type: String,
    cwd: PathBuf,
    started: Instant,
}

/// What firing hooks produced.
#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    /// Set when a hook blocked the operation; the reason to show the user or
    /// feed back to the model. Never set for observation-only events.
    pub blocked: Option<String>,
    /// stdout of the hooks that allowed, trimmed and concatenated.
    pub stdout: String,
}

impl UserHooks {
    /// Build the session's hook set, validating every rule. `cwd` is the
    /// project directory: where hook commands run.
    pub fn new(
        rules: Vec<UserHookRule>,
        session_id: impl Into<String>,
        session_title: impl Into<String>,
        client_type: impl Into<String>,
        cwd: PathBuf,
    ) -> Result<Self, String> {
        let rules = rules
            .into_iter()
            .map(|rule| {
                let event = rule.event;
                CompiledRule::new(rule)
                    .map_err(|e| format!("invalid [[hooks]] rule for {}: {e}", event.name()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            rules,
            session_id: session_id.into(),
            session_title: std::sync::Mutex::new(session_title.into()),
            client_type: client_type.into(),
            cwd,
            started: Instant::now(),
        })
    }

    /// The session's title, as payloads carry it.
    #[must_use]
    pub fn session_title(&self) -> String {
        self.session_title.lock().unwrap().clone()
    }

    /// The session's title from the next event on: one set with `/title`,
    /// or made for the session after a turn.
    pub fn set_session_title(&self, title: impl Into<String>) {
        *self.session_title.lock().unwrap() = title.into();
    }

    /// No rules configured: firing is a no-op everywhere.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether any rule listens for `event`. Heartbeat scheduling and cheap
    /// guards key off this.
    #[must_use]
    pub fn has_event(&self, event: UserHookEvent) -> bool {
        self.rules
            .iter()
            .any(|compiled| compiled.rule.event == event)
    }

    /// Milliseconds since the session (and this hook set) started.
    #[must_use]
    pub fn uptime(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    /// Run every matching hook for `event`, in parallel, and interpret their
    /// results. `matcher_input` is what the rules' regexes test (a tool name,
    /// the prompt text, …) and `fields` are the event's own payload fields.
    ///
    /// Rules with identical commands run once. On an observation-only event
    /// the exit codes are ignored: the hooks cannot block whatever is
    /// happening.
    pub async fn fire(
        &self,
        event: UserHookEvent,
        matcher_input: &str,
        fields: Map<String, Value>,
    ) -> HookOutcome {
        self.fire_with_progress(event, matcher_input, fields, |_| {})
            .await
    }

    /// [`fire`](Self::fire), calling `on_start` with each hook's command just
    /// as its process spawns — SessionEnd's progress display uses it.
    pub async fn fire_with_progress(
        &self,
        event: UserHookEvent,
        matcher_input: &str,
        fields: Map<String, Value>,
        on_start: impl Fn(&str),
    ) -> HookOutcome {
        let matched = self.matched_rules(event, matcher_input);
        if matched.is_empty() {
            return HookOutcome::default();
        }
        let payload = self.payload(event, fields);
        let blockable = event.is_blockable();
        let on_start = &on_start;

        let runs: Vec<_> = matched
            .iter()
            .map(|rule| {
                let payload = payload.clone();
                let cwd = self.cwd.clone();
                let command = rule.command.clone();
                let timeout = rule.timeout();
                async move {
                    let result =
                        run_hook_command(&command, timeout, &payload, &cwd, on_start).await;
                    (command, result)
                }
            })
            .collect();
        let results = futures::future::join_all(runs).await;

        let mut outcome = HookOutcome::default();
        for (command, result) in results {
            match result {
                HookRun::Allow { stdout } => {
                    let stdout = stdout.trim();
                    if !stdout.is_empty() {
                        if !outcome.stdout.is_empty() {
                            outcome.stdout.push_str("\n\n");
                        }
                        outcome.stdout.push_str(stdout);
                    }
                }
                HookRun::Block { reason } if blockable => {
                    let reason = if reason.trim().is_empty() {
                        format!("hook {command:?} exited with code 2")
                    } else {
                        reason.trim().to_string()
                    };
                    tracing::info!("{} hook blocked: {reason}", event.name());
                    if outcome.blocked.is_none() {
                        outcome.blocked = Some(reason);
                    }
                }
                // A block attempt on an observation-only event, or a failing
                // hook: fail open, as documented.
                HookRun::Block { .. } => {
                    tracing::warn!(
                        "{} hook {command:?} tried to block an observation-only event; ignored",
                        event.name()
                    );
                }
            }
        }
        outcome
    }

    /// Fire an observation event without awaiting it, for sync call sites
    /// (sub-agent spawns, background-task bookkeeping). No-op outside a Tokio
    /// runtime and when nothing listens for the event.
    pub fn fire_spawned(
        self: &Arc<Self>,
        event: UserHookEvent,
        matcher_input: impl Into<String>,
        fields: Map<String, Value>,
    ) {
        if !self.has_event(event) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let hooks = Arc::clone(self);
        let matcher_input = matcher_input.into();
        handle.spawn(async move {
            hooks.fire(event, &matcher_input, fields).await;
        });
    }

    /// Matching rules for this firing, identical commands collapsed to one
    /// run.
    fn matched_rules(&self, event: UserHookEvent, matcher_input: &str) -> Vec<&UserHookRule> {
        let mut seen = std::collections::HashSet::new();
        self.rules
            .iter()
            .filter(|compiled| compiled.matches(event, matcher_input))
            .map(|compiled| &compiled.rule)
            .filter(|rule| seen.insert(rule.command.as_str()))
            .collect()
    }

    /// The JSON document a hook receives on stdin: the base session fields
    /// plus the event's own, all snake_case.
    fn payload(&self, event: UserHookEvent, fields: Map<String, Value>) -> String {
        let mut payload = Map::new();
        payload.insert(
            "hook_event_name".to_string(),
            Value::String(event.name().to_string()),
        );
        payload.insert(
            "session_id".to_string(),
            Value::String(self.session_id.clone()),
        );
        payload.insert(
            "session_title".to_string(),
            Value::String(self.session_title()),
        );
        payload.insert(
            "client_type".to_string(),
            Value::String(self.client_type.clone()),
        );
        payload.insert(
            "cwd".to_string(),
            Value::String(self.cwd.to_string_lossy().to_string()),
        );
        for (key, value) in fields {
            payload.entry(key).or_insert(value);
        }
        serde_json::to_string(&payload).unwrap_or_default()
    }
}

/// One hook process's interpreted result.
enum HookRun {
    /// Exit 0 (or any other non-blocking outcome): the turn continues.
    Allow { stdout: String },
    /// Exit 2, or a JSON deny on stdout: the hook wants the operation stopped.
    Block { reason: String },
}

/// Spawn `command` through the shell in `cwd`, feed it the payload on stdin,
/// and collect its verdict. Fail-open on every failure mode: a hook that
/// cannot start, crashes, exits with an unexpected code or overruns its
/// timeout never blocks the user.
async fn run_hook_command(
    command: &str,
    timeout: Duration,
    payload: &str,
    cwd: &Path,
    on_start: &impl Fn(&str),
) -> HookRun {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::process_group::isolate(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!("hook {command:?} could not start: {e}; allowing");
            return HookRun::Allow {
                stdout: String::new(),
            };
        }
    };
    on_start(command);

    if let Some(mut stdin) = child.stdin.take() {
        let payload = payload.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(payload.as_bytes()).await;
            // Closing stdin is what tells the hook the payload is complete.
        });
    }

    let read = collect_child(&mut child);
    let result = match tokio::time::timeout(timeout, read).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                "hook {command:?} timed out after {} s; allowing",
                timeout.as_secs()
            );
            // The hook and what it started.
            let mut child = child;
            let _ = crate::process_group::stop(&mut child, crate::process_group::STOP_GRACE).await;
            return HookRun::Allow {
                stdout: String::new(),
            };
        }
    };

    let (status, stdout, stderr) = match result {
        Ok(collected) => collected,
        Err(e) => {
            tracing::warn!("hook {command:?} could not be collected: {e}; allowing");
            return HookRun::Allow {
                stdout: String::new(),
            };
        }
    };

    if status.success() {
        if let Some(reason) = json_deny_reason(&stdout) {
            return HookRun::Block { reason };
        }
        return HookRun::Allow { stdout };
    }
    match status.code() {
        Some(2) => HookRun::Block {
            reason: truncate(&stderr, MAX_CAPTURED_OUTPUT),
        },
        other => {
            tracing::warn!("hook {command:?} exited with {other:?}; allowing (fail-open)");
            HookRun::Allow {
                stdout: String::new(),
            }
        }
    }
}

/// A finished child's exit status and captured output.
type CollectedChild = (std::process::ExitStatus, String, String);

/// Read `stream` to its end into `kept`, keeping at most `limit` bytes; the
/// rest is read and discarded, so the writer never blocks on a full pipe.
async fn read_bounded(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    kept: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    (&mut *stream).take(limit as u64).read_to_end(kept).await?;
    tokio::io::copy(stream, &mut tokio::io::sink()).await?;
    Ok(())
}

/// Read stdout and stderr, and reap the child. The child's exit ends the
/// collection: its output is read for at most [`EOF_GRACE`] more, since a
/// background process it left running can hold the pipes open.
async fn collect_child(child: &mut Child) -> std::io::Result<CollectedChild> {
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let status = {
        let reading = async {
            tokio::join!(
                read_bounded(&mut stdout, &mut out, MAX_CAPTURED_BYTES),
                read_bounded(&mut stderr, &mut err, MAX_CAPTURED_BYTES)
            )
        };
        tokio::pin!(reading);
        tokio::select! {
            (read_out, read_err) = &mut reading => {
                read_out?;
                read_err?;
                child.wait().await?
            }
            status = child.wait() => {
                let _ = tokio::time::timeout(EOF_GRACE, &mut reading).await;
                status?
            }
        }
    };
    let stringify = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    Ok((
        status,
        truncate(&stringify(out), MAX_CAPTURED_OUTPUT),
        truncate(&stringify(err), MAX_CAPTURED_OUTPUT),
    ))
}

/// A JSON deny on stdout: `{"hookSpecificOutput":{"permissionDecision":"deny",…}}`.
fn json_deny_reason(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let parsed: Value = serde_json::from_str(trimmed).ok()?;
    let output = parsed.get("hookSpecificOutput")?;
    if output.get("permissionDecision")?.as_str()? != "deny" {
        return None;
    }
    let reason = output
        .get("permissionDecisionReason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .unwrap_or("denied by hook");
    Some(reason.to_string())
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}… [truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Commands run through `sh -c`, so a test's command is the script itself.
    fn test_hooks(rules: Vec<UserHookRule>) -> UserHooks {
        UserHooks::new(
            rules,
            "session_test",
            "Test session",
            "rusta_cli",
            std::env::temp_dir(),
        )
        .expect("test hooks are valid")
    }

    fn rule(event: UserHookEvent, command: &str) -> UserHookRule {
        UserHookRule {
            event,
            matcher: None,
            command: command.to_string(),
            timeout: Some(5),
        }
    }

    /// Run one rule and return its outcome.
    async fn run_one(event: UserHookEvent, command: &str) -> HookOutcome {
        let hooks = test_hooks(vec![rule(event, command)]);
        hooks.fire(event, "", Map::new()).await
    }

    #[test]
    fn rules_are_validated() {
        let mut bad_timeout = rule(UserHookEvent::Stop, "true");
        bad_timeout.timeout = Some(601);
        assert!(bad_timeout.validate().is_err());
        bad_timeout.timeout = Some(0);
        assert!(bad_timeout.validate().is_err());

        let mut bad_matcher = rule(UserHookEvent::PreToolUse, "true");
        bad_matcher.matcher = Some("(".to_string());
        assert!(bad_matcher.validate().is_err());

        let empty = rule(UserHookEvent::Stop, "  ");
        assert!(empty.validate().is_err());

        let ok = rule(UserHookEvent::Stop, "true");
        assert!(ok.validate().is_ok());
    }

    #[tokio::test]
    async fn exit_zero_allows_and_collects_stdout() {
        let outcome = run_one(UserHookEvent::Stop, "echo context-bits").await;
        assert_eq!(outcome.blocked, None);
        assert_eq!(outcome.stdout, "context-bits");
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr() {
        let outcome = run_one(UserHookEvent::PreToolUse, "echo nope 1>&2; exit 2").await;
        assert_eq!(outcome.blocked.as_deref(), Some("nope"));
    }

    #[tokio::test]
    async fn json_deny_blocks() {
        let outcome = run_one(
            UserHookEvent::PreToolUse,
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"use rg instead"}}'"#,
        )
        .await;
        assert_eq!(
            outcome.blocked.as_deref(),
            Some("use rg instead"),
            "the JSON deny reason is the block reason"
        );
    }

    #[tokio::test]
    async fn json_deny_without_reason_uses_fallback() {
        let outcome = run_one(
            UserHookEvent::Stop,
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny"}}'"#,
        )
        .await;
        assert_eq!(outcome.blocked.as_deref(), Some("denied by hook"));
    }

    /// Every blockable event honors exit 2 and the JSON deny.
    #[tokio::test]
    async fn all_blockable_events_block() {
        for event in [
            UserHookEvent::PreToolUse,
            UserHookEvent::UserPromptSubmit,
            UserHookEvent::Stop,
        ] {
            let exit2 = run_one(event, "echo reason 1>&2; exit 2").await;
            assert_eq!(
                exit2.blocked.as_deref(),
                Some("reason"),
                "{event:?} must honor exit 2"
            );
            let deny = run_one(
                event,
                r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"no"}}'"#,
            )
            .await;
            assert_eq!(
                deny.blocked.as_deref(),
                Some("no"),
                "{event:?} must honor the JSON deny"
            );
        }
    }

    /// Observation events never block, whatever the hook says.
    #[tokio::test]
    async fn observation_events_cannot_block() {
        for event in [
            UserHookEvent::SessionEnd,
            UserHookEvent::PostToolUse,
            UserHookEvent::TurnStarted,
        ] {
            let outcome = run_one(event, "echo nope 1>&2; exit 2").await;
            assert_eq!(outcome.blocked, None, "{event:?} must not block");
        }
    }

    /// A stream is kept up to its limit and the rest read and discarded: a
    /// writer that never stops cannot grow the buffer, nor block on a full
    /// pipe.
    #[tokio::test]
    async fn output_is_kept_up_to_the_limit_and_drained() {
        use tokio::io::AsyncReadExt;
        let mut flood = tokio::io::repeat(b'y').take(10 * 1024 * 1024);
        let mut kept = Vec::new();
        read_bounded(&mut flood, &mut kept, 4_096).await.unwrap();
        assert_eq!(kept.len(), 4_096);
        let mut rest = Vec::new();
        flood.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty(), "the rest was drained");
    }

    /// A hook that floods its stdout is cut to the captured-output limit.
    #[tokio::test]
    async fn a_flooding_hook_is_captured_bounded() {
        let outcome = run_one(
            UserHookEvent::UserPromptSubmit,
            "head -c 2000000 /dev/zero | tr '\\0' y; exit 0",
        )
        .await;
        assert_eq!(outcome.blocked, None);
        assert!(
            outcome.stdout.ends_with("… [truncated]"),
            "{}",
            &outcome.stdout[outcome.stdout.len() - 40..]
        );
        assert_eq!(
            outcome.stdout.chars().count(),
            MAX_CAPTURED_OUTPUT + "… [truncated]".chars().count()
        );
    }

    /// The hook's exit is its verdict: one that leaves a background process
    /// holding its pipes and exits 2 blocks at once, instead of waiting for
    /// the pipes to close until it times out and fails open.
    #[tokio::test]
    async fn a_block_is_not_lost_to_a_background_process() {
        let started = std::time::Instant::now();
        let outcome = run_one(
            UserHookEvent::PreToolUse,
            "sleep 3 & echo nope 1>&2; exit 2",
        )
        .await;
        assert_eq!(outcome.blocked.as_deref(), Some("nope"));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "decided in {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn crash_fails_open() {
        let outcome = run_one(UserHookEvent::PreToolUse, "kill -9 $$").await;
        assert_eq!(outcome.blocked, None, "a crashed hook must not block");
    }

    #[tokio::test]
    async fn missing_command_fails_open() {
        let outcome = run_one(UserHookEvent::PreToolUse, "command-that-does-not-exist-xyz").await;
        assert_eq!(outcome.blocked, None);
    }

    #[tokio::test]
    async fn timeout_fails_open() {
        let hooks = test_hooks(vec![UserHookRule {
            timeout: Some(1),
            ..rule(UserHookEvent::PreToolUse, "sleep 30")
        }]);
        let start = Instant::now();
        let outcome = hooks.fire(UserHookEvent::PreToolUse, "", Map::new()).await;
        assert_eq!(outcome.blocked, None, "a timed-out hook must not block");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the hook must be terminated, not waited out"
        );
    }

    #[tokio::test]
    async fn non_zero_non_two_fails_open() {
        let outcome = run_one(UserHookEvent::PreToolUse, "exit 1").await;
        assert_eq!(outcome.blocked, None);
    }

    #[tokio::test]
    async fn matcher_regex_filters_rules() {
        let hooks = test_hooks(vec![
            UserHookRule {
                matcher: Some("^Bash$".to_string()),
                ..rule(UserHookEvent::PreToolUse, "echo blocked 1>&2; exit 2")
            },
            UserHookRule {
                matcher: Some("bash".to_string()),
                ..rule(UserHookEvent::PreToolUse, "echo other 1>&2; exit 2")
            },
        ]);
        let blocked = hooks
            .fire(UserHookEvent::PreToolUse, "Bash", Map::new())
            .await;
        assert_eq!(blocked.blocked.as_deref(), Some("blocked"));
        let blocked = hooks
            .fire(UserHookEvent::PreToolUse, "bash_tool", Map::new())
            .await;
        assert_eq!(
            blocked.blocked.as_deref(),
            Some("other"),
            "the matcher is a search, not a full match"
        );
        let blocked = hooks
            .fire(UserHookEvent::PreToolUse, "grep", Map::new())
            .await;
        assert_eq!(blocked.blocked, None, "no matcher must match");
    }

    #[tokio::test]
    async fn identical_commands_run_once() {
        let counter =
            std::env::temp_dir().join(format!("rusta-hooks-dedup-{}", std::process::id()));
        let _ = std::fs::remove_file(&counter);
        let command = format!("echo x >> '{}'", counter.display());
        let hooks = test_hooks(vec![
            rule(UserHookEvent::PreToolUse, &command),
            rule(UserHookEvent::PreToolUse, &command),
            UserHookRule {
                matcher: Some("nomatch".to_string()),
                ..rule(UserHookEvent::PreToolUse, &command)
            },
        ]);
        hooks.fire(UserHookEvent::PreToolUse, "", Map::new()).await;
        let lines = std::fs::read_to_string(&counter)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(lines, 1, "identical commands must run exactly once");
        let _ = std::fs::remove_file(&counter);
    }

    /// Hooks for the same event run in parallel, not one after another.
    #[tokio::test]
    async fn matching_hooks_run_in_parallel() {
        let hooks = test_hooks(vec![
            rule(UserHookEvent::SessionEnd, "sleep 1"),
            rule(UserHookEvent::SessionEnd, "sleep 1"),
        ]);
        let start = Instant::now();
        hooks.fire(UserHookEvent::SessionEnd, "", Map::new()).await;
        assert!(
            start.elapsed() < Duration::from_millis(1900),
            "two 1 s hooks ran sequentially"
        );
    }

    /// A title the session gets later is in the payloads from then on.
    #[tokio::test]
    async fn payloads_carry_the_title_set_later() {
        let dir = tempfile::tempdir().unwrap();
        let capture = dir.path().join("payload.json");
        let hooks = UserHooks::new(
            vec![rule(
                UserHookEvent::PreToolUse,
                &format!("cat > '{}'", capture.display()),
            )],
            "session_abc",
            "",
            "rusta_cli",
            dir.path().to_path_buf(),
        )
        .unwrap();
        hooks.set_session_title("Fix the login page");
        assert_eq!(hooks.session_title(), "Fix the login page");
        hooks
            .fire(UserHookEvent::PreToolUse, "Bash", Map::new())
            .await;
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();
        assert_eq!(payload["session_title"], json!("Fix the login page"));
    }

    #[tokio::test]
    async fn payload_carries_base_and_event_fields() {
        let capture =
            std::env::temp_dir().join(format!("rusta-hooks-payload-{}", std::process::id()));
        let _ = std::fs::remove_file(&capture);
        let command = format!("cat > '{}'", capture.display());
        let hooks = UserHooks::new(
            vec![rule(UserHookEvent::PreToolUse, &command)],
            "session_abc",
            "Fix the login page",
            "rusta_cli",
            std::env::temp_dir(),
        )
        .unwrap();
        let mut fields = Map::new();
        fields.insert("tool_name".to_string(), json!("Bash"));
        fields.insert("tool_input".to_string(), json!({"command": "ls"}));
        hooks.fire(UserHookEvent::PreToolUse, "Bash", fields).await;
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();
        assert_eq!(payload["hook_event_name"], json!("PreToolUse"));
        assert_eq!(payload["session_id"], json!("session_abc"));
        assert_eq!(payload["session_title"], json!("Fix the login page"));
        assert_eq!(payload["client_type"], json!("rusta_cli"));
        assert_eq!(
            payload["cwd"],
            json!(std::env::temp_dir().to_string_lossy().as_ref())
        );
        assert_eq!(payload["tool_name"], json!("Bash"));
        assert_eq!(payload["tool_input"]["command"], json!("ls"));
        let _ = std::fs::remove_file(&capture);
    }

    #[test]
    fn unknown_rule_fields_are_rejected() {
        let parsed: Result<UserHookRule, _> = toml::from_str(
            r#"
event = "Stop"
command = "true"
extra = "nope"
"#,
        );
        assert!(parsed.is_err(), "unknown fields must fail the config load");
    }

    #[test]
    fn event_names_deserialize_pascal_case() {
        let rule: UserHookRule = toml::from_str(
            r#"
event = "UserPromptSubmit"
command = "true"
"#,
        )
        .unwrap();
        assert_eq!(rule.event, UserHookEvent::UserPromptSubmit);
        assert!(
            toml::from_str::<UserHookRule>("event = \"NoSuchEvent\"\ncommand = \"true\"\n")
                .is_err()
        );
    }
}
