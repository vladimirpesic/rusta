use std::io::Write;
use std::sync::{Arc, Mutex};

use agent_core::agent_loop::llm::{CompactionNotice, CompactionTrigger, LlmCallbacks, RetryNotice};
use agent_core::agent_loop::types::{
    StopReason, SubagentToolEventSender, ToolEvent, ToolEventSender, ToolStatus,
};

use crate::terminal_sanitize::TerminalStreams;

use lm_bridge::message::{ContentPart, ToolCall};
use lm_bridge::usage::TokenUsage;
use serde::Serialize;

/// Build callbacks for headless mode that print assistant text and tool-call
/// deltas to stdout as they arrive.
pub fn headless_callbacks() -> (LlmCallbacks, ToolEventSender) {
    let printer = Arc::new(Mutex::new(HeadlessPrinter::new()));

    let text_printer = printer.clone();
    let think_printer = printer.clone();
    let tool_printer = printer.clone();
    let retry_printer = printer.clone();
    let compaction_printer = printer.clone();

    let callbacks = LlmCallbacks {
        on_text_delta: Some(Arc::new(move |delta: &str| {
            text_printer.lock().unwrap().print_text_delta(delta);
        })),
        on_think_delta: Some(Arc::new(move |delta: &str| {
            think_printer.lock().unwrap().print_think_delta(delta);
        })),
        on_tool_call_delta: Some(Arc::new(move |tc: &ToolCall| {
            tool_printer.lock().unwrap().print_tool_call_delta(tc);
        })),
        on_retry: Some(Arc::new(move |notice: &RetryNotice| {
            retry_printer.lock().unwrap().print_retry(notice);
        })),
        on_compaction: Some(Arc::new(move |notice: &CompactionNotice| {
            compaction_printer.lock().unwrap().print_compaction(notice);
        })),
    };

    let event_printer = printer.clone();
    let tool_event_sender: ToolEventSender = Arc::new(move |event: ToolEvent| {
        event_printer.lock().unwrap().print_tool_event(event);
    });

    (callbacks, tool_event_sender)
}

/// Build callbacks for the REPL that prepend a `rusta:` label to assistant
/// output.
pub fn repl_callbacks() -> (LlmCallbacks, ToolEventSender) {
    let printer = Arc::new(Mutex::new(ReplPrinter::new()));

    let text_printer = printer.clone();
    let think_printer = printer.clone();
    let tool_printer = printer.clone();
    let retry_printer = printer.clone();
    let compaction_printer = printer.clone();

    let callbacks = LlmCallbacks {
        on_text_delta: Some(Arc::new(move |delta: &str| {
            text_printer.lock().unwrap().print_text_delta(delta);
        })),
        on_think_delta: Some(Arc::new(move |delta: &str| {
            think_printer.lock().unwrap().print_think_delta(delta);
        })),
        on_tool_call_delta: Some(Arc::new(move |tc: &ToolCall| {
            tool_printer.lock().unwrap().print_tool_call_delta(tc);
        })),
        on_retry: Some(Arc::new(move |notice: &RetryNotice| {
            retry_printer.lock().unwrap().print_retry(notice);
        })),
        on_compaction: Some(Arc::new(move |notice: &CompactionNotice| {
            compaction_printer.lock().unwrap().print_compaction(notice);
        })),
    };

    let event_printer = printer.clone();
    let tool_event_sender: ToolEventSender = Arc::new(move |event: ToolEvent| {
        event_printer.lock().unwrap().print_tool_event(event);
    });

    (callbacks, tool_event_sender)
}

