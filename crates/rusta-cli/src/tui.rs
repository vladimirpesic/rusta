use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::agent_loop::llm::{CompactionNotice, LlmCallbacks, RetryNotice};
use agent_core::agent_loop::types::{StepResult, ToolEvent, ToolEventSender, ToolStatus};
use agent_core::background::BackgroundManager;
use agent_core::cron::CronManager;
use agent_core::errors::AgentError;
use agent_core::steer::SteerChannel;
use agent_core::user_hooks::{UserHookEvent, UserHooks};
use agent_core::{
    ApprovalHook, ApprovalRequest, ApprovalResponse, Llm, PromptOrigin, UserInteraction,
};

use crate::commands::{self, Command};
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use lm_bridge::message::{Role, ToolCall};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Margin, Position},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::error;

/// Run the Ratatui-based chat interface.
pub async fn run(ctx: commands::CommandContext, shutdown: CancellationToken) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();

    let (approval_tx, approval_rx) = mpsc::unbounded_channel();
    let (user_tx, user_rx) = mpsc::unbounded_channel();

    {
        let mut agent = ctx.agent.lock().await;
        agent.set_approval_hook(Arc::new(TuiApprovalHook { tx: approval_tx }));
        agent.set_user_interaction(Arc::new(TuiUserInteraction { tx: user_tx }));
    }
    let handles = SessionHandles::of(&ctx).await;
    let mut app = App::new(ctx, shutdown, approval_rx, user_rx, handles);
    let result = app.run(&mut terminal).await;
    ratatui::restore();
    result
}

/// What the event loop reaches without the agent lock. The turn task holds
/// that lock for a whole turn, so while a turn runs the loop may only use
/// these handles: anything that waits for the lock stalls rendering, input
/// and approval prompts until the turn ends, and an approval the turn is
/// waiting on can then never be answered.
struct SessionHandles {
    /// The session agent's user-cancel flag: set before `prompt_cancel`
    /// fires so the interrupted turn is followed by the interruption
    /// reminder.
    user_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// The session agent's steering channel.
    steer: Arc<SteerChannel>,
    /// The session's user hooks, for `UserPromptQueued`.
    user_hooks: Option<Arc<UserHooks>>,
    /// The background tasks, whose completions ring the bell.
    background: Arc<BackgroundManager>,
    /// The session's scheduled prompts, ticked while idle.
    cron: Arc<CronManager>,
}

impl SessionHandles {
    /// Take the handles once, before the first turn holds the lock. Every one
    /// of them stays the same object for the session: `/new` clears the cron
    /// store in place rather than replacing it.
    async fn of(ctx: &commands::CommandContext) -> Self {
        let agent = ctx.agent.lock().await;
        Self {
            user_cancel: agent.user_cancel_flag(),
            steer: agent.steer_channel(),
            user_hooks: agent.tool_ctx().user_hooks.clone(),
            background: Arc::clone(&agent.tool_ctx().background),
            cron: Arc::clone(&agent.tool_ctx().cron),
        }
    }
}

enum ChatRole {
    User,
    Assistant,
    Error,
    System,
}

struct ChatMessage {
    role: ChatRole,
    text: String,
}

type ApprovalPayload = (ApprovalRequest, oneshot::Sender<ApprovalResponse>);
type UserQuestionPayload = (String, oneshot::Sender<Option<String>>);

struct TuiApprovalHook {
    tx: mpsc::UnboundedSender<ApprovalPayload>,
}

#[async_trait]
impl ApprovalHook for TuiApprovalHook {
    async fn request_approval(&self, request: ApprovalRequest) -> ApprovalResponse {
        let (resp_tx, resp_rx) = oneshot::channel();
        if self.tx.send((request, resp_tx)).is_err() {
            return ApprovalResponse::DenyOnce;
        }
        resp_rx.await.unwrap_or(ApprovalResponse::DenyOnce)
    }
}

struct TuiUserInteraction {
    tx: mpsc::UnboundedSender<UserQuestionPayload>,
}

#[async_trait]
impl UserInteraction for TuiUserInteraction {
    async fn ask(&self, question: &str) -> Option<String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        if self.tx.send((question.to_string(), resp_tx)).is_err() {
            return None;
        }
        resp_rx.await.unwrap_or(None)
    }
}

/// What the model's streaming callbacks hand the UI loop, in order.
///
/// Deltas and retry notices share one channel so a retry is always seen after
/// the partial output it supersedes and before the retried output.
enum StreamItem {
    Delta(String),
    Retry(String),
    /// A compaction started (`true`) or finished (`false`), described.
    Compaction(bool, String),
}

pub struct App {
    ctx: commands::CommandContext,
    shutdown: CancellationToken,
    prompt_cancel: CancellationToken,
    /// Everything the loop uses while a turn holds the agent lock. Esc and
    /// Ctrl+C interrupt the turn through its user-cancel flag; Ctrl-S (and
    /// the queue drained into a starting `wait_for`) steer through its
    /// channel.
    handles: SessionHandles,
    input: String,
    cursor: usize,
    messages: Vec<ChatMessage>,
    /// Input Enter-queued while a turn runs. The entries run in order after
    /// the turn; steerable ones join a running `wait_for` early.
    queue: PromptQueue,
    /// Serial number for `UserPromptQueued`'s `prompt_id` field.
    queued_count: u64,
    /// When the last idle Ctrl+C arrived: a second one within the window
    /// exits (Ctrl-C twice).
    exit_armed_at: Option<Instant>,
    list_state: ListState,
    /// A turn started and not yet picked up by the event loop. The loop takes
    /// the handle out to poll it, so it is `None` while the turn runs: ask
    /// [`Self::turn_active`] whether one is running.
    task: Option<tokio::task::JoinHandle<Result<StepResult, AgentError>>>,
    /// Whether a turn is running: from `start_turn` until the loop has seen
    /// the turn's task finish. Every handler that behaves differently during
    /// a turn (Enter queues, Ctrl-S steers, Esc and Ctrl-C interrupt) asks
    /// this, never `task`.
    turn_active: bool,
    stream_tx: tokio::sync::mpsc::UnboundedSender<StreamItem>,
    stream_rx: tokio::sync::mpsc::UnboundedReceiver<StreamItem>,
    tool_event_tx: tokio::sync::mpsc::UnboundedSender<ToolEvent>,
    tool_event_rx: tokio::sync::mpsc::UnboundedReceiver<ToolEvent>,
    approval_rx: mpsc::UnboundedReceiver<ApprovalPayload>,
    user_rx: mpsc::UnboundedReceiver<UserQuestionPayload>,
    pending_approval: Option<ApprovalPayload>,
    pending_question: Option<UserQuestionPayload>,
    status: String,
    should_quit: bool,
    last_background_completion_count: usize,
    /// The user's last prompt, put back in the input when the session's
    /// lock refuses it, so Enter sends it again.
    last_prompt: Option<String>,
}

