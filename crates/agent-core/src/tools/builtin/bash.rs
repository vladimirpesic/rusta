use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses, ToolEvent,
    ToolStatus,
};

const DEFAULT_TIMEOUT_SECONDS: u64 = 60;
const MAX_TIMEOUT_SECONDS: u64 = 5 * 60;
const DEFAULT_BACKGROUND_TIMEOUT_SECONDS: u64 = 10 * 60;
const MAX_BACKGROUND_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;
/// Maximum bytes of combined stdout/stderr to capture before truncating.
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const TRUNCATION_MARKER: &str = "\n[Output truncated after 16 MiB]";

pub struct BashTool {
    tool: Tool,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "bash",
                "Run a shell command. Use with care; destructive commands require approval.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to execute" },
                        "stdin": { "type": "string", "description": "Input written to the command's stdin. Without it the command's stdin is empty: it never reads the terminal" },
                        "env": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                            "description": "Additional environment variables"
                        },
                        "cwd": { "type": "string", "description": "Working directory (default: agent cwd)" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds. Foreground default 60, max 300. Background default 600, max 86400. A foreground command that exceeds its timeout is moved to the background (not killed) unless auto_background_on_timeout is false." },
                        "run_in_background": { "type": "boolean", "description": "Run detached in the background and return a task id" },
                        "background": { "type": "boolean", "description": "Deprecated alias for run_in_background" },
                        "disable_timeout": { "type": "boolean", "description": "Do not apply a timeout. Only applies when run_in_background is true." },
                        "auto_background_on_timeout": { "type": "boolean", "description": "When a foreground command exceeds its timeout, keep it running by moving it to the background and returning a task id instead of killing it (default true)." },
                        "description": { "type": "string", "description": "Short description of the background task (required when run_in_background is true)" }
                    },
                    "required": ["command"]
                }),
            ),
        }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for BashTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, input: &serde_json::Value) -> ToolAccesses {
        let cwd = resolve_cwd(input, &std::path::PathBuf::from("."));
        ToolAccesses::read_write_tree(cwd)
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        "run_command"
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        tool_call_id: &str,
    ) -> ExecutableToolResult {
        let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if command.trim().is_empty() {
            return ExecutableToolResult::error("Empty command");
        }

        // Neither the command nor its `cwd` is fenced here: a command can `cd`
        // anywhere, so a workspace check on `cwd` would guard nothing.
        // Dangerous commands are the permission engine's to ask about
        // (`dangerous_command_guard`).
        let cwd = resolve_cwd(&input, &ctx.cwd);

        let run_in_background = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .or_else(|| input.get("background").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        let disable_timeout = input
            .get("disable_timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let auto_background_on_timeout = input
            .get("auto_background_on_timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        if run_in_background {
            let description = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if description.trim().is_empty() {
                return ExecutableToolResult::error(
                    "description is required when run_in_background is true",
                );
            }
            let env = collect_env(&input);
            let timeout = normalize_timeout(
                input.get("timeout").and_then(|v| v.as_u64()),
                true,
                disable_timeout,
            )
            .map(Duration::from_secs);
            let id = ctx.background.spawn(command, cwd, env, timeout);
            return ExecutableToolResult::text(format!(
                "task_id: {id}\n\
                 description: {description}\n\
                 status: running\n\
                 automatic_notification: true\n\
                 next_step: Continue your current work; the answer will arrive automatically when the task completes.\n\
                 next_step: Use task_output with this task_id for a non-blocking status/output snapshot.\n\
                 next_step: Use task_stop only if the task must be cancelled."
            ));
        }

        let timeout_secs =
            normalize_timeout(input.get("timeout").and_then(|v| v.as_u64()), false, false)
                .expect("foreground timeout is always bounded");

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(&cwd);
        cmd.envs(collect_env(&input));

        // The command reads what the call gives it, or nothing: never
        // Rusta's own input, which is the terminal or, headless, the
        // approval answers.
        if input.get("stdin").is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::process_group::isolate(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ExecutableToolResult::error(format!("Failed to spawn shell: {e}")),
        };
        // A call dropped while its command runs takes what it started along.
        let mut group = crate::process_group::KillOnDrop::new(&child);

        if let Some(stdin_text) = input.get("stdin").and_then(|v| v.as_str())
            && let Some(mut stdin) = child.stdin.take()
        {
            use tokio::io::AsyncWriteExt;
            if let Err(e) = stdin.write_all(stdin_text.as_bytes()).await {
                return ExecutableToolResult::error(format!("Failed to write stdin: {e}"));
            }
        }

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let stdout_reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        let mut stdout_lines = stdout_reader.lines();
        let mut stderr_lines = stderr_reader.lines();

        let mut stdout_text = String::new();
        let mut stderr_text = String::new();
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut timed_out = false;
        let mut capped = false;
        let mut total_bytes = 0usize;

        let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            loop {
                if stdout_done && stderr_done {
                    break;
                }
                tokio::select! {
                    line = stdout_lines.next_line(), if !stdout_done && !capped => {
                        match line {
                            Ok(Some(l)) => {
                                let line_bytes = l.len().saturating_add(1);
                                if total_bytes.saturating_add(line_bytes) > MAX_OUTPUT_BYTES {
                                    stdout_text.push_str(TRUNCATION_MARKER);
                                    capped = true;
                                    stdout_done = true;
                                    stderr_done = true;
                                    crate::process_group::kill(&mut child);
                                    break;
                                }
                                total_bytes += line_bytes;
                                ctx.emit_tool_event(ToolEvent::Progress {
                                    id: tool_call_id.to_string(),
                                    stdout_chunk: Some(format!("{l}\n")),
                                    stderr_chunk: None,
                                    status: ToolStatus::Running,
                                });
                                if !stdout_text.is_empty() {
                                    stdout_text.push('\n');
                                }
                                stdout_text.push_str(&l);
                            }
                            Ok(None) => stdout_done = true,
                            Err(e) => {
                                ctx.emit_tool_event(ToolEvent::Progress {
                                    id: tool_call_id.to_string(),
                                    stdout_chunk: None,
                                    stderr_chunk: Some(format!("stdout read error: {e}\n")),
                                    status: ToolStatus::Running,
                                });
                                stdout_done = true;
                            }
                        }
                    }
                    line = stderr_lines.next_line(), if !stderr_done && !capped => {
                        match line {
                            Ok(Some(l)) => {
                                let line_bytes = l.len().saturating_add(1);
                                if total_bytes.saturating_add(line_bytes) > MAX_OUTPUT_BYTES {
                                    stderr_text.push_str(TRUNCATION_MARKER);
                                    capped = true;
                                    stdout_done = true;
                                    stderr_done = true;
                                    crate::process_group::kill(&mut child);
                                    break;
                                }
                                total_bytes += line_bytes;
                                ctx.emit_tool_event(ToolEvent::Progress {
                                    id: tool_call_id.to_string(),
                                    stdout_chunk: None,
                                    stderr_chunk: Some(format!("{l}\n")),
                                    status: ToolStatus::Running,
                                });
                                if !stderr_text.is_empty() {
                                    stderr_text.push('\n');
                                }
                                stderr_text.push_str(&l);
                            }
                            Ok(None) => stderr_done = true,
                            Err(e) => {
                                ctx.emit_tool_event(ToolEvent::Progress {
                                    id: tool_call_id.to_string(),
                                    stdout_chunk: None,
                                    stderr_chunk: Some(format!("stderr read error: {e}\n")),
                                    status: ToolStatus::Running,
                                });
                                stderr_done = true;
                            }
                        }
                    }
                }
            }
        })
        .await;

        if result.is_err() {
            // The foreground deadline fired. Rather than kill a command that may
            // be mid-work, move the still-running process to the background under
            // a fresh, longer deadline and hand back a task id.
            // Opt out with `auto_background_on_timeout: false`.
            if auto_background_on_timeout {
                // The background task owns the command from here on.
                group.disarm();
                let description = command_summary(command);
                let bg_timeout = Duration::from_secs(DEFAULT_BACKGROUND_TIMEOUT_SECONDS);
                let id = ctx.background.adopt_child(
                    command,
                    cwd,
                    child,
                    (stdout_lines, stderr_lines),
                    stdout_text,
                    stderr_text,
                    Some(bg_timeout),
                );
                return ExecutableToolResult::text(format!(
                    "task_id: {id}\n\
                     description: {description}\n\
                     status: running (auto-backgrounded after the {timeout_secs}s foreground timeout)\n\
                     automatic_notification: true\n\
                     next_step: The command exceeded its foreground timeout and was moved to the background instead of being killed; it keeps running.\n\
                     next_step: Continue your current work; the result arrives automatically when it finishes.\n\
                     next_step: Use task_output with this task_id for a non-blocking snapshot, or task_stop to cancel it."
                ));
            }
            timed_out = true;
            crate::process_group::kill(&mut child);
        }

        let exit_status = match child.wait().await {
            Ok(s) => s,
            Err(e) => {
                return ExecutableToolResult::error(format!("Failed to wait for command: {e}"));
            }
        };
        // What a finished command left running on purpose (`server &`) stays.
        group.disarm();

        if timed_out {
            return ExecutableToolResult::error(format!(
                "Command timed out after {timeout_secs}s\n{stdout_text}\nSTDERR:\n{stderr_text}"
            ));
        }

        let mut text = String::new();
        if !stdout_text.is_empty() {
            text.push_str(&stdout_text);
        }
        if !stderr_text.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("STDERR:\n");
            text.push_str(&stderr_text);
        }

        if exit_status.success() {
            ExecutableToolResult::text(text)
        } else {
            ExecutableToolResult::error(format!(
                "Exit code {}\n{text}",
                exit_status.code().unwrap_or(-1)
            ))
        }
    }
}