/// Build callbacks that emit structured NDJSON lines for `--stream-json`.
pub fn json_callbacks() -> (LlmCallbacks, ToolEventSender) {
    let printer = Arc::new(Mutex::new(JsonPrinter::new()));

    let text_printer = printer.clone();
    let think_printer = printer.clone();
    let tool_printer = printer.clone();
    let retry_printer = printer.clone();
    let compaction_printer = printer.clone();

    let callbacks = LlmCallbacks {
        on_text_delta: Some(Arc::new(move |delta: &str| {
            text_printer
                .lock()
                .unwrap()
                .emit(JsonEvent::AssistantDelta {
                    delta: delta.to_string(),
                });
        })),
        on_think_delta: Some(Arc::new(move |delta: &str| {
            think_printer
                .lock()
                .unwrap()
                .emit(JsonEvent::ThinkingDelta {
                    delta: delta.to_string(),
                });
        })),
        on_tool_call_delta: Some(Arc::new(move |tc: &ToolCall| {
            tool_printer.lock().unwrap().emit(JsonEvent::ToolCallDelta {
                tool_call: tc.clone(),
            });
        })),
        on_retry: Some(Arc::new(move |notice: &RetryNotice| {
            retry_printer.lock().unwrap().emit(JsonEvent::Retry {
                attempt: notice.attempt,
                max_attempts: notice.max_attempts,
                delay_ms: u64::try_from(notice.delay.as_millis()).unwrap_or(u64::MAX),
                error: notice.error.clone(),
            });
        })),
        on_compaction: Some(Arc::new(move |notice: &CompactionNotice| {
            compaction_printer
                .lock()
                .unwrap()
                .emit(JsonEvent::compaction(notice));
        })),
    };

    let event_printer = printer.clone();
    let tool_event_sender: ToolEventSender = Arc::new(move |event: ToolEvent| {
        emit_tool_event(&mut event_printer.lock().unwrap(), event, None);
    });

    (callbacks, tool_event_sender)
}

/// Where `--stream-json` puts sub-agents' tool calls and results: the same
/// `tool_call` and `tool_result` events, each with the sub-agent's
/// `agent_id`.
pub fn json_subagent_tool_events() -> SubagentToolEventSender {
    let printer = Mutex::new(JsonPrinter::new());
    Arc::new(move |agent_id: &str, event: ToolEvent| {
        emit_tool_event(&mut printer.lock().unwrap(), event, Some(agent_id));
    })
}

/// A tool event as `--stream-json` prints it; progress is not printed.
fn emit_tool_event(printer: &mut JsonPrinter, event: ToolEvent, agent_id: Option<&str>) {
    let agent_id = agent_id.map(str::to_string);
    match event {
        ToolEvent::Call {
            id,
            name,
            args,
            status,
        } => {
            let arguments = serde_json::from_str(&args).unwrap_or(serde_json::Value::String(args));
            printer.emit(JsonEvent::ToolCall {
                id,
                name,
                arguments,
                status,
                agent_id,
            });
        }
        ToolEvent::Result { id, result, status } => {
            printer.emit(JsonEvent::ToolResult {
                id,
                status,
                output: result.output,
                is_error: result.is_error,
                agent_id,
            });
        }
        ToolEvent::Progress { .. } => {}
    }
}

/// A tool-event sender that shows nothing: what the REPL leaves on the agent
/// between turns, so the finished turn's printer can end its line.
pub fn silent_tool_events() -> ToolEventSender {
    Arc::new(|_: ToolEvent| {})
}

/// Print a structured usage event.
pub fn emit_usage(usage: &TokenUsage) {
    let mut printer = JsonPrinter::new();
    printer.emit(JsonEvent::Usage { usage: *usage });
}

/// The answers an `approval_request` event accepts, one per line on stdin:
/// allow once, allow for the session, deny once, deny for the session, and
/// cancel the turn.
pub const APPROVAL_ANSWERS: [&str; 5] = ["y", "a", "n", "s", "c"];

/// Ask, as an `approval_request` event, whether a tool call may run — one of
/// the session's own, or a sub-agent's (`agent_id`). The answer is read from
/// stdin, one of [`APPROVAL_ANSWERS`].
pub fn emit_approval_request(
    tool: &str,
    arguments: &serde_json::Value,
    reason: Option<String>,
    agent_id: Option<String>,
) {
    JsonPrinter::new().emit(JsonEvent::ApprovalRequest {
        tool: tool.to_string(),
        arguments: arguments.clone(),
        reason,
        agent_id,
        answers: APPROVAL_ANSWERS,
    });
}

/// Ask the user a question, as a `question` event. The answer is read from
/// stdin, one line.
pub fn emit_question(question: &str) {
    JsonPrinter::new().emit(JsonEvent::Question {
        question: question.to_string(),
    });
}