/// How long after an idle Ctrl+C a second one exits (the "press again to
/// quit" window).
const EXIT_ARM_WINDOW: Duration = Duration::from_secs(2);

/// What a Ctrl+C does: interrupt the running turn, arm the exit prompt, or
/// exit. A steered or queued message is never involved — steering is not a
/// cancel and must not trip the interruption reminder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlCOutcome {
    Interrupt,
    ArmExit,
    Exit,
}

fn ctrl_c_outcome(task_running: bool, exit_armed: bool) -> CtrlCOutcome {
    if task_running {
        CtrlCOutcome::Interrupt
    } else if exit_armed {
        CtrlCOutcome::Exit
    } else {
        CtrlCOutcome::ArmExit
    }
}

/// What Esc does: interrupt the running turn, or clear the input when idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscOutcome {
    Interrupt,
    ClearInput,
}

fn esc_outcome(task_running: bool) -> EscOutcome {
    if task_running {
        EscOutcome::Interrupt
    } else {
        EscOutcome::ClearInput
    }
}

/// Input Enter-queued while a turn runs. Entries run in order after the
/// turn; the steerable ones can also join a running `wait_for` early.
#[derive(Debug, Default)]
struct PromptQueue {
    entries: Vec<String>,
}

/// Queued commands are not steered into a running wait; they stay queued
/// until the turn ends. Those are a shell-style line (`!…`) and a slash
/// command (`/…`).
fn is_steerable_input(text: &str) -> bool {
    !text.starts_with('/') && !text.starts_with('!')
}

impl PromptQueue {
    fn push(&mut self, text: String) {
        self.entries.push(text);
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove and return the first entry.
    fn pop(&mut self) -> Option<String> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.entries.remove(0))
        }
    }

    /// Remove and return every steerable entry, in order. Shell-style and
    /// slash-command entries stay queued for after the turn.
    fn drain_steerable(&mut self) -> Vec<String> {
        let mut steered = Vec::new();
        let mut kept = Vec::new();
        for entry in self.entries.drain(..) {
            if is_steerable_input(&entry) {
                steered.push(entry);
            } else {
                kept.push(entry);
            }
        }
        self.entries = kept;
        steered
    }
}