fn collect_env(input: &serde_json::Value) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("NO_COLOR".to_string(), "1".to_string());
    env.insert("TERM".to_string(), "dumb".to_string());
    env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    env.insert("PIP_REQUIRE_VIRTUALENV".to_string(), "false".to_string());
    env.insert("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string());
    if let Some(map) = input.get("env").and_then(|v| v.as_object()) {
        for (key, value) in map {
            if let Some(val) = value.as_str() {
                env.insert(key.clone(), val.to_string());
            }
        }
    }
    env
}

/// A short, single-line label for a command, used as the description of a task
/// that was auto-backgrounded on timeout.
fn command_summary(command: &str) -> String {
    let one_line = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 80 {
        let truncated: String = one_line.chars().take(77).collect();
        format!("{truncated}...")
    } else {
        one_line
    }
}

fn resolve_cwd(input: &serde_json::Value, default: &std::path::Path) -> PathBuf {
    input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .map(|p| if p.is_absolute() { p } else { default.join(p) })
        .unwrap_or_else(|| default.to_path_buf())
}

fn normalize_timeout(
    timeout: Option<u64>,
    is_background: bool,
    disable_timeout: bool,
) -> Option<u64> {
    if is_background && disable_timeout {
        return None;
    }
    let cap = if is_background {
        MAX_BACKGROUND_TIMEOUT_SECONDS
    } else {
        MAX_TIMEOUT_SECONDS
    };
    let default = if is_background {
        DEFAULT_BACKGROUND_TIMEOUT_SECONDS
    } else {
        DEFAULT_TIMEOUT_SECONDS
    };
    Some(timeout.unwrap_or(default).min(cap))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ExecutableToolContext {
        let background = std::sync::Arc::new(crate::background::BackgroundManager::new());
        ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::env::current_dir().unwrap(),
            allowed_paths: std::collections::HashSet::new(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            user_interaction: None,
            search_url: None,
            plan_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: crate::permissions::PermissionMode::Default,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            background: background.clone(),
            goal_manager: std::sync::Arc::new(std::sync::Mutex::new(
                crate::goal::GoalManager::new(),
            )),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            cron: std::sync::Arc::new(crate::cron::CronManager::new()),
            ..Default::default()
        }
    }

    /// A command killed for overrunning its timeout takes what it started
    /// along; so does a call dropped while its command runs (a cancelled
    /// turn). What a finished command left running on purpose stays.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_stopped_takes_what_it_started_and_a_finished_one_does_not() {
        use crate::process_group::testing::{
            can_see_processes, gone, kill_process, pid_in, running,
        };
        if !can_see_processes() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let starting = |name: &str| {
            let file = dir.path().join(name);
            (
                format!("sleep 300 & echo $! > {}; wait", file.display()),
                file,
            )
        };

        let (command, file) = starting("timed-out");
        let result = BashTool::new()
            .execute(
                json!({"command": command, "timeout": 1, "auto_background_on_timeout": false}),
                &ctx(),
                "t",
            )
            .await;
        assert!(
            result.text_content().contains("timed out"),
            "{}",
            result.text_content()
        );
        assert!(
            gone(pid_in(&file).await).await,
            "the sleep outlived its timeout"
        );

        let (command, file) = starting("dropped");
        let context = ctx();
        let tool = BashTool::new();
        let call = tool.execute(json!({"command": command}), &context, "t");
        let grandchild = tokio::select! {
            _ = call => panic!("the command finished"),
            pid = pid_in(&file) => pid,
        };
        assert!(
            gone(grandchild).await,
            "the sleep outlived its dropped call"
        );

        let file = dir.path().join("left");
        let result = BashTool::new()
            .execute(
                json!({"command": format!("sleep 300 > /dev/null 2>&1 & echo $! > {}", file.display())}),
                &ctx(),
                "t",
            )
            .await;
        assert!(!result.is_error, "{}", result.text_content());
        let server = pid_in(&file).await;
        assert!(
            running(server),
            "a finished command's background process stays"
        );
        kill_process(server);
    }

    #[tokio::test]
    async fn test_bash_echo() {
        let tool = BashTool::new();
        let result = tool
            .execute(json!({"command": "echo hello"}), &ctx(), "test-id")
            .await;
        assert!(!result.is_error);
        assert!(result.text_content().contains("hello"));
    }

    #[tokio::test]
    async fn test_bash_env() {
        let tool = BashTool::new();
        let result = tool
            .execute(
                json!({"command": "echo $MY_VAR", "env": {"MY_VAR": "42"}}),
                &ctx(),
                "test-id",
            )
            .await;
        assert!(!result.is_error);
        assert!(result.text_content().contains("42"));
    }

    #[tokio::test]
    async fn test_bash_stdin() {
        let tool = BashTool::new();
        let result = tool
            .execute(
                json!({"command": "cat", "stdin": "hello stdin"}),
                &ctx(),
                "test-id",
            )
            .await;
        assert!(!result.is_error);
        assert!(result.text_content().contains("hello stdin"));
    }

    /// The working directory may lie outside the allowed paths: the command
    /// could `cd` there anyway.
    #[tokio::test]
    async fn a_cwd_outside_the_workspace_is_allowed() {
        let outside = tempfile::tempdir().unwrap();
        let result = BashTool::new()
            .execute(
                json!({"command": "pwd", "cwd": outside.path()}),
                &ctx(),
                "test-id",
            )
            .await;
        assert!(!result.is_error, "{}", result.text_content());
        // The folder's own name: `pwd` spells the path as the shell sees it,
        // which on Windows is Git Bash's `/tmp/…`.
        let name = outside.path().file_name().unwrap().to_str().unwrap();
        assert!(
            result.text_content().contains(name),
            "{}",
            result.text_content()
        );
    }

    #[tokio::test]
    async fn test_bash_timeout_opt_out_kills() {
        let tool = BashTool::new();
        let result = tool
            .execute(
                json!({"command": "sleep 5", "timeout": 1, "auto_background_on_timeout": false}),
                &ctx(),
                "test-id",
            )
            .await;
        assert!(result.is_error);
        assert!(result.text_content().contains("timed out"));
    }

    /// By default a foreground command that exceeds its timeout is moved to the
    /// background (not killed) and keeps running under a fresh deadline.
    #[tokio::test]
    async fn test_bash_timeout_auto_backgrounds() {
        let c = ctx();
        let tool = BashTool::new();
        let result = tool
            .execute(
                json!({"command": "echo starting && sleep 30", "timeout": 1}),
                &c,
                "test-id",
            )
            .await;
        assert!(!result.is_error, "auto-background must not be an error");
        let text = result.text_content();
        assert!(text.contains("auto-backgrounded"), "{text}");
        let task_id = text
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("a task id");

        // The adopted process is still running, and its foreground output was
        // carried over.
        assert_eq!(
            c.background.status(task_id),
            Some(crate::background::TaskStatus::Running)
        );
        for _ in 0..50 {
            if c.background.stdout_contains(task_id, "starting") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(c.background.stdout_contains(task_id, "starting"));

        // Clean up the still-running sleep.
        c.background.stop(task_id);
    }

    #[tokio::test]
    async fn test_bash_progress_uses_tool_call_id() {
        let tool = BashTool::new();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let sender = std::sync::Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        }) as crate::agent_loop::types::ToolEventSender;
        let mut ctx = ctx();
        ctx.tool_event_sender = Some(sender);

        let _ = tool
            .execute(json!({"command": "echo hello"}), &ctx, "my-bash-id")
            .await;

        let ids: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .map(|e: &crate::agent_loop::types::ToolEvent| e.id().to_string())
            .collect();
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| id == "my-bash-id"));
    }

    #[tokio::test]
    async fn test_bash_run_in_background_requires_description() {
        let tool = BashTool::new();
        let result = tool
            .execute(
                json!({"command": "echo hello", "run_in_background": true}),
                &ctx(),
                "test-id",
            )
            .await;
        assert!(result.is_error);
        assert!(result.text_content().contains("description is required"));
    }

    #[tokio::test]
    async fn test_bash_run_in_background_returns_task_id() {
        let tool = BashTool::new();
        let result = tool
            .execute(
                json!({
                    "command": "echo hello",
                    "run_in_background": true,
                    "description": "echo task"
                }),
                &ctx(),
                "test-id",
            )
            .await;
        assert!(!result.is_error);
        assert!(result.text_content().contains("task_id: task-"));
    }

    #[tokio::test]
    async fn test_bash_background_timeout_stops_task() {
        let c = ctx();
        let tool = BashTool::new();
        let result = tool
            .execute(
                json!({
                    "command": "sleep 30",
                    "run_in_background": true,
                    "description": "sleeper",
                    "timeout": 1
                }),
                &c,
                "test-id",
            )
            .await;
        assert!(!result.is_error);
        let text = result.text_content();
        let task_id = text
            .lines()
            .find(|l| l.starts_with("task_id:"))
            .unwrap()
            .strip_prefix("task_id: ")
            .unwrap();

        for _ in 0..50 {
            if c.background
                .output(task_id, 0, None)
                .await
                .map(|o| o.contains("timed out"))
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let output = c.background.output(task_id, 0, None).await.unwrap();
        assert!(output.contains("timed out"));
    }

    #[test]
    fn test_normalize_timeout_caps_and_disable() {
        assert_eq!(normalize_timeout(Some(10), false, false), Some(10));
        assert_eq!(normalize_timeout(None, false, false), Some(60));
        assert_eq!(normalize_timeout(Some(600), false, false), Some(300));
        assert_eq!(normalize_timeout(None, true, false), Some(600));
        assert_eq!(normalize_timeout(Some(100_000), true, false), Some(86_400));
        assert_eq!(normalize_timeout(Some(100_000), true, true), None);
    }
}