/// The `error` line printed, on stderr, when a `--stream-json` turn fails.
#[must_use]
pub fn error_line(message: &str) -> String {
    serde_json::json!({ "type": "error", "message": message }).to_string()
}

/// What a stopped turn tells the user: why it stopped, and that the reply
/// before this line is the model's handoff.
#[must_use]
pub fn stop_notice(reason: StopReason) -> String {
    format!(
        "[stopped] {} — the reply above is the model's handoff: what was done, what is \
         blocked, and the next step.",
        reason.describe()
    )
}

/// Report a stopped turn: a `stop` event under `--stream-json`, the
/// [`stop_notice`] on stderr otherwise.
pub fn report_stop(reason: StopReason, stream_json: bool) {
    if stream_json {
        let mut printer = JsonPrinter::new();
        printer.emit(JsonEvent::Stop {
            reason: reason.as_str(),
            description: reason.describe(),
        });
    } else {
        eprintln!("{}", stop_notice(reason));
    }
}

/// Report a failed turn: an `error` event on stderr under `--stream-json`, an
/// `Error:` line on stderr otherwise.
pub fn report_turn_error(error: &dyn std::fmt::Display, stream_json: bool) {
    if stream_json {
        eprintln!("{}", error_line(&error.to_string()));
    } else {
        eprintln!(
            "{}",
            crate::terminal_sanitize::for_stderr(&format!("Error: {error}"))
        );
    }
}

/// Report that a headless run stopped waiting for background work: a `stop`
/// event under `--stream-json`, a line on stderr otherwise.
pub fn report_headless_stop(reason: &'static str, description: &'static str, stream_json: bool) {
    if stream_json {
        let mut printer = JsonPrinter::new();
        printer.emit(JsonEvent::Stop {
            reason,
            description,
        });
    } else {
        eprintln!("[headless] {description}");
    }
}