impl App {
    fn new(
        ctx: commands::CommandContext,
        shutdown: CancellationToken,
        approval_rx: mpsc::UnboundedReceiver<ApprovalPayload>,
        user_rx: mpsc::UnboundedReceiver<UserQuestionPayload>,
        handles: SessionHandles,
    ) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let model_name = ctx.llm.model_name().to_string();
        let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (tool_event_tx, tool_event_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            ctx,
            shutdown,
            prompt_cancel: CancellationToken::new(),
            handles,
            input: String::new(),
            cursor: 0,
            messages: vec![ChatMessage {
                role: ChatRole::System,
                text: "Welcome to rusta TUI. Type a prompt and press Enter. While a turn \
                       runs: Enter queues for after it, Ctrl-S steers into it, Esc or Ctrl-C \
                       interrupt it. Ctrl-C twice or Ctrl-D on an empty input exits."
                    .into(),
            }],
            queue: PromptQueue::default(),
            queued_count: 0,
            exit_armed_at: None,
            list_state,
            task: None,
            turn_active: false,
            stream_tx,
            stream_rx,
            tool_event_tx,
            tool_event_rx,
            approval_rx,
            user_rx,
            pending_approval: None,
            pending_question: None,
            status: format!("model: {}", model_name),
            should_quit: false,
            last_background_completion_count: 0,
            last_prompt: None,
        }
    }

    async fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let mut events = crossterm::event::EventStream::new();
        // Scheduled prompts are polled only while idle: a tick never fires
        // into a running turn, so a fire waits for the current turn to end.
        let mut cron_ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        // A turn leaves the ticker unpolled; the ticks it missed are dropped,
        // not replayed back to back once it ends.
        cron_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The interval's first tick is immediate; that poll belongs to the
        // initial state check, not to a 1-second boundary.
        cron_ticker.tick().await;
        while !self.should_quit {
            self.check_background_notifications();
            if self.shutdown.is_cancelled() {
                break;
            }

            terminal.draw(|frame| self.draw(frame))?;

            if let Some(task) = self.task.take() {
                let mut task = task;
                let task_fut = std::pin::Pin::new(&mut task);
                tokio::select! {
                    Some(Ok(event)) = events.next() => {
                        self.handle_event(event).await?;
                        self.task = Some(task);
                    }
                    Some(item) = self.stream_rx.recv() => {
                        self.handle_stream_item(item);
                        self.task = Some(task);
                    }
                    Some(event) = self.tool_event_rx.recv() => {
                        self.handle_tool_event(event);
                        self.task = Some(task);
                    }
                    Some(req) = self.approval_rx.recv() => {
                        self.handle_approval_request(req);
                        self.task = Some(task);
                    }
                    Some(req) = self.user_rx.recv() => {
                        self.handle_user_question(req);
                        self.task = Some(task);
                    }
                    result = task_fut => {
                        // The turn is over: what it left in the channels
                        // belongs to it, and the agent lock is free again.
                        self.turn_active = false;
                        while let Ok(item) = self.stream_rx.try_recv() {
                            self.handle_stream_item(item);
                        }
                        while let Ok(event) = self.tool_event_rx.try_recv() {
                            self.handle_tool_event(event);
                        }
                        match result {
                            Ok(step_result) => {
                                self.handle_result(step_result).await;
                            }
                            Err(e) => {
                                self.status = format!("task error: {e}");
                            }
                        }
                    }
                }
            } else {
                tokio::select! {
                    Some(Ok(event)) = events.next() => {
                        self.handle_event(event).await?;
                    }
                    Some(item) = self.stream_rx.recv() => {
                        self.handle_stream_item(item);
                    }
                    Some(event) = self.tool_event_rx.recv() => {
                        self.handle_tool_event(event);
                    }
                    Some(req) = self.approval_rx.recv() => {
                        self.handle_approval_request(req);
                    }
                    Some(req) = self.user_rx.recv() => {
                        self.handle_user_question(req);
                    }
                    _ = cron_ticker.tick() => {
                        self.deliver_cron_fires().await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Turn the cron fires that came due while the TUI was idle into a turn:
    /// the first envelope is the turn's prompt, and the others are steered
    /// into that same turn. Nothing here waits for the agent lock, which the
    /// turn takes as soon as it starts.
    async fn deliver_cron_fires(&mut self) {
        debug_assert!(
            !self.turn_active,
            "cron fires are delivered only while idle"
        );
        let fires = self.handles.cron.tick();
        if fires.is_empty() {
            return;
        }
        print!("\x07");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut fires = fires.into_iter();
        if let Some(first) = fires.next() {
            // The user may be typing: a scheduled prompt must not take their
            // half-written input with it.
            let input = std::mem::take(&mut self.input);
            let cursor = self.cursor;
            self.start_turn(first.envelope(), PromptOrigin::Cron).await;
            self.input = input;
            self.cursor = cursor;
            for fire in fires {
                self.handles.steer.steer(fire.envelope());
            }
        }
    }

    fn check_background_notifications(&mut self) {
        let current = self.handles.background.pending_completion_count();
        if current > self.last_background_completion_count {
            print!("\x07");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        self.last_background_completion_count = current;
    }

    async fn handle_event(&mut self, event: Event) -> anyhow::Result<()> {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            if key.code == KeyCode::Enter {
                self.submit().await;
            } else if key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && self.pending_approval.is_none()
                && self.pending_question.is_none()
            {
                self.steer_input().await;
            } else {
                self.on_key(key);
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) {
        if self.pending_approval.is_some() {
            self.on_key_approval(key);
            return;
        }
        if self.pending_question.is_some() {
            self.on_key_question(key);
            return;
        }

        // The first idle Ctrl+C arms the exit; any other key disarms it.
        if !(key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
            self.exit_armed_at = None;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match ctrl_c_outcome(
                    self.turn_active,
                    self.exit_armed_at
                        .is_some_and(|at| at.elapsed() <= EXIT_ARM_WINDOW),
                ) {
                    CtrlCOutcome::Interrupt => self.interrupt_turn(),
                    CtrlCOutcome::ArmExit => {
                        self.exit_armed_at = Some(Instant::now());
                        self.status = "Press Ctrl-C again to exit".into();
                    }
                    CtrlCOutcome::Exit => self.should_quit = true,
                }
            }
            KeyCode::Esc => match esc_outcome(self.turn_active) {
                EscOutcome::Interrupt => self.interrupt_turn(),
                EscOutcome::ClearInput => {
                    self.input.clear();
                    self.cursor = 0;
                }
            },
            // Ctrl-D on an empty input exits.
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.input.is_empty() {
                    self.should_quit = true;
                }
            }
            KeyCode::Enter => {} // handled asynchronously in handle_event
            KeyCode::Char(c) => self.insert_char(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_cursor_left(),
            KeyCode::Right => self.move_cursor_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Up => self.scroll_up(),
            KeyCode::Down => self.scroll_down(),
            _ => {}
        }
    }

    /// Interrupt the running turn as the user's own: the flag marks the cancel,
    /// so the turn is followed by the interruption reminder. Steering does not
    /// come through here and never trips it.
    fn interrupt_turn(&mut self) {
        self.handles
            .user_cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.prompt_cancel.cancel();
        self.status = "Interrupting...".into();
    }

    fn on_key_approval(&mut self, key: KeyEvent) {
        let Some((_, resp_tx)) = self.pending_approval.take() else {
            return;
        };
        let response = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => ApprovalResponse::AllowOnce,
            KeyCode::Char('a') | KeyCode::Char('A') => ApprovalResponse::AllowSession,
            KeyCode::Char('n') | KeyCode::Char('N') => ApprovalResponse::DenyOnce,
            KeyCode::Char('s') | KeyCode::Char('S') => ApprovalResponse::DenySession,
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => ApprovalResponse::Cancel,
            _ => {
                self.pending_approval = Some((
                    ApprovalRequest::new("", "", serde_json::Value::Null),
                    resp_tx,
                ));
                return;
            }
        };
        let _ = resp_tx.send(response);
        self.status = format!("model: {}", self.ctx.llm.model_name());
    }

    fn on_key_question(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let Some((_, resp_tx)) = self.pending_question.take() else {
                    return;
                };
                let answer = self.input.trim().to_string();
                self.input.clear();
                self.cursor = 0;
                let _ = resp_tx.send(if answer.is_empty() {
                    None
                } else {
                    Some(answer)
                });
                self.status = format!("model: {}", self.ctx.llm.model_name());
            }
            KeyCode::Char(c) => self.insert_char(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_cursor_left(),
            KeyCode::Right => self.move_cursor_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Esc => {
                let Some((_, resp_tx)) = self.pending_question.take() else {
                    return;
                };
                let _ = resp_tx.send(None);
                self.input.clear();
                self.cursor = 0;
                self.status = format!("model: {}", self.ctx.llm.model_name());
            }
            _ => {}
        }
    }

    fn handle_approval_request(&mut self, req: ApprovalPayload) {
        let request = &req.0;
        self.status = if request.rule == agent_core::permissions::PLAN_REVIEW_RULE {
            // The plan is the call's argument: shown whole, to be read.
            let plan = request
                .arguments
                .get("plan")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            self.messages.push(ChatMessage {
                role: ChatRole::System,
                text: format!("[plan review] The agent's plan:\n\n{}", plan.trim()),
            });
            self.scroll_to_bottom();
            "Approve this plan and leave plan mode? [y/n/c]".into()
        } else {
            match request.reason() {
                Some(reason) => format!("Approve {} — {reason}? [y/n/a/s/c]", request.tool_name),
                None => format!("Approve {}? [y/n/a/s/c]", request.tool_name),
            }
        };
        self.pending_approval = Some(req);
    }

    fn handle_user_question(&mut self, req: UserQuestionPayload) {
        let (question, _) = &req;
        self.messages.push(ChatMessage {
            role: ChatRole::System,
            text: format!("[ask_user] {question}"),
        });
        self.scroll_to_bottom();
        self.pending_question = Some(req);
        self.status = "Type your answer and press Enter".into();
        self.input.clear();
        self.cursor = 0;
    }

    fn insert_char(&mut self, c: char) {
        if self.cursor == self.input.len() {
            self.input.push(c);
        } else {
            self.input.insert(self.cursor, c);
        }
        self.cursor += c.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_char_boundary();
        self.input.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    fn delete(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        let next = self.next_char_boundary();
        self.input.replace_range(self.cursor..next, "");
    }

    fn move_cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.prev_char_boundary();
        }
    }

    fn move_cursor_right(&mut self) {
        if self.cursor < self.input.len() {
            self.cursor = self.next_char_boundary();
        }
    }

    fn prev_char_boundary(&self) -> usize {
        let bytes = self.input.as_bytes();
        let mut idx = self.cursor;
        while idx > 0 {
            idx -= 1;
            if bytes[idx] & 0b1100_0000 != 0b1000_0000 {
                break;
            }
        }
        idx
    }

    fn next_char_boundary(&self) -> usize {
        let bytes = self.input.as_bytes();
        let mut idx = self.cursor;
        while idx < bytes.len() {
            idx += 1;
            if idx < bytes.len() && bytes[idx] & 0b1100_0000 != 0b1000_0000 {
                break;
            }
            if idx == bytes.len() {
                break;
            }
        }
        idx
    }

    fn scroll_up(&mut self) {
        let current = self.list_state.selected().unwrap_or(0);
        if current > 0 {
            self.list_state.select(Some(current - 1));
        }
    }

    fn scroll_down(&mut self) {
        let current = self.list_state.selected().unwrap_or(0);
        let max = self.messages.len().saturating_sub(1);
        if current < max {
            self.list_state.select(Some(current + 1));
        }
    }

    async fn submit(&mut self) {
        if self.pending_question.is_some() {
            // Enter answers the question — the turn waits on it. (Enter is
            // routed here by handle_event, so on_key_question never sees it.)
            let Some((_, resp_tx)) = self.pending_question.take() else {
                return;
            };
            let answer = self.input.trim().to_string();
            self.input.clear();
            self.cursor = 0;
            let _ = resp_tx.send(if answer.is_empty() {
                None
            } else {
                Some(answer)
            });
            self.status = format!("model: {}", self.ctx.llm.model_name());
            return;
        }
        if self.pending_approval.is_some() {
            return; // approvals answer with y/n/a/s/c
        }
        let text = self.input.trim();
        if text.is_empty() {
            return;
        }

        let text = text.to_string();
        self.input.clear();
        self.cursor = 0;
        if self.turn_active {
            // A turn runs: Enter queues the input for after the turn (Ctrl-S
            // steers instead). Slash commands queue too and execute in order.
            self.queue_input(text);
        } else if text.starts_with('/') {
            self.run_command(&text).await;
        } else {
            self.send_prompt(text).await;
        }
    }

    /// Ctrl-S: inject the input into the running turn at the next step
    /// boundary. With nothing running there is no turn to steer, so
    /// the input is sent as a prompt instead. Input that cannot be steered —
    /// a slash command or a shell-style line — is queued for after the turn,
    /// as Enter would: a command needs the agent the turn is holding.
    async fn steer_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        if !self.turn_active {
            if text.starts_with('/') {
                self.run_command(&text).await;
            } else {
                self.send_prompt(text).await;
            }
            return;
        }
        if !is_steerable_input(&text) {
            self.queue_input(text);
            return;
        }
        self.handles.steer.steer(text.clone());
        self.messages.push(ChatMessage {
            role: ChatRole::System,
            text: format!("[steered] {text}"),
        });
        self.scroll_to_bottom();
        self.status = "Steered into the running turn".into();
    }

    /// Queue `text` for after the running turn, and say so.
    fn queue_input(&mut self, text: String) {
        self.queue.push(text.clone());
        self.queued_count += 1;
        self.fire_prompt_queued_hook(&text);
        self.messages.push(ChatMessage {
            role: ChatRole::System,
            text: format!("[queued] {text}"),
        });
        self.scroll_to_bottom();
        let pending = self.queue.entries.len();
        self.status = format!("Queued ({pending} for after this turn)");
    }

    /// The `UserPromptQueued` hook: fired when a prompt is queued for
    /// after the running turn. Spawned rather than awaited: the hook's
    /// process must not hold up the event loop while a turn runs.
    fn fire_prompt_queued_hook(&self, text: &str) {
        let Some(hooks) = &self.handles.user_hooks else {
            return;
        };
        let mut fields = serde_json::Map::new();
        fields.insert(
            "prompt_id".to_string(),
            serde_json::Value::String(format!("queued-{}", self.queued_count)),
        );
        fields.insert(
            "prompt".to_string(),
            serde_json::Value::String(text.to_string()),
        );
        fields.insert(
            "queue_length".to_string(),
            serde_json::Value::from(self.queue.entries.len() as u64),
        );
        hooks.fire_spawned(UserHookEvent::UserPromptQueued, text, fields);
    }

    /// Run the queued inputs in order: slash commands execute, plain text
    /// starts a new turn (the rest of the queue waits for it).
    async fn drain_queue(&mut self) {
        while !self.should_quit && !self.queue.is_empty() {
            let Some(text) = self.queue.pop() else { break };
            if text.starts_with('/') {
                if self.run_command(&text).await {
                    return; // a follow-up turn started; the queue waits for it
                }
            } else {
                self.send_prompt(text).await;
                return;
            }
        }
    }

    /// Execute a slash command. Returns `true` when it started a turn (a
    /// follow-up prompt like `/swarm`): a caller draining the queue must then
    /// wait for that turn instead of continuing.
    async fn run_command(&mut self, text: &str) -> bool {
        // Commands lock the agent; while a turn runs they are queued instead.
        debug_assert!(!self.turn_active, "agent lock taken while a turn runs");
        let cmd = Command::parse(text);

        if matches!(cmd, Command::Exit) {
            self.messages.push(ChatMessage {
                role: ChatRole::System,
                text: "Goodbye!".to_string(),
            });
            self.should_quit = true;
            return false;
        }

        let clears_canvas = matches!(cmd, Command::New | Command::Clear);
        let follow_up = cmd.follow_up_prompt();
        let result = commands::execute(&mut self.ctx, cmd).await;
        let succeeded = result.is_ok();
        self.status = format!("model: {}", self.ctx.llm.model_name());

        if clears_canvas {
            self.messages.clear();
            // The queued entries addressed the wiped conversation.
            self.queue = PromptQueue::default();
        }

        match result {
            Ok(msg) => self.messages.push(ChatMessage {
                role: ChatRole::System,
                text: msg,
            }),
            Err(err) => self.messages.push(ChatMessage {
                role: ChatRole::Error,
                text: err,
            }),
        }
        self.scroll_to_bottom();
        // `/swarm <task>` goes on to send its task.
        if succeeded && let Some(task) = follow_up {
            self.send_prompt(task).await;
            return true;
        }
        false
    }

    /// Send `text` as the user's prompt.
    async fn send_prompt(&mut self, text: String) {
        self.start_turn(text, PromptOrigin::User).await;
    }

    /// Start a turn on `text`, a prompt from `origin`.
    async fn start_turn(&mut self, text: String, origin: PromptOrigin) {
        debug_assert!(!self.turn_active, "agent lock taken while a turn runs");
        self.last_prompt = (origin == PromptOrigin::User).then(|| text.clone());
        self.messages.push(ChatMessage {
            role: ChatRole::User,
            text: text.clone(),
        });
        self.input.clear();
        self.cursor = 0;
        self.scroll_to_bottom();

        let agent = self.ctx.agent.clone();
        {
            let mut agent = agent.lock().await;
            let tool_event_sender: ToolEventSender = Arc::new({
                let tx = self.tool_event_tx.clone();
                move |event| {
                    let _ = tx.send(event);
                }
            });
            agent.set_tool_event_sender(tool_event_sender);
        }
        let llm = self.ctx.llm.clone();
        self.prompt_cancel = CancellationToken::new();
        let cancel = self.prompt_cancel.clone();
        let stream_tx = self.stream_tx.clone();
        let retry_tx = self.stream_tx.clone();
        let compaction_tx = self.stream_tx.clone();
        let tool_event_tx_for_delta = self.tool_event_tx.clone();
        let callbacks = LlmCallbacks {
            on_text_delta: Some(Arc::new(move |delta: &str| {
                let _ = stream_tx.send(StreamItem::Delta(delta.to_string()));
            })),
            on_retry: Some(Arc::new(move |notice: &RetryNotice| {
                let _ = retry_tx.send(StreamItem::Retry(notice.describe()));
            })),
            on_compaction: Some(Arc::new(move |notice: &CompactionNotice| {
                let started = matches!(notice, CompactionNotice::Started { .. });
                let _ = compaction_tx.send(StreamItem::Compaction(started, notice.describe()));
            })),
            on_tool_call_delta: Some(Arc::new(move |tc: &ToolCall| {
                let _ = tool_event_tx_for_delta.send(ToolEvent::Call {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    args: tc.arguments.clone().unwrap_or_default(),
                    status: ToolStatus::Pending,
                });
            })),
            ..Default::default()
        };
        self.status = "Thinking...".into();
        self.turn_active = true;
        self.task = Some(tokio::spawn(async move {
            agent
                .lock()
                .await
                .prompt_from(origin, llm.as_ref(), text, callbacks, &cancel)
                .await
        }));
    }

    fn handle_stream_item(&mut self, item: StreamItem) {
        match item {
            StreamItem::Delta(delta) => self.handle_stream_delta(delta),
            StreamItem::Retry(description) => {
                // The notice follows the partial answer it supersedes; the
                // retried output then opens a fresh assistant message below it.
                self.messages.push(ChatMessage {
                    role: ChatRole::System,
                    text: format!("[retry] {description}"),
                });
                self.status = "Retrying...".into();
                self.scroll_to_bottom();
            }
            StreamItem::Compaction(started, description) => {
                self.messages.push(ChatMessage {
                    role: ChatRole::System,
                    text: format!("[compaction] {description}"),
                });
                self.status = if started {
                    "Compacting context...".into()
                } else {
                    "Thinking...".into()
                };
                self.scroll_to_bottom();
            }
        }
    }

    fn handle_stream_delta(&mut self, delta: String) {
        if let Some(last) = self.messages.last_mut()
            && matches!(last.role, ChatRole::Assistant)
        {
            last.text.push_str(&delta);
            self.scroll_to_bottom();
            return;
        }
        self.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            text: delta,
        });
        self.scroll_to_bottom();
    }

    fn handle_tool_event(&mut self, event: ToolEvent) {
        match event {
            ToolEvent::Call { name, args, .. } => {
                self.messages.push(ChatMessage {
                    role: ChatRole::System,
                    text: format!("[tool-call] {name}({args})"),
                });
                self.scroll_to_bottom();
                if name == agent_core::tools::builtin::wait_for::WAIT_FOR_TOOL_NAME {
                    self.steer_queue_into_wait();
                }
            }
            ToolEvent::Result { id, status, .. } => {
                self.messages.push(ChatMessage {
                    role: ChatRole::System,
                    text: format!("[tool] {id} {status:?}"),
                });
                self.scroll_to_bottom();
            }
            ToolEvent::Progress {
                stdout_chunk,
                stderr_chunk,
                ..
            } => {
                if let Some(out) = stdout_chunk {
                    if let Some(last) = self.messages.last_mut()
                        && matches!(last.role, ChatRole::Assistant)
                    {
                        last.text.push_str(&out);
                        self.scroll_to_bottom();
                        return;
                    }
                    self.messages.push(ChatMessage {
                        role: ChatRole::Assistant,
                        text: out,
                    });
                    self.scroll_to_bottom();
                }
                if let Some(err) = stderr_chunk {
                    self.messages.push(ChatMessage {
                        role: ChatRole::Error,
                        text: err,
                    });
                    self.scroll_to_bottom();
                }
            }
        }
    }

    /// Messages queued when the model starts a `wait_for` are steered in
    /// automatically — unless they are shell-style or slash commands — so they
    /// reach the model now instead of waiting out the turn. The wait ends early
    /// with the interrupted-by-message result.
    fn steer_queue_into_wait(&mut self) {
        if !self.turn_active {
            return;
        }
        let steered = self.queue.drain_steerable();
        if steered.is_empty() {
            return;
        }
        for text in steered {
            self.handles.steer.steer(text.clone());
            self.messages.push(ChatMessage {
                role: ChatRole::System,
                text: format!("[steered] {text}"),
            });
        }
        self.scroll_to_bottom();
    }

    async fn handle_result(&mut self, result: Result<StepResult, AgentError>) {
        self.status = format!("model: {}", self.ctx.llm.model_name());
        match result {
            Ok(step) => {
                let stopped = step.stop_reason;
                debug_assert!(!self.turn_active, "agent lock taken while a turn runs");
                let (text, first_exchange) = {
                    let agent = self.ctx.agent.lock().await;
                    let text = agent.history().last().and_then(|m| {
                        if m.role == Role::Assistant {
                            Some(m.text_content())
                        } else {
                            None
                        }
                    });
                    (text, agent.first_exchange())
                };
                if let Some(text) = text {
                    if let Some(last) = self.messages.last_mut() {
                        if matches!(last.role, ChatRole::Assistant) {
                            last.text = text;
                        } else {
                            self.messages.push(ChatMessage {
                                role: ChatRole::Assistant,
                                text,
                            });
                        }
                    } else {
                        self.messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            text,
                        });
                    }
                }
                if let Some(reason) = stopped {
                    self.messages.push(ChatMessage {
                        role: ChatRole::System,
                        text: crate::stream::stop_notice(reason),
                    });
                }
                crate::record_session_activity(&self.ctx, None).await;
                crate::session_title::after_turn(&self.ctx, first_exchange);
            }
            Err(AgentError::SessionOccupied(occupied)) => {
                self.messages.push(ChatMessage {
                    role: ChatRole::Error,
                    text: occupied.to_string(),
                });
                self.messages.push(ChatMessage {
                    role: ChatRole::System,
                    text: "Press Enter to send it again, or run /takeover to take the session \
                           over."
                        .to_string(),
                });
                if self.input.is_empty()
                    && let Some(prompt) = self.last_prompt.take()
                {
                    self.cursor = prompt.chars().count();
                    self.input = prompt;
                }
            }
            Err(e) => {
                error!("Agent error: {e}");
                self.messages.push(ChatMessage {
                    role: ChatRole::Error,
                    text: e.to_string(),
                });
            }
        }
        self.scroll_to_bottom();
        // Input queued during the turn runs now, in order.
        self.drain_queue().await;
    }

    fn scroll_to_bottom(&mut self) {
        let idx = self.messages.len().saturating_sub(1);
        self.list_state.select(Some(idx));
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);
        let chat_area = chunks[0];
        let input_area = chunks[1];
        let status_area = chunks[2];

        self.draw_chat(frame, chat_area);
        self.draw_input(frame, input_area);
        self.draw_status(frame, status_area);
    }

    fn draw_chat(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let inner = area.inner(Margin::new(1, 1));
        let usable_width = inner.width as usize;

        let items: Vec<ListItem> = self
            .messages
            .iter()
            .map(|msg| {
                let (label, style) = match msg.role {
                    ChatRole::User => ("you: ", Style::default().fg(Color::Green)),
                    ChatRole::Assistant => ("rusta: ", Style::default().fg(Color::Cyan)),
                    ChatRole::Error => ("error: ", Style::default().fg(Color::Red)),
                    ChatRole::System => ("system: ", Style::default().fg(Color::Yellow)),
                };
                let prefix_len = label.chars().count();
                let width = usable_width.saturating_sub(prefix_len);
                let wrapped = wrap_text(&msg.text, width);
                let lines: Vec<Line> = wrapped
                    .into_iter()
                    .enumerate()
                    .map(|(i, line)| {
                        if i == 0 {
                            Line::from(vec![Span::styled(label, style), Span::raw(line)])
                        } else {
                            let indent = " ".repeat(prefix_len);
                            Line::from(vec![Span::raw(indent), Span::raw(line)])
                        }
                    })
                    .collect();
                ListItem::new(Text::from(lines))
            })
            .collect();

        // The session's title once it has one: set with `/title`, or made
        // for it after a turn.
        let title = self.ctx.session_title.current();
        let title = if title.trim().is_empty() {
            "Chat".to_string()
        } else {
            format!("Chat · {title}")
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_input(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let inner = area.inner(Margin::new(1, 1));
        let title = if self.pending_approval.is_some() {
            "Approve? [y/n/a/s/c]"
        } else if self.pending_question.is_some() {
            "Your answer"
        } else {
            "Input"
        };
        let paragraph = Paragraph::new(self.input.as_str())
            .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(paragraph, area);
        let cursor_chars = self.input[..self.cursor].chars().count();
        frame.set_cursor_position(Position::new(inner.x + cursor_chars as u16, inner.y));
    }

    fn draw_status(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let status = Paragraph::new(self.status.clone()).style(Style::default().fg(Color::Gray));
        frame.render_widget(status, area);
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        let mut remaining = raw_line;
        while !remaining.is_empty() {
            if remaining.chars().count() <= width {
                lines.push(remaining.to_string());
                break;
            }
            let mut byte_idx = 0;
            for (idx, _) in remaining.char_indices() {
                if remaining[..idx].chars().count() == width {
                    byte_idx = idx;
                    break;
                }
            }
            if byte_idx == 0 {
                // Fallback for abnormal widths; consume one char.
                let first = remaining.chars().next().unwrap_or_default();
                byte_idx = first.len_utf8();
            }
            lines.push(remaining[..byte_idx].to_string());
            remaining = &remaining[byte_idx..];
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Esc interrupts a running turn; it never quits the app (the quit paths
    /// are Ctrl-C twice, Ctrl-D on empty input, and /exit).
    #[test]
    fn esc_interrupts_the_turn_and_does_not_quit() {
        assert_eq!(esc_outcome(true), EscOutcome::Interrupt);
        assert_eq!(esc_outcome(false), EscOutcome::ClearInput);
    }

    /// Ctrl-C interrupts while a turn runs; only a second press while idle
    /// exits.
    #[test]
    fn ctrl_c_interrupts_then_arms_then_exits() {
        assert_eq!(ctrl_c_outcome(true, false), CtrlCOutcome::Interrupt);
        assert_eq!(ctrl_c_outcome(true, true), CtrlCOutcome::Interrupt);
        assert_eq!(ctrl_c_outcome(false, false), CtrlCOutcome::ArmExit);
        assert_eq!(ctrl_c_outcome(false, true), CtrlCOutcome::Exit);
    }

    /// Queued input runs after the turn, in the order it was queued.
    #[test]
    fn the_queue_runs_in_order() {
        let mut queue = PromptQueue::default();
        queue.push("first".to_string());
        queue.push("second".to_string());
        queue.push("third".to_string());
        assert_eq!(queue.pop().as_deref(), Some("first"));
        assert_eq!(queue.pop().as_deref(), Some("second"));
        assert_eq!(queue.pop().as_deref(), Some("third"));
        assert!(queue.is_empty());
        assert!(queue.pop().is_none());
    }

    /// An `App` on a test context, with its handles taken as `run` takes them.
    async fn test_app(dir: &std::path::Path) -> App {
        let ctx = commands::CommandContext::for_test(dir).await;
        let handles = SessionHandles::of(&ctx).await;
        let (_approval_tx, approval_rx) = mpsc::unbounded_channel();
        let (_user_tx, user_rx) = mpsc::unbounded_channel();
        App::new(ctx, CancellationToken::new(), approval_rx, user_rx, handles)
    }

    /// The chat pane's top border: `Chat`, then the session's title once it
    /// has one.
    #[tokio::test]
    async fn the_chat_pane_shows_the_session_title() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(dir.path()).await;
        let top_row = |app: &mut App| {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12))
                .expect("a test terminal");
            terminal
                .draw(|frame| app.draw(frame))
                .expect("a drawn frame");
            let buffer = terminal.backend().buffer().clone();
            (0..buffer.area.width)
                .map(|x| buffer[(x, 0)].symbol().to_string())
                .collect::<String>()
        };
        assert!(top_row(&mut app).contains("Chat─"), "{}", top_row(&mut app));
        app.ctx.session_title.set("Fix the login test");
        let row = top_row(&mut app);
        assert!(row.contains("Chat · Fix the login test"), "{row}");
    }

    /// A plan review shows the plan whole, to be read before it is approved;
    /// any other approval names the tool it asks about.
    #[tokio::test]
    async fn a_plan_review_shows_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(dir.path()).await;
        let (resp_tx, resp_rx) = oneshot::channel();
        app.handle_approval_request((
            ApprovalRequest::new(
                "exit_plan_mode",
                agent_core::permissions::PLAN_REVIEW_RULE,
                serde_json::json!({ "plan": "## Context\nFix the parser.\n" }),
            ),
            resp_tx,
        ));
        let shown = &app.messages.last().expect("the plan is shown").text;
        assert_eq!(
            shown,
            "[plan review] The agent's plan:\n\n## Context\nFix the parser."
        );
        assert!(
            app.status.starts_with("Approve this plan"),
            "{}",
            app.status
        );
        app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(resp_rx.await.unwrap(), ApprovalResponse::AllowOnce);

        let (resp_tx, _resp_rx) = oneshot::channel();
        app.handle_approval_request((
            ApprovalRequest::new(
                "bash",
                "run_command",
                serde_json::json!({ "command": "ls" }),
            ),
            resp_tx,
        ));
        assert_eq!(app.status, "Approve bash? [y/n/a/s/c]");
    }

    /// A message the session's lock refused is put back in the input, to
    /// send again, and the chat says how to take the session over.
    #[tokio::test]
    async fn a_refused_message_is_put_back_to_send_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(dir.path()).await;
        app.last_prompt = Some("fix the tests".to_string());
        let occupied = agent_core::session_lock::SessionOccupied {
            session: "default".to_string(),
            holder: None,
        };
        app.handle_result(Err(AgentError::SessionOccupied(occupied)))
            .await;
        assert_eq!(app.input, "fix the tests");
        assert_eq!(app.cursor, "fix the tests".chars().count());
        let said: Vec<&str> = app.messages.iter().map(|m| m.text.as_str()).collect();
        assert!(
            said.iter()
                .any(|t| t.contains("Session 'default' is in use")),
            "{said:?}"
        );
        assert!(said.iter().any(|t| t.contains("/takeover")), "{said:?}");
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn type_input(app: &mut App, text: &str) {
        app.input = text.to_string();
        app.cursor = app.input.len();
    }

    /// While a turn holds the agent lock, everything the event loop does
    /// finishes at once: none of it waits for the lock. Enter queues,
    /// Ctrl-S steers plain text and queues a command, Esc and Ctrl-C
    /// interrupt without quitting, and no second turn is started.
    #[tokio::test]
    async fn the_event_loop_never_waits_for_a_running_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(dir.path()).await;
        let agent = Arc::clone(&app.ctx.agent);
        let (locked_tx, locked_rx) = oneshot::channel();
        let turn = tokio::spawn(async move {
            let _guard = agent.lock().await;
            let _ = locked_tx.send(());
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        locked_rx.await.unwrap();
        app.turn_active = true;

        let handled = tokio::time::timeout(Duration::from_secs(2), async {
            app.check_background_notifications();
            type_input(&mut app, "queued text");
            app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .unwrap();
            type_input(&mut app, "steer me");
            app.handle_event(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .await
                .unwrap();
            type_input(&mut app, "/compact");
            app.handle_event(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .await
                .unwrap();
            app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE))
                .await
                .unwrap();
            app.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .await
                .unwrap();
        })
        .await;
        turn.abort();
        assert!(
            handled.is_ok(),
            "an event-loop handler waited for the agent lock a running turn holds"
        );

        assert_eq!(
            app.queue.entries,
            vec!["queued text".to_string(), "/compact".to_string()],
            "Enter queues; a command sent with Ctrl-S is queued, not steered"
        );
        assert_eq!(app.handles.steer.drain(), vec!["steer me".to_string()]);
        assert!(app.prompt_cancel.is_cancelled(), "Esc interrupts the turn");
        assert!(
            app.handles
                .user_cancel
                .load(std::sync::atomic::Ordering::SeqCst),
            "as the user's own cancel"
        );
        assert!(!app.should_quit, "interrupting never quits");
        assert!(app.exit_armed_at.is_none(), "Ctrl-C mid-turn interrupts");
        assert!(app.task.is_none(), "no second turn was started");
    }

    /// Cron fires that come due together share one turn: the first is its
    /// prompt and the others are steered in. The user's half-typed input
    /// survives the delivery.
    #[tokio::test]
    async fn cron_fires_due_together_share_one_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(dir.path()).await;
        let two_minutes_ago = agent_core::cron::now_millis() - 120_000;
        for prompt in ["first job", "second job"] {
            app.handles
                .cron
                .add_at("* * * * *", prompt, true, two_minutes_ago)
                .unwrap();
        }
        type_input(&mut app, "half-typed");
        app.cursor = 4;

        app.deliver_cron_fires().await;
        app.task
            .take()
            .expect("the first fire starts a turn")
            .abort();

        assert!(app.turn_active);
        let prompt = &app.messages.last().unwrap().text;
        assert!(prompt.contains("first job"), "{prompt}");
        let steered = app.handles.steer.drain();
        assert_eq!(steered.len(), 1, "the second fire joins the same turn");
        assert!(steered[0].contains("second job"), "{}", steered[0]);
        assert_eq!(app.input, "half-typed");
        assert_eq!(app.cursor, 4);
    }

    /// Queued shell-style and slash-command input is not steered into
    /// a running wait; plain text is, in order.
    #[test]
    fn auto_steer_skips_shell_and_command_input() {
        let mut queue = PromptQueue::default();
        queue.push("plain one".to_string());
        queue.push("!make test".to_string());
        queue.push("/compact".to_string());
        queue.push("plain two".to_string());

        assert_eq!(
            queue.drain_steerable(),
            vec!["plain one".to_string(), "plain two".to_string()]
        );
        assert_eq!(
            queue.pop().as_deref(),
            Some("!make test"),
            "the shell command stayed queued"
        );
        assert_eq!(
            queue.pop().as_deref(),
            Some("/compact"),
            "the slash command stayed queued"
        );
        assert!(queue.is_empty());
    }
}