/// Print a structured response-id event.
pub fn emit_response_id(response_id: Option<&str>) {
    if let Some(id) = response_id {
        let mut printer = JsonPrinter::new();
        printer.emit(JsonEvent::ResponseId {
            response_id: id.to_string(),
        });
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JsonEvent {
    AssistantDelta {
        delta: String,
    },
    ThinkingDelta {
        delta: String,
    },
    /// The model starts a tool call: its id and name, as streamed so far.
    ToolCallDelta {
        tool_call: ToolCall,
    },
    /// A tool call with its complete arguments, once: `pending` as it runs,
    /// `failed` when it was refused before it could, or the shared result's
    /// status for a repeat of a call in the same step. Its `tool_result`
    /// follows.
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
        status: ToolStatus,
        /// The sub-agent whose call it is; absent for the session's own.
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    ToolResult {
        id: String,
        status: ToolStatus,
        output: Vec<ContentPart>,
        is_error: bool,
        /// The sub-agent whose call it is; absent for the session's own.
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    Usage {
        usage: TokenUsage,
    },
    ResponseId {
        response_id: String,
    },
    /// A tool call waits for approval: answer on stdin with one of `answers`.
    ApprovalRequest {
        tool: String,
        arguments: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// The sub-agent asking; absent for the session's own agent.
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        answers: [&'static str; 5],
    },
    /// The model asks the user a question: answer on stdin, one line.
    Question {
        question: String,
    },
    /// The turn was stopped before the model finished on its own; the last
    /// assistant text is its handoff.
    Stop {
        reason: &'static str,
        description: &'static str,
    },
    /// A model call failed and is being retried. Every delta since the last
    /// `retry` (or the start of the step) belongs to the discarded attempt.
    Retry {
        attempt: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_attempts: Option<u32>,
        delay_ms: u64,
        error: String,
    },
    /// Context compaction: `started`, then `summarized`, `evicted` or
    /// `unchanged`. Token counts are estimates.
    Compaction {
        phase: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens_before: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens_after: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        removed: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl JsonEvent {
    fn compaction(notice: &CompactionNotice) -> Self {
        match notice {
            CompactionNotice::Started { trigger, tokens } => Self::Compaction {
                phase: "started",
                trigger: Some(match trigger {
                    CompactionTrigger::Proactive => "proactive",
                    CompactionTrigger::Overflow => "overflow",
                    CompactionTrigger::Manual => "manual",
                }),
                tokens_before: Some(*tokens),
                tokens_after: None,
                removed: None,
                reason: None,
            },
            CompactionNotice::Summarized {
                tokens_before,
                tokens_after,
            } => Self::Compaction {
                phase: "summarized",
                trigger: None,
                tokens_before: Some(*tokens_before),
                tokens_after: Some(*tokens_after),
                removed: None,
                reason: None,
            },
            CompactionNotice::Evicted {
                removed,
                tokens_before,
                tokens_after,
                reason,
            } => Self::Compaction {
                phase: "evicted",
                trigger: None,
                tokens_before: Some(*tokens_before),
                tokens_after: Some(*tokens_after),
                removed: Some(*removed),
                reason: Some(reason.clone()),
            },
            CompactionNotice::Unchanged { reason } => Self::Compaction {
                phase: "unchanged",
                trigger: None,
                tokens_before: None,
                tokens_after: None,
                removed: None,
                reason: Some(reason.clone()),
            },
        }
    }
}

struct JsonPrinter;

impl JsonPrinter {
    fn new() -> Self {
        Self
    }

    fn emit(&mut self, event: JsonEvent) {
        if let Ok(line) = serde_json::to_string(&event) {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{line}");
            let _ = stdout.flush();
        }
    }
}

/// Where a text printer writes: this process's stdout and stderr, or, in
/// tests, buffers.
trait Sink: Send {
    /// Write `text` to stdout, flushed.
    fn out(&mut self, text: &str);
    /// Write `text` to stderr, flushed.
    fn err(&mut self, text: &str);
}

/// This process's stdout and stderr.
struct Stdio;

impl Sink for Stdio {
    fn out(&mut self, text: &str) {
        if !text.is_empty() {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    }

    fn err(&mut self, text: &str) {
        if !text.is_empty() {
            eprint!("{text}");
            let _ = std::io::stderr().flush();
        }
    }
}

/// The line a streamed tool call is announced with.
fn tool_call_line(tc: &ToolCall) -> String {
    let args = tc.arguments.as_deref().unwrap_or("...");
    format!("[tool-call] {}({args})", tc.name)
}

/// Print one tool event as the text printers do: a `[tool]` line for the
/// call and for each status change, and the call's output as it streams.
/// A call's result ends its output streams first.
fn print_tool_event(streams: &mut TerminalStreams, sink: &mut dyn Sink, event: ToolEvent) {
    let status_line = |streams: &TerminalStreams, id: &str, status: ToolStatus| {
        format!(
            "{}\n",
            streams.stdout_line(&format!("[tool] [{id}] {status:?}"))
        )
    };
    match event {
        ToolEvent::Call {
            id,
            name,
            args,
            status,
        } => {
            let line = streams.stdout_line(&format!("[tool] {name}({args}) [{id}] {status:?}"));
            sink.out(&format!("{line}\n"));
        }
        ToolEvent::Progress {
            id,
            stdout_chunk: None,
            stderr_chunk: None,
            status,
        } => sink.out(&status_line(streams, &id, status)),
        ToolEvent::Progress {
            id,
            stdout_chunk,
            stderr_chunk,
            ..
        } => {
            if let Some(out) = stdout_chunk {
                sink.out(&streams.tool_stdout(&id, &out));
            }
            if let Some(err) = stderr_chunk {
                sink.err(&streams.tool_stderr(&id, &err));
            }
        }
        ToolEvent::Result { id, status, .. } => {
            let (out, err) = streams.finish_tool(&id);
            sink.out(&out);
            sink.err(&err);
            sink.out(&status_line(streams, &id, status));
        }
    }
}

struct HeadlessPrinter {
    /// Inside a `[thinking]` run: the answer that follows starts on its own
    /// line, and thinking after other output gets a label of its own.
    in_think: bool,
    text_needs_newline: bool,
    /// One sanitizer per output stream; piped output stays byte-exact.
    streams: TerminalStreams,
    sink: Box<dyn Sink>,
}

impl HeadlessPrinter {
    fn new() -> Self {
        Self::with_sink(TerminalStreams::for_stdio(), Box::new(Stdio))
    }

    fn with_sink(streams: TerminalStreams, sink: Box<dyn Sink>) -> Self {
        Self {
            in_think: false,
            text_needs_newline: false,
            streams,
            sink,
        }
    }

    /// End the model's current run of text or thinking, printing what its
    /// sanitizers still held.
    fn end_model_run(&mut self) {
        let tail = self.streams.finish_model();
        if !tail.is_empty() {
            self.sink.out(&tail);
            self.text_needs_newline = !tail.ends_with('\n');
        }
    }

    fn print_text_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if self.in_think {
            self.end_model_run();
            self.sink.out("\n");
            self.in_think = false;
        }
        let delta = self.streams.text(delta);
        if !delta.is_empty() {
            self.sink.out(&delta);
            self.text_needs_newline = !delta.ends_with('\n');
        }
    }

    fn print_think_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if !self.in_think {
            self.end_model_run();
            self.sink.out("\n[thinking] ");
            self.in_think = true;
            self.text_needs_newline = false;
        }
        self.sink.out(&self.streams.think(delta));
    }

    fn print_tool_call_delta(&mut self, tc: &ToolCall) {
        self.end_model_run();
        let line = self.streams.stdout_line(&tool_call_line(tc));
        self.sink.out(&format!("\n{line}\n"));
        self.in_think = false;
        self.text_needs_newline = false;
    }

    /// Mark everything the failed attempt printed as superseded. The notice
    /// goes to stderr so stdout stays the model's output; stdout only gets the
    /// line break that starts the retried output on a fresh line.
    fn print_retry(&mut self, notice: &RetryNotice) {
        self.end_model_run();
        if self.text_needs_newline || self.in_think {
            self.sink.out("\n");
        }
        let line = self
            .streams
            .stderr_line(&format!("[retry] {}", notice.describe()));
        self.sink.err(&format!("{line}\n"));
        self.in_think = false;
        self.text_needs_newline = false;
    }

    /// Report compaction on stderr, keeping stdout the model's output.
    fn print_compaction(&mut self, notice: &CompactionNotice) {
        self.end_model_run();
        if self.text_needs_newline || self.in_think {
            self.sink.out("\n");
            self.text_needs_newline = false;
            self.in_think = false;
        }
        let line = self
            .streams
            .stderr_line(&format!("[compaction] {}", notice.describe()));
        self.sink.err(&format!("{line}\n"));
    }

    fn print_tool_event(&mut self, event: ToolEvent) {
        print_tool_event(&mut self.streams, self.sink.as_mut(), event);
    }
}

impl Drop for HeadlessPrinter {
    fn drop(&mut self) {
        // What the streams still held ends the output.
        self.end_model_run();
        let (out, err) = self.streams.finish();
        self.sink.out(&out);
        self.sink.err(&err);
        if self.text_needs_newline {
            self.sink.out("\n");
        }
    }
}

struct ReplPrinter {
    label_printed: bool,
    think_started: bool,
    text_needs_newline: bool,
    /// One sanitizer per output stream; piped output stays byte-exact.
    streams: TerminalStreams,
    sink: Box<dyn Sink>,
}

impl ReplPrinter {
    fn new() -> Self {
        Self::with_sink(TerminalStreams::for_stdio(), Box::new(Stdio))
    }

    fn with_sink(streams: TerminalStreams, sink: Box<dyn Sink>) -> Self {
        Self {
            label_printed: false,
            think_started: false,
            text_needs_newline: false,
            streams,
            sink,
        }
    }

    /// End the model's current run of text or thinking, printing what its
    /// sanitizers still held.
    fn end_model_run(&mut self) {
        let tail = self.streams.finish_model();
        if !tail.is_empty() {
            self.sink.out(&tail);
            self.text_needs_newline = !tail.ends_with('\n');
        }
    }

    fn print_text_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if !self.label_printed {
            self.end_model_run();
            self.sink.out("\nrusta: ");
            self.label_printed = true;
        }
        // Model output can carry an ESC that came from a fetched page.
        let delta = self.streams.text(delta);
        if !delta.is_empty() {
            self.sink.out(&delta);
            self.text_needs_newline = !delta.ends_with('\n');
        }
    }

    fn print_think_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if !self.think_started {
            self.end_model_run();
            self.sink.out("\n[thinking] ");
            self.think_started = true;
        }
        self.sink.out(&self.streams.think(delta));
    }

    fn print_tool_call_delta(&mut self, tc: &ToolCall) {
        self.end_model_run();
        let line = self.streams.stdout_line(&tool_call_line(tc));
        self.sink.out(&format!("\n{line}\n"));
        self.text_needs_newline = false;
    }

    /// Mark everything the failed attempt printed as superseded, and start the
    /// retried output under a fresh label.
    fn print_retry(&mut self, notice: &RetryNotice) {
        self.end_model_run();
        let line = self
            .streams
            .stdout_line(&format!("[retry] {}", notice.describe()));
        self.sink.out(&format!("\n{line}\n"));
        self.label_printed = false;
        self.think_started = false;
        self.text_needs_newline = false;
    }

    /// Report compaction between steps; the next output starts a fresh label.
    fn print_compaction(&mut self, notice: &CompactionNotice) {
        self.end_model_run();
        let line = self
            .streams
            .stdout_line(&format!("[compaction] {}", notice.describe()));
        self.sink.out(&format!("\n{line}\n"));
        self.label_printed = false;
        self.think_started = false;
        self.text_needs_newline = false;
    }

    fn print_tool_event(&mut self, event: ToolEvent) {
        print_tool_event(&mut self.streams, self.sink.as_mut(), event);
    }
}

impl Drop for ReplPrinter {
    fn drop(&mut self) {
        // What the streams still held ends the output.
        self.end_model_run();
        let (out, err) = self.streams.finish();
        self.sink.out(&out);
        self.sink.err(&err);
        if self.text_needs_newline {
            self.sink.out("\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a printer wrote to stdout and to stderr.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<(String, String)>>);

    impl Captured {
        fn stdout(&self) -> String {
            self.0.lock().unwrap().0.clone()
        }

        fn stderr(&self) -> String {
            self.0.lock().unwrap().1.clone()
        }
    }

    impl Sink for Captured {
        fn out(&mut self, text: &str) {
            self.0.lock().unwrap().0.push_str(text);
        }

        fn err(&mut self, text: &str) {
            self.0.lock().unwrap().1.push_str(text);
        }
    }

    fn headless(terminal: bool) -> (HeadlessPrinter, Captured) {
        let captured = Captured::default();
        let streams = TerminalStreams::new(terminal, terminal);
        (
            HeadlessPrinter::with_sink(streams, Box::new(captured.clone())),
            captured,
        )
    }

    fn repl(terminal: bool) -> (ReplPrinter, Captured) {
        let captured = Captured::default();
        let streams = TerminalStreams::new(terminal, terminal);
        (
            ReplPrinter::with_sink(streams, Box::new(captured.clone())),
            captured,
        )
    }

    fn progress(id: &str, stdout: &str) -> ToolEvent {
        ToolEvent::Progress {
            id: id.to_string(),
            stdout_chunk: Some(stdout.to_string()),
            stderr_chunk: None,
            status: ToolStatus::Running,
        }
    }

    fn finished(id: &str) -> ToolEvent {
        ToolEvent::Result {
            id: id.to_string(),
            result: agent_core::agent_loop::types::ExecutableToolResult::text("done"),
            status: ToolStatus::Completed,
        }
    }

    /// A tool call whose streamed arguments carry a clipboard write.
    fn hostile_call() -> ToolCall {
        ToolCall::new("tc-1", "bash").with_arguments("{\"x\":\"\u{1B}]52;c;aGk=\u{7}\"}")
    }

    fn assert_clean(output: &str) {
        assert!(
            !output.contains('\u{1B}') && !output.contains('\u{7}'),
            "{output:?}"
        );
    }

    /// On a terminal, thinking, the answer and `[tool-call]` lines all reach
    /// it sanitized, in the headless printer as in the REPL's.
    #[test]
    fn a_terminal_gets_thinking_answers_and_tool_call_lines_sanitized() {
        let (mut printer, captured) = headless(true);
        printer.print_think_delta("plan \u{1B}[2J");
        printer.print_text_delta("\u{1B}[31manswer\u{1B}[0m");
        printer.print_tool_call_delta(&hostile_call());
        drop(printer);
        let out = captured.stdout();
        assert_clean(&out);
        assert!(out.contains("[thinking] plan \nanswer"), "{out:?}");
        assert!(out.contains("[tool-call] bash({\"x\":\"\"})"), "{out:?}");

        let (mut printer, captured) = repl(true);
        printer.print_think_delta("plan \u{1B}[2J");
        printer.print_text_delta("\u{1B}[31manswer\u{1B}[0m");
        printer.print_tool_call_delta(&hostile_call());
        drop(printer);
        let out = captured.stdout();
        assert_clean(&out);
        assert!(out.contains("[thinking] plan \nrusta: answer"), "{out:?}");
        assert!(out.contains("[tool-call] bash({\"x\":\"\"})"), "{out:?}");
    }

    /// A sequence one tool leaves open swallows neither another tool's output
    /// nor the answer; the tool's own result prints what it held.
    #[test]
    fn a_sequence_held_by_one_tool_never_swallows_other_output() {
        let (mut printer, captured) = repl(true);
        printer.print_tool_event(progress("a", "a-out \u{1B}]0;open"));
        printer.print_tool_event(progress("b", "b-out\n"));
        printer.print_tool_event(finished("b"));
        printer.print_text_delta("the answer");
        let out = captured.stdout();
        assert!(
            out.ends_with("a-out b-out\n[tool] [b] Completed\n\nrusta: the answer"),
            "{out:?}"
        );
        printer.print_tool_event(finished("a"));
        assert!(
            captured
                .stdout()
                .ends_with("the answer0;open[tool] [a] Completed\n"),
            "{:?}",
            captured.stdout()
        );
    }

    /// What a stream still held when the turn ends is printed, not lost.
    #[test]
    fn an_unfinished_sequence_is_printed_when_the_turn_ends() {
        let (mut printer, captured) = headless(true);
        printer.print_text_delta("end \u{1B}]0;unfinished");
        printer.print_tool_event(progress("t", "out \u{1B}]0;cut"));
        assert_eq!(captured.stdout(), "end out ");
        drop(printer);
        assert_eq!(captured.stdout(), "end out 0;unfinished0;cut\n");
    }

    /// Piped output is the model's and the tools' bytes, exactly.
    #[test]
    fn piped_output_is_byte_exact() {
        let raw = "\u{1B}[31mraw\r\n\u{1B}]0;open";
        let (mut printer, captured) = headless(false);
        printer.print_text_delta(raw);
        printer.print_tool_event(progress("t", raw));
        printer.print_tool_event(ToolEvent::Progress {
            id: "t".to_string(),
            stdout_chunk: None,
            stderr_chunk: Some(raw.to_string()),
            status: ToolStatus::Running,
        });
        drop(printer);
        assert_eq!(captured.stdout(), format!("{raw}{raw}\n"));
        assert_eq!(captured.stderr(), raw);
    }

    #[test]
    fn test_json_callbacks_produce_assistant_delta() {
        let (callbacks, _sender) = json_callbacks();
        let cb = callbacks.on_text_delta.unwrap();
        cb("hello");
        // stdout capture not trivial in test; ensure no panic.
    }

    /// Every front-end hears about retries, so none leaves a discarded partial
    /// answer looking like the real one.
    #[test]
    fn a_stop_is_reported_with_its_reason() {
        let event = serde_json::to_value(JsonEvent::Stop {
            reason: StopReason::MaxTurns.as_str(),
            description: StopReason::MaxTurns.describe(),
        })
        .unwrap();
        assert_eq!(event["type"], "stop");
        assert_eq!(event["reason"], "max_turns");
        assert!(
            stop_notice(StopReason::RepeatBreaker)
                .starts_with("[stopped] it kept repeating the same tool call")
        );
    }

    #[test]
    fn every_front_end_handles_retry_notices() {
        let notice = RetryNotice {
            attempt: 1,
            max_attempts: Some(10),
            delay: std::time::Duration::from_millis(500),
            error: "API connection error: the response stream was interrupted".into(),
        };
        for (callbacks, _) in [headless_callbacks(), repl_callbacks(), json_callbacks()] {
            let on_retry = callbacks.on_retry.expect("retry callback is wired");
            on_retry(&notice);
        }
    }

    /// Every front-end reports compaction, which costs a model call and
    /// changes the history under the model.
    #[test]
    fn every_front_end_handles_compaction_notices() {
        let notices = [
            CompactionNotice::Started {
                trigger: CompactionTrigger::Proactive,
                tokens: 120_000,
            },
            CompactionNotice::Summarized {
                tokens_before: 120_000,
                tokens_after: 9_000,
            },
        ];
        for (callbacks, _) in [headless_callbacks(), repl_callbacks(), json_callbacks()] {
            let on_compaction = callbacks
                .on_compaction
                .expect("compaction callback is wired");
            for notice in &notices {
                on_compaction(notice);
            }
        }
    }

    #[test]
    fn the_json_compaction_event_is_machine_readable() {
        let started = serde_json::to_value(JsonEvent::compaction(&CompactionNotice::Started {
            trigger: CompactionTrigger::Overflow,
            tokens: 10,
        }))
        .unwrap();
        assert_eq!(started["type"], "compaction");
        assert_eq!(started["phase"], "started");
        assert_eq!(started["trigger"], "overflow");
        assert!(started.get("tokens_after").is_none());
        let evicted = serde_json::to_value(JsonEvent::compaction(&CompactionNotice::Evicted {
            removed: 4,
            tokens_before: 10,
            tokens_after: 5,
            reason: "empty summary".into(),
        }))
        .unwrap();
        assert_eq!(evicted["phase"], "evicted");
        assert_eq!(evicted["removed"], 4);
        assert_eq!(evicted["reason"], "empty summary");
    }

    /// Asking for approval under `--stream-json` is an event like any other,
    /// naming the answers stdin takes, so the stream stays one JSON value per
    /// line — a bare `[approve] …` prompt left the next event on its line.
    #[test]
    fn the_approval_request_is_a_json_event_naming_its_answers() {
        let event = serde_json::to_value(JsonEvent::ApprovalRequest {
            tool: "bash".into(),
            arguments: serde_json::json!({"command": "rm -rf build"}),
            reason: Some("dangerous command: rm -rf".into()),
            agent_id: Some("sub-1".into()),
            answers: APPROVAL_ANSWERS,
        })
        .unwrap();
        assert_eq!(event["agent_id"], "sub-1");
        assert_eq!(event["type"], "approval_request");
        assert_eq!(event["tool"], "bash");
        assert_eq!(event["arguments"]["command"], "rm -rf build");
        assert_eq!(event["reason"], "dangerous command: rm -rf");
        assert_eq!(
            event["answers"],
            serde_json::json!(["y", "a", "n", "s", "c"])
        );

        let question = serde_json::to_value(JsonEvent::Question {
            question: "Which database?".into(),
        })
        .unwrap();
        assert_eq!(question["type"], "question");
        assert_eq!(question["question"], "Which database?");
    }

    /// Every answer an `approval_request` offers is one the hook understands.
    #[test]
    fn every_offered_answer_is_understood() {
        for answer in APPROVAL_ANSWERS {
            assert!(
                crate::approval::understands(answer),
                "{answer} must be a known answer"
            );
        }
    }

    /// An error carrying quotes or line breaks is still one valid JSON line.
    #[test]
    fn the_error_line_escapes_its_message() {
        let line = error_line("API error 400: {\"error\":\"bad\"}\nsecond line");
        assert!(!line.contains('\n'));
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["type"], "error");
        assert_eq!(
            event["message"],
            "API error 400: {\"error\":\"bad\"}\nsecond line"
        );
    }

    #[test]
    fn the_json_retry_event_is_machine_readable() {
        let event = JsonEvent::Retry {
            attempt: 2,
            max_attempts: None,
            delay_ms: 1500,
            error: "timeout".into(),
        };
        let line = serde_json::to_value(&event).unwrap();
        assert_eq!(line["type"], "retry");
        assert_eq!(line["attempt"], 2);
        assert_eq!(line["delay_ms"], 1500);
        assert!(line.get("max_attempts").is_none());
    }
}
