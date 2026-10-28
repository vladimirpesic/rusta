//! Per-session scheduled prompts: `cron_create` enqueues a prompt that is
//! re-injected into *this session* — never a shell command — when the session
//! is idle, wrapped in a `<cron-fire>` envelope.
//!
//! A task is `{cron, prompt, recurring}`: a 5-field cron expression read in
//! local time, the prompt to enqueue, and whether it repeats. Tasks live in
//! the session's records directory and come back on resume; a new session or
//! a fork starts empty. Fires are computed on a poll tick (the front-ends
//! drive [`CronManager::tick`] while idle): missed fires coalesce into one
//! delivery (`coalescedCount`), recurring tasks past 7 days get one final
//! `stale="true"` fire and are then deleted, one-shots auto-delete after
//! their single fire, and deterministic per-id jitter keeps the fleet off the
//! `:00` and `:30` marks.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Datelike, Local, NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};

/// At most this many live tasks per session.
pub const MAX_TASKS: usize = 50;

/// A scheduled prompt is at most this many UTF-8 bytes.
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;

/// What a sub-agent's cron call answers.
pub const CRON_MAIN_AGENT_ONLY: &str = "Cron tools are only supported by the main agent.";

/// What a session fork is told when the source session had cron tasks.
pub const CRON_FORK_CLEARED_REMINDER: &str = "This fork does not have any scheduled cron tasks. Tasks from the source session \
     continue to run in the source session. Create new tasks here if needed.";

/// [`CRON_FORK_CLEARED_REMINDER`] as the `<system-reminder>` user message a
/// session fork's records end with.
#[must_use]
pub fn cron_fork_cleared_message() -> lm_bridge::message::Message {
    lm_bridge::message::Message::user(format!(
        "<system-reminder>\n{CRON_FORK_CLEARED_REMINDER}\n</system-reminder>"
    ))
}

/// The plan-mode veto for the cron tools: both `cron_create` and
/// `cron_delete` are blocked in plan mode.
pub fn plan_mode_denial(tool_name: &str) -> String {
    format!(
        "{tool_name} is not available in plan mode because it would mutate scheduled \
         work that runs after plan exit. Call exit_plan_mode first."
    )
}

/// The kill-switch refusal, naming Rusta's variable (`KIMI_DISABLE_CRON` is
/// honored too, as an alias).
pub fn disabled_message() -> String {
    "Cron scheduling is disabled (RUSTA_DISABLE_CRON=1).".to_string()
}

/// A `cron_delete` id: Rusta's 8-hex ids, or a ULID.
pub const ID_PATTERN: &str = r"^(?:[0-9a-f]{8}|[0-9A-HJKMNP-TV-Z]{26})$";

/// A recurring task older than this fires one final time with
/// `stale="true"` and is then auto-deleted.
const STALE_THRESHOLD_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// A one-shot whose first fire lands further out than this is refused at
/// create time.
pub const ONE_SHOT_MAX_FUTURE_MS: i64 = 350 * 24 * 60 * 60 * 1000;

const MAX_COALESCE_ITERATIONS: usize = 10_000;
const FIVE_YEARS_CAP_MINUTES: i64 = 5 * 366 * 24 * 60;
const MS_PER_MINUTE: i64 = 60_000;
const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
const RECURRING_JITTER_MAX_FRACTION: f64 = 0.1;
const RECURRING_JITTER_MAX_MS: f64 = 15.0 * 60_000.0;
const ONE_SHOT_JITTER_MAX_MS: f64 = 90_000.0;
const ID_HEX_LEN: usize = 8;

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Whether cron is turned off for this process: `RUSTA_DISABLE_CRON=1`,
/// with `KIMI_DISABLE_CRON` honored as an alias.
pub fn cron_disabled() -> bool {
    cron_disabled_with(|name| std::env::var(name).ok())
}

/// [`cron_disabled`], reading variables through `env`, injected so tests need
/// not mutate the process.
fn cron_disabled_with(env: impl Fn(&str) -> Option<String>) -> bool {
    ["RUSTA_DISABLE_CRON", "KIMI_DISABLE_CRON"]
        .into_iter()
        .any(|name| env(name).is_some_and(|value| flag_on(&value)))
}

/// Whether `value` turns a flag on, as Rusta's other boolean variables
/// read: `1`, `true`, `yes` or `on`, in any case, surrounding spaces aside.
fn flag_on(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// A scheduled prompt. `created_at`/`last_fired_at` are wall-clock
/// milliseconds; the cursor (`last_fired_at`) is where the next fire is
/// computed from, so a resumed session resumes the schedule where it left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronTask {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub created_at: i64,
    #[serde(default = "default_recurring")]
    pub recurring: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<i64>,
}

fn default_recurring() -> bool {
    true
}

/// A due fire of one task: what its `<cron-fire>` envelope tells the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFire {
    pub job_id: String,
    pub cron: String,
    pub recurring: bool,
    /// How many scheduled runs this one delivery stands for
    /// (`coalescedCount`).
    pub coalesced_count: usize,
    /// The task's final fire before it is deleted for its age.
    pub stale: bool,
    pub prompt: String,
}

impl CronFire {
    /// The envelope the model receives.
    #[must_use]
    pub fn envelope(&self) -> String {
        render_cron_fire_xml(
            &self.job_id,
            &self.cron,
            self.recurring,
            self.coalesced_count,
            self.stale,
            &self.prompt,
        )
    }

    /// Fold a later fire of the same job into this one: the runs add up, the
    /// fire is stale when either was, and the later fire's prompt is kept.
    fn absorb(&mut self, later: Self) {
        *self = Self {
            coalesced_count: self.coalesced_count + later.coalesced_count,
            stale: self.stale || later.stale,
            ..later
        };
    }
}

/// The per-session cron store. Front-ends poll [`Self::tick`] while the
/// session is idle; nothing runs in the background, so a fire never injects
/// into a running turn and a missed window is coalesced by the next tick.
pub struct CronManager {
    tasks: Mutex<Vec<CronTask>>,
    /// Fires waiting for the next turn, one per job ([`Self::queue_fires`]).
    pending_fires: Mutex<Vec<CronFire>>,
    persist_path: Mutex<Option<PathBuf>>,
}

impl CronManager {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
            pending_fires: Mutex::new(Vec::new()),
            persist_path: Mutex::new(None),
        }
    }

    /// Load the session's cron sidecar at `path`. A resumed session gets its
    /// tasks back; a fresh one starts empty. Either way future changes
    /// persist to `path`.
    pub fn load(path: &Path, resumed: bool) -> Self {
        let manager = Self::new();
        if resumed {
            match std::fs::read_to_string(path) {
                Ok(contents) => match serde_json::from_str::<Vec<CronTask>>(&contents) {
                    Ok(tasks) => *manager.tasks.lock().unwrap() = tasks,
                    Err(e) => {
                        tracing::warn!("ignoring unreadable cron sidecar {}: {e}", path.display())
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("could not read cron sidecar {}: {e}", path.display()),
            }
        }
        *manager.persist_path.lock().unwrap() = Some(path.to_path_buf());
        manager
    }

    /// Path future changes persist to, when this store is backed by a file.
    pub fn persist_path(&self) -> Option<PathBuf> {
        self.persist_path.lock().unwrap().clone()
    }

    /// Point future changes at `path` without loading anything.
    pub fn set_persist_path(&self, path: PathBuf) {
        *self.persist_path.lock().unwrap() = Some(path);
    }

    pub(crate) fn save(&self) {
        let Some(path) = self.persist_path() else {
            return;
        };
        let tasks = self.tasks.lock().unwrap().clone();
        let write = || -> Result<(), String> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create cron directory: {e}"))?;
            }
            let file = std::fs::File::create(&path)
                .map_err(|e| format!("failed to create cron file: {e}"))?;
            serde_json::to_writer_pretty(file, &tasks)
                .map_err(|e| format!("failed to write cron file: {e}"))
        };
        if let Err(e) = write() {
            tracing::warn!("failed to persist cron tasks: {e}");
        }
    }

    /// Whether the kill switch is set (`RUSTA_DISABLE_CRON` /
    /// `KIMI_DISABLE_CRON`).
    pub fn is_disabled(&self) -> bool {
        cron_disabled()
    }

    /// Add a task. Refuses the 51st live task.
    pub fn add(&self, cron: &str, prompt: &str, recurring: bool) -> Result<CronTask, String> {
        self.add_at(cron, prompt, recurring, now_millis())
    }

    /// [`Self::add`] against an explicit clock, the deterministic seam the
    /// tests use.
    pub fn add_at(
        &self,
        cron: &str,
        prompt: &str,
        recurring: bool,
        now: i64,
    ) -> Result<CronTask, String> {
        {
            let tasks = self.tasks.lock().unwrap();
            if tasks.len() >= MAX_TASKS {
                return Err(format!(
                    "Cron job cap reached (max {MAX_TASKS} per session)."
                ));
            }
        }
        let mut attempts = 0;
        let id = loop {
            let id = new_id();
            if self.get(&id).is_none() {
                break id;
            }
            attempts += 1;
            if attempts >= 8 {
                return Err("failed to generate a unique cron job id".to_string());
            }
        };
        let task = CronTask {
            id,
            cron: cron.to_string(),
            prompt: prompt.to_string(),
            created_at: now,
            recurring,
            last_fired_at: None,
        };
        self.tasks.lock().unwrap().push(task.clone());
        self.save();
        Ok(task)
    }

    /// Drop every task and every fire waiting for delivery, and persist the
    /// empty state. The store itself stays: a front-end holding a handle to it
    /// (the TUI ticks it without the agent lock) keeps working after `/new`.
    pub fn clear(&self) {
        self.tasks.lock().unwrap().clear();
        self.pending_fires.lock().unwrap().clear();
        self.save();
    }

    /// Delete a task by id; not-found is a plain `false`, which the tool
    /// reports as an error rather than a silent no-op.
    pub fn remove(&self, id: &str) -> bool {
        let mut tasks = self.tasks.lock().unwrap();
        let before = tasks.len();
        tasks.retain(|t| t.id != id);
        let removed = tasks.len() != before;
        drop(tasks);
        if removed {
            self.save();
        }
        removed
    }

    pub fn get(&self, id: &str) -> Option<CronTask> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .cloned()
    }

    /// All live tasks, in insertion order.
    pub fn list(&self) -> Vec<CronTask> {
        self.tasks.lock().unwrap().clone()
    }

    pub fn task_count(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }

    /// Whether a recurring task is past its 7-day window at `now`.
    pub fn is_stale_at(&self, task: &CronTask, now: i64) -> bool {
        task.recurring && now - task.created_at >= STALE_THRESHOLD_MS
    }

    /// The next post-jitter fire time from the task's cursor, for display
    /// (`cron_list`, `cron_create`'s `nextFireAt`).
    pub fn next_fire_for(&self, task: &CronTask, now: i64) -> Option<i64> {
        let parsed = parse_cron_expression(&task.cron).ok()?;
        let base = cursor_base(task, now);
        let ideal = compute_next_cron_run(&parsed, base)?;
        Some(compute_jittered_next(task, &parsed, ideal))
    }

    /// Compute due fires at `now`, advancing cursors and deleting one-shots
    /// and stale tasks, and return the fires to deliver, one per due task.
    /// Does nothing when the kill switch is set.
    pub fn tick(&self) -> Vec<CronFire> {
        self.tick_at(now_millis())
    }

    /// [`Self::tick`] against an explicit clock; the deterministic seam the
    /// tests use.
    pub fn tick_at(&self, now: i64) -> Vec<CronFire> {
        if self.is_disabled() {
            return Vec::new();
        }
        let mut fires = Vec::new();
        let mut changed = false;
        {
            let mut tasks = self.tasks.lock().unwrap();
            let snapshot = tasks.clone();
            for task in &snapshot {
                let Some((fire, remove, cursor)) = process_due(task, now) else {
                    continue;
                };
                fires.push(fire);
                changed = true;
                let mut task = task.clone();
                if remove {
                    tasks.retain(|t| t.id != task.id);
                } else if let Some(cursor) = cursor {
                    task.last_fired_at = Some(cursor);
                    if let Some(slot) = tasks.iter_mut().find(|t| t.id == task.id) {
                        *slot = task;
                    }
                }
            }
        }
        if changed {
            self.save();
        }
        fires
    }

    /// Whether this store keeps a headless run alive: live tasks, or fires
    /// queued for delivery. The kill switch empties it.
    pub fn has_pending_work(&self) -> bool {
        !self.is_disabled() && (self.task_count() > 0 || self.pending_fire_count() > 0)
    }

    /// Queue due fires for injection ahead of the next turn's prompt, merged
    /// per job: a job already waiting absorbs its new fire, so a job that
    /// came due many times while the session sat idle is delivered once,
    /// with the runs it missed in its `coalescedCount`. Returns the ids of
    /// the jobs that were not waiting yet, which a front-end announces.
    pub fn queue_fires(&self, fires: Vec<CronFire>) -> Vec<String> {
        let mut pending = self.pending_fires.lock().unwrap();
        let mut newly_due = Vec::new();
        for fire in fires {
            match pending.iter_mut().find(|p| p.job_id == fire.job_id) {
                Some(waiting) => waiting.absorb(fire),
                None => {
                    newly_due.push(fire.job_id.clone());
                    pending.push(fire);
                }
            }
        }
        newly_due
    }

    /// Take the queued fires' `<cron-fire>` envelopes, one per job, in the
    /// order the jobs came due.
    pub fn drain_fire_markers(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending_fires.lock().unwrap())
            .iter()
            .map(CronFire::envelope)
            .collect()
    }

    /// Number of jobs whose fire waits for the next turn.
    pub fn pending_fire_count(&self) -> usize {
        self.pending_fires.lock().unwrap().len()
    }
}

impl Default for CronManager {
    fn default() -> Self {
        Self::new()
    }
}

/// What process_due decided for one task: its fire, whether the task is
/// spent (one-shot or stale), and the cursor to advance to when it is not.
type DueOutcome = (CronFire, bool, Option<i64>);

/// From the task's cursor, is a jittered fire due at `now`? A parse failure
/// skips the task.
fn process_due(task: &CronTask, now: i64) -> Option<DueOutcome> {
    let parsed = parse_cron_expression(&task.cron).ok()?;
    let base = cursor_base(task, now);
    let ideal = compute_next_cron_run(&parsed, base)?;
    let next_fire_at = compute_jittered_next(task, &parsed, ideal);
    if now < next_fire_at {
        return None;
    }
    let mut coalesced_count = 1usize;
    let mut last_due_ms = None;
    if task.recurring {
        let (count, last_due) = count_coalesced(task, &parsed, ideal, now);
        coalesced_count = count;
        last_due_ms = Some(last_due);
    }
    let stale = task.recurring && now - task.created_at >= STALE_THRESHOLD_MS;
    let fire = CronFire {
        job_id: task.id.clone(),
        cron: task.cron.clone(),
        recurring: task.recurring,
        coalesced_count,
        stale,
        prompt: task.prompt.clone(),
    };
    let remove = !task.recurring || stale;
    let cursor = (!remove).then_some(last_due_ms.unwrap_or(now));
    Some((fire, remove, cursor))
}

/// The base instant the next fire is computed from: the persisted cursor
/// when it is sane (finite and not in the future), else creation time.
fn cursor_base(task: &CronTask, now: i64) -> i64 {
    match task.last_fired_at {
        Some(seen) if seen <= now && seen > task.created_at => seen,
        _ => task.created_at,
    }
}

/// How many ideal fires fell between the first due fire and `now`, so one
/// delivery can say `coalescedCount="N"`.
fn count_coalesced(
    task: &CronTask,
    parsed: &ParsedCron,
    first_fire_ms: i64,
    now_ms: i64,
) -> (usize, i64) {
    let mut count = 1usize;
    let mut cursor = first_fire_ms;
    let mut last_due_ms = first_fire_ms;
    while count < MAX_COALESCE_ITERATIONS {
        let Some(next) = compute_next_cron_run(parsed, cursor) else {
            break;
        };
        if next > now_ms {
            break;
        }
        let jittered_next = compute_jittered_next(task, parsed, next);
        if jittered_next > now_ms {
            break;
        }
        count += 1;
        cursor = next;
        last_due_ms = next;
    }
    (count, last_due_ms)
}

/// The jittered fire for an ideal run time: one-shots at `:00`/`:30`
/// pull earlier by up to 90 s, never before creation; everything else shifts
/// forward by the recurring jitter.
pub fn compute_jittered_next(task: &CronTask, parsed: &ParsedCron, ideal_ms: i64) -> i64 {
    if task.recurring {
        jittered_next_run_ms(task, parsed, ideal_ms)
    } else {
        one_shot_jittered_next_run_ms(task, ideal_ms)
    }
}

fn jittered_next_run_ms(task: &CronTask, parsed: &ParsedCron, ideal_ms: i64) -> i64 {
    let period = match compute_next_cron_run(parsed, ideal_ms) {
        Some(next) if next > ideal_ms => next - ideal_ms,
        _ => MS_PER_DAY,
    };
    let cap = (period as f64 * RECURRING_JITTER_MAX_FRACTION).min(RECURRING_JITTER_MAX_MS);
    if cap <= 0.0 {
        return ideal_ms;
    }
    ideal_ms + (cap * fraction_from_id(&task.id)) as i64
}

fn one_shot_jittered_next_run_ms(task: &CronTask, ideal_ms: i64) -> i64 {
    if ideal_ms % MS_PER_MINUTE != 0 {
        return ideal_ms;
    }
    let minute = local_naive(ideal_ms).minute();
    if minute != 0 && minute != 30 {
        return ideal_ms;
    }
    let offset = (ONE_SHOT_JITTER_MAX_MS * fraction_from_id(&task.id)) as i64;
    let shifted = ideal_ms - offset;
    if shifted < task.created_at {
        return ideal_ms;
    }
    shifted
}

/// A deterministic [0, 1) fraction of the task id — the whole jitter for the
/// task's lifetime. 8-hex ids read as a number; anything else is djb2-hashed.
fn fraction_from_id(id: &str) -> f64 {
    const TWO_POW_32: f64 = 4294967296.0;
    if id.len() == ID_HEX_LEN
        && id.chars().all(|c| c.is_ascii_hexdigit())
        && let Ok(n) = u32::from_str_radix(id, 16)
    {
        return n as f64 / TWO_POW_32;
    }
    let mut hash: i32 = 5381;
    for b in id.bytes() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_add(hash)
            .wrapping_add(i32::from(b));
    }
    (hash as u32) as f64 / TWO_POW_32
}

fn new_id() -> String {
    let mut id = String::with_capacity(ID_HEX_LEN);
    for _ in 0..ID_HEX_LEN {
        id.push(char::from_digit(fastrand::u32(..0x10), 16).unwrap_or('0'));
    }
    id
}

// --- 5-field cron in local time ---

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCron {
    pub raw: String,
    pub minutes: BTreeSet<i32>,
    pub hours: BTreeSet<i32>,
    pub days_of_month: BTreeSet<i32>,
    pub months: BTreeSet<i32>,
    pub days_of_week: BTreeSet<i32>,
    pub dom_wildcard: bool,
    pub dow_wildcard: bool,
}

const MINUTE_RANGE: (i32, i32) = (0, 59);
const HOUR_RANGE: (i32, i32) = (0, 23);
const DOM_RANGE: (i32, i32) = (1, 31);
const MONTH_RANGE: (i32, i32) = (1, 12);
const DOW_RANGE: (i32, i32) = (0, 7);

/// Parse a 5-field cron expression in local time. Errors carry stable
/// messages (`cron.expression_invalid`).
pub fn parse_cron_expression(expr: &str) -> Result<ParsedCron, String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err("cron expression is empty".to_string());
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron expression must have exactly 5 fields (minute hour day-of-month month day-of-week); got {}",
            fields.len()
        ));
    }
    let minutes = parse_field(fields[0], MINUTE_RANGE, "minute")?;
    let hours = parse_field(fields[1], HOUR_RANGE, "hour")?;
    let days_of_month = parse_field(fields[2], DOM_RANGE, "day-of-month")?;
    let months = parse_field(fields[3], MONTH_RANGE, "month")?;
    let dow_raw = parse_field(fields[4], DOW_RANGE, "day-of-week")?;
    let days_of_week: BTreeSet<i32> = dow_raw
        .into_iter()
        .map(|v| if v == 7 { 0 } else { v })
        .collect();

    Ok(ParsedCron {
        raw: trimmed.to_string(),
        minutes,
        hours,
        days_of_month,
        months,
        days_of_week,
        dom_wildcard: fields[2] == "*",
        dow_wildcard: fields[4] == "*",
    })
}

fn parse_field(field: &str, (min, max): (i32, i32), name: &str) -> Result<BTreeSet<i32>, String> {
    if field.is_empty() {
        return Err(format!("cron {name} field is empty"));
    }
    let mut out = BTreeSet::new();
    for term in field.split(',') {
        if term.is_empty() {
            return Err(format!("cron {name} field has empty term in list"));
        }
        add_term(&mut out, term, (min, max), name)?;
    }
    if out.is_empty() {
        return Err(format!("cron {name} field matches no values"));
    }
    Ok(out)
}

fn parse_cron_int(raw: &str, name: &str, role: &str) -> Result<i32, String> {
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "cron {name} {role} must be a non-negative integer with digits only (got {raw:?})"
        ));
    }
    raw.parse::<i32>()
        .map_err(|_| format!("cron {name} {role} must be a non-negative integer (got {raw:?})"))
}

fn add_term(
    out: &mut BTreeSet<i32>,
    term: &str,
    (min, max): (i32, i32),
    name: &str,
) -> Result<(), String> {
    let (range_part, step) = match term.split_once('/') {
        Some((range_part, step_str)) => {
            if step_str.is_empty() {
                return Err(format!("cron {name} step is empty in {term:?}"));
            }
            let step = parse_cron_int(step_str, name, "step")?;
            if step <= 0 {
                return Err(format!(
                    "cron {name} step must be a positive integer (got {step_str:?})"
                ));
            }
            if range_part.is_empty() {
                return Err(format!(
                    "cron {name} step needs a range or \"*\" before \"/\" in {term:?}"
                ));
            }
            (range_part, step)
        }
        None => (term, 1),
    };

    let (lo, hi) = if range_part == "*" {
        (min, max)
    } else if let Some(dash) = range_part.find('-') {
        let lo = parse_cron_int(&range_part[..dash], name, "range lower bound")?;
        let hi = parse_cron_int(&range_part[dash + 1..], name, "range upper bound")?;
        if lo < min || hi > max || lo > hi {
            return Err(format!(
                "cron {name} range {lo}-{hi} out of bounds (must be {min}..{max}, ascending)"
            ));
        }
        (lo, hi)
    } else {
        let single = parse_cron_int(range_part, name, "value")?;
        if !(min..=max).contains(&single) {
            return Err(format!(
                "cron {name} value {single} out of range {min}..{max}"
            ));
        }
        if term.contains('/') {
            (single, max)
        } else {
            out.insert(single);
            return Ok(());
        }
    };

    let mut v = lo;
    while v <= hi {
        out.insert(v);
        v += step;
    }
    Ok(())
}

/// The next ideal fire after `from_ms`, or `None` when nothing matches
/// within 5×366 days.
pub fn compute_next_cron_run(expr: &ParsedCron, from_ms: i64) -> Option<i64> {
    next_run_within_minutes(expr, from_ms, FIVE_YEARS_CAP_MINUTES)
}

/// Whether the expression fires within
/// `years` years of `from_ms`.
pub fn has_fire_within_years(expr: &ParsedCron, years: i64, from_ms: i64) -> bool {
    let cap = (years * 366 * 24 * 60).max(1);
    next_run_within_minutes(expr, from_ms, cap).is_some()
}

/// Step minute by minute on local wall time, fast-forwarding whole non-matching
/// months/days/hours.
fn next_run_within_minutes(expr: &ParsedCron, from_ms: i64, cap_minutes: i64) -> Option<i64> {
    let start = local_naive(from_ms).with_second(0)?.with_nanosecond(0)?;
    let mut date = start + chrono::Duration::minutes(1);
    let deadline_ms = from_ms + cap_minutes * MS_PER_MINUTE;

    const HARD_ITERATION_CAP: usize = 10_000_000;
    let mut iterations = 0;
    while local_millis(date) <= deadline_ms && iterations < HARD_ITERATION_CAP {
        iterations += 1;
        if !expr.months.contains(&(date.month() as i32)) {
            date = advance_month(date);
            continue;
        }
        if !day_matches(expr, &date) {
            date = advance_day(date);
            continue;
        }
        if !expr.hours.contains(&(date.hour() as i32)) {
            date = advance_hour(date);
            continue;
        }
        if !expr.minutes.contains(&(date.minute() as i32)) {
            date = advance_minute(date);
            continue;
        }
        return Some(local_millis(date));
    }
    None
}

fn day_matches(expr: &ParsedCron, date: &NaiveDateTime) -> bool {
    let dom_ok = expr.days_of_month.contains(&(date.day() as i32));
    let dow_ok = expr
        .days_of_week
        .contains(&(date.weekday().num_days_from_sunday() as i32));

    if expr.dom_wildcard && expr.dow_wildcard {
        return true;
    }
    if expr.dom_wildcard {
        return dow_ok;
    }
    if expr.dow_wildcard {
        return dom_ok;
    }
    dom_ok || dow_ok
}

fn advance_month(date: NaiveDateTime) -> NaiveDateTime {
    let first = midnight(date).with_day(1).unwrap_or_else(|| midnight(date));
    first
        .checked_add_months(chrono::Months::new(1))
        .unwrap_or(first)
}

fn advance_day(date: NaiveDateTime) -> NaiveDateTime {
    midnight(date) + chrono::Duration::days(1)
}

fn advance_hour(date: NaiveDateTime) -> NaiveDateTime {
    let date = date
        .with_minute(0)
        .and_then(|d| d.with_second(0))
        .unwrap_or(date);
    date + chrono::Duration::hours(1)
}

fn advance_minute(date: NaiveDateTime) -> NaiveDateTime {
    let date = date.with_second(0).unwrap_or(date);
    date + chrono::Duration::minutes(1)
}

fn midnight(date: NaiveDateTime) -> NaiveDateTime {
    date.with_hour(0)
        .and_then(|d| d.with_minute(0))
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(date)
}

/// Local wall-clock view of a timestamp.
fn local_naive(ms: i64) -> NaiveDateTime {
    DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(|| DateTime::from_timestamp_millis(0).unwrap())
        .with_timezone(&Local)
        .naive_local()
}

/// The timestamp a local wall-clock instant resolves to. A DST gap nudges
/// forward until it lands; an ambiguous fall-back takes the earlier reading.
fn local_millis(naive: NaiveDateTime) -> i64 {
    match naive.and_local_timezone(Local) {
        chrono::LocalResult::Single(dt) => dt.timestamp_millis(),
        chrono::LocalResult::Ambiguous(early, _) => early.timestamp_millis(),
        chrono::LocalResult::None => local_millis(naive + chrono::Duration::hours(1)),
    }
}

// --- Rendering ---

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// A plain-English summary, or the raw expression when it fits no pattern.
pub fn cron_to_human(expr: &ParsedCron) -> String {
    let all_min = is_full_range(&expr.minutes, 0, 59);
    let all_hour = is_full_range(&expr.hours, 0, 23);
    let all_dom = expr.dom_wildcard;
    let all_month = is_full_range(&expr.months, 1, 12);
    let all_dow = expr.dow_wildcard;

    if all_hour && all_dom && all_month && all_dow {
        if let Some(step) = detect_step(&expr.minutes, 0, 59).filter(|s| *s > 1) {
            return format!("every {step} minutes");
        }
        if all_min {
            return "every minute".to_string();
        }
        if expr.minutes.len() == 1 {
            let m = *expr.minutes.iter().next().unwrap();
            return format!("at minute {m} of every hour");
        }
    }

    if expr.minutes.len() == 1 && all_dom && all_month && all_dow {
        let m = *expr.minutes.iter().next().unwrap();
        if let Some(step) = detect_step(&expr.hours, 0, 23).filter(|s| *s > 1) {
            return format!("every {step} hours at minute {m:02}");
        }
    }

    if expr.minutes.len() == 1 && expr.hours.len() == 1 && all_dom && all_month {
        let h = *expr.hours.iter().next().unwrap();
        let m = *expr.minutes.iter().next().unwrap();
        if all_dow {
            return format!("at {h:02}:{m:02} every day");
        }
        if let Some(dow_str) = format_dows(&expr.days_of_week) {
            return format!("at {h:02}:{m:02} on {dow_str}");
        }
    }

    if expr.minutes.len() == 1
        && expr.hours.len() == 1
        && expr.days_of_month.len() == 1
        && !expr.dom_wildcard
        && expr.months.len() == 1
        && all_dow
    {
        let h = *expr.hours.iter().next().unwrap();
        let m = *expr.minutes.iter().next().unwrap();
        let d = *expr.days_of_month.iter().next().unwrap();
        let mo = *expr.months.iter().next().unwrap();
        return format!(
            "at {h:02}:{m:02} on day {d} of {}",
            MONTH_NAMES[(mo - 1) as usize]
        );
    }

    expr.raw.clone()
}

fn is_full_range(set: &BTreeSet<i32>, min: i32, max: i32) -> bool {
    set.len() as i32 == max - min + 1 && set.iter().all(|v| (min..=max).contains(v))
}

fn detect_step(set: &BTreeSet<i32>, min: i32, max: i32) -> Option<i32> {
    let values: Vec<i32> = set.iter().copied().collect();
    if values.len() < 2 || values[0] != min {
        return None;
    }
    let step = values[1] - values[0];
    if step <= 0 {
        return None;
    }
    let mut expected = min;
    for v in &values {
        if *v != expected {
            return None;
        }
        expected += step;
    }
    if expected - step > max {
        return None;
    }
    Some(step)
}

fn format_dows(set: &BTreeSet<i32>) -> Option<String> {
    let values: Vec<i32> = set.iter().copied().collect();
    if values.is_empty() {
        return None;
    }
    if values.len() == 5 && values.iter().enumerate().all(|(i, v)| *v == i as i32 + 1) {
        return Some("weekdays".to_string());
    }
    if values.len() == 2 && values[0] == 0 && values[1] == 6 {
        return Some("weekends".to_string());
    }
    Some(
        values
            .iter()
            .map(|v| DAY_NAMES[*v as usize])
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// Local time as ISO 8601 with its offset: `2026-10-13T09:57:00.000+02:00`.
pub fn format_local_iso_with_offset(ms: i64) -> String {
    let date: DateTime<Local> = DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(|| DateTime::from_timestamp_millis(0).unwrap())
        .with_timezone(&Local);
    date.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()
}

/// The fire's envelope, prompt verbatim.
pub fn render_cron_fire_xml(
    job_id: &str,
    cron: &str,
    recurring: bool,
    coalesced_count: usize,
    stale: bool,
    prompt: &str,
) -> String {
    let job_id = xml_attr(job_id);
    let cron = xml_attr(cron);
    let recurring = if recurring { "true" } else { "false" };
    let coalesced_count = coalesced_count.to_string();
    let stale = if stale { "true" } else { "false" };
    format!(
        "<cron-fire jobId=\"{job_id}\" cron=\"{cron}\" recurring=\"{recurring}\" \
         coalescedCount=\"{coalesced_count}\" stale=\"{stale}\">\n\
         <prompt>\n\
         {prompt}\n\
         </prompt>\n\
         </cron-fire>"
    )
}

fn xml_attr(value: &str) -> String {
    if value.is_empty() {
        return "unknown".to_string();
    }
    value.replace('&', "&amp;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(expr: &str) -> ParsedCron {
        parse_cron_expression(expr).expect(expr)
    }

    fn task(cron: &str, id: &str, created_at: i64) -> CronTask {
        CronTask {
            id: id.to_string(),
            cron: cron.to_string(),
            prompt: "do the thing".to_string(),
            created_at,
            recurring: true,
            last_fired_at: None,
        }
    }

    fn millis(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        local_millis(NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            chrono::NaiveTime::from_hms_opt(h, min, 0).unwrap(),
        ))
    }

    #[test]
    fn parser_accepts_lists_ranges_and_steps() {
        let p = parse("*/15 1-5,20 1,15 */2 0,6");
        assert_eq!(
            p.minutes.iter().copied().collect::<Vec<_>>(),
            vec![0, 15, 30, 45]
        );
        assert_eq!(
            p.hours.iter().copied().collect::<Vec<_>>(),
            (1..=5).chain([20]).collect::<Vec<_>>()
        );
        assert_eq!(
            p.days_of_month.iter().copied().collect::<Vec<_>>(),
            vec![1, 15]
        );
        assert_eq!(
            p.months.iter().copied().collect::<Vec<_>>(),
            vec![1, 3, 5, 7, 9, 11]
        );
        assert_eq!(
            p.days_of_week.iter().copied().collect::<Vec<_>>(),
            vec![0, 6]
        );
    }

    #[test]
    fn parser_normalizes_sunday_seven_to_zero() {
        assert_eq!(
            parse("0 0 * * 7")
                .days_of_week
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![0]
        );
        assert_eq!(
            parse("0 0 * * 0")
                .days_of_week
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![0]
        );
    }

    #[test]
    fn parser_rejects_bad_expressions() {
        for expr in [
            "",
            "1 2 3",
            "a * * * *",
            "0 0 32 * *",
            "0 0 * 13 *",
            "0 0 * * 8",
            "*/0 * * * *",
            "1- * * * *",
            "1,,2 * * * *",
        ] {
            assert!(
                parse_cron_expression(expr).is_err(),
                "{expr:?} must not parse"
            );
        }
    }

    #[test]
    fn parser_normalizes_and_reports_field_count() {
        let err = parse_cron_expression("1 2 3 4 5 6").unwrap_err();
        assert!(err.contains("exactly 5 fields"), "{err}");
        assert!(err.contains("got 6"), "{err}");
    }

    #[test]
    fn day_of_month_and_day_of_week_are_an_or() {
        // The 13th and every Friday, like Vixie cron.
        let p = parse("0 0 13 * 5");
        assert!(!p.dom_wildcard && !p.dow_wildcard);
        let fri_the_13th = |y, m, d, h, min| {
            let naive = NaiveDateTime::new(
                chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap(),
                chrono::NaiveTime::from_hms_opt(h, min, 0).unwrap(),
            );
            day_matches(&p, &naive)
        };
        assert!(
            fri_the_13th(2026, 11, 13, 12, 0),
            "Friday the 13th matches both"
        );
        assert!(fri_the_13th(2026, 3, 13, 12, 0), "a plain 13th matches");
        assert!(fri_the_13th(2026, 11, 6, 12, 0), "a plain Friday matches");
        assert!(!fri_the_13th(2026, 11, 12, 12, 0), "neither matches");
    }

    #[test]
    fn next_run_steps_minute_boundaries() {
        let p = parse("*/5 * * * *");
        let from = millis(2026, 10, 13, 10, 7);
        assert_eq!(
            compute_next_cron_run(&p, from),
            Some(millis(2026, 10, 13, 10, 10))
        );
        // Strictly after: a fire exactly at `from` does not count.
        let at = millis(2026, 10, 13, 10, 10);
        assert_eq!(
            compute_next_cron_run(&p, at),
            Some(millis(2026, 10, 13, 10, 15))
        );
    }

    #[test]
    fn next_run_walks_to_the_next_matching_day() {
        let p = parse("0 9 * * 1-5");
        // 2026-10-13 is a Tuesday.
        let from = millis(2026, 10, 13, 10, 0);
        assert_eq!(
            compute_next_cron_run(&p, from),
            Some(millis(2026, 10, 14, 9, 0))
        );
        let friday = millis(2026, 10, 16, 10, 0);
        assert_eq!(
            compute_next_cron_run(&p, friday),
            Some(millis(2026, 10, 19, 9, 0))
        );
    }

    #[test]
    fn february_31_never_fires_within_five_years() {
        let p = parse("0 0 31 2 *");
        let from = millis(2026, 10, 13, 0, 0);
        assert!(!has_fire_within_years(&p, 5, from));
        assert_eq!(compute_next_cron_run(&p, from), None);
    }

    #[test]
    fn pinned_date_fires_and_repeats_yearly() {
        let p = parse("30 14 28 2 *");
        let from = millis(2026, 10, 13, 0, 0);
        assert_eq!(
            compute_next_cron_run(&p, from),
            Some(millis(2027, 2, 28, 14, 30))
        );
    }

    #[test]
    fn human_schedule_renders_common_shapes() {
        assert_eq!(cron_to_human(&parse("* * * * *")), "every minute");
        assert_eq!(cron_to_human(&parse("*/5 * * * *")), "every 5 minutes");
        assert_eq!(
            cron_to_human(&parse("7 * * * *")),
            "at minute 7 of every hour"
        );
        assert_eq!(
            cron_to_human(&parse("0 */3 * * *")),
            "every 3 hours at minute 00"
        );
        assert_eq!(cron_to_human(&parse("0 9 * * *")), "at 09:00 every day");
        assert_eq!(
            cron_to_human(&parse("57 8 * * 1-5")),
            "at 08:57 on weekdays"
        );
        assert_eq!(cron_to_human(&parse("0 9 * * 0,6")), "at 09:00 on weekends");
        assert_eq!(
            cron_to_human(&parse("0 9 * * 1,3")),
            "at 09:00 on Monday, Wednesday"
        );
        assert_eq!(
            cron_to_human(&parse("30 14 28 2 *")),
            "at 14:30 on day 28 of February"
        );
        assert_eq!(cron_to_human(&parse("1,2 3 4 5 6")), "1,2 3 4 5 6");
    }

    #[test]
    fn ids_are_eight_hex_characters() {
        let cron = CronManager::new();
        for _ in 0..20 {
            let t = cron.add_at("* * * * *", "p", true, 0).unwrap();
            assert_eq!(t.id.len(), 8);
            assert!(t.id.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn jitter_is_deterministic_and_within_bounds() {
        let p = parse("*/5 * * * *");
        let t = task("*/5 * * * *", "0f0f0f0f", 0);
        let ideal = millis(2026, 10, 13, 10, 10);
        let jittered = jittered_next_run_ms(&t, &p, ideal);
        // 10% of a 5-minute period is 30 s; the offset is deterministic.
        assert!(
            jittered >= ideal && jittered <= ideal + 30_000,
            "{jittered}"
        );
        assert_eq!(jittered, jittered_next_run_ms(&t, &p, ideal));
        // A 15-minute period caps at 15 minutes, not 90 s.
        let p = parse("0 9 * * *");
        let ideal = millis(2026, 10, 13, 9, 0);
        let jittered = jittered_next_run_ms(&t, &p, ideal);
        assert!(
            jittered >= ideal && jittered <= ideal + 15 * 60_000,
            "{jittered}"
        );
        assert!(
            jittered > ideal + 30_000,
            "a day-long period jitters beyond 30 s"
        );
    }

    #[test]
    fn one_shot_jitter_only_pulls_round_marks_earlier() {
        let mut t = task("0 9 * * *", "0f0f0f0f", 0);
        t.recurring = false;
        // :00 — pulled earlier by at most 90 s.
        let at_noon = millis(2026, 10, 13, 12, 0);
        let jittered = one_shot_jittered_next_run_ms(&t, at_noon);
        assert!(
            jittered <= at_noon && jittered >= at_noon - 90_000,
            "{jittered}"
        );
        // :30 — same.
        let at_half = millis(2026, 10, 13, 12, 30);
        let jittered = one_shot_jittered_next_run_ms(&t, at_half);
        assert!(
            jittered <= at_half && jittered >= at_half - 90_000,
            "{jittered}"
        );
        // Any other minute passes through unchanged.
        let at_seven = millis(2026, 10, 13, 12, 7);
        assert_eq!(one_shot_jittered_next_run_ms(&t, at_seven), at_seven);
        // Never before the task existed. This id's offset is ≈5.3 s, so a
        // created_at one second before the mark clamps back to the mark.
        t.created_at = at_noon - 1_000;
        assert_eq!(one_shot_jittered_next_run_ms(&t, at_noon), at_noon);
    }

    #[test]
    fn tick_is_quiet_before_the_first_fire() {
        let cron = CronManager::new();
        let now = millis(2026, 10, 13, 10, 0);
        cron.add_at("30 14 * * *", "look", true, now).unwrap();
        assert!(cron.tick_at(now).is_empty());
        assert_eq!(cron.task_count(), 1);
    }

    #[test]
    fn a_due_fire_returns_its_envelope_and_advances_the_cursor() {
        let cron = CronManager::new();
        let created = millis(2026, 10, 13, 10, 0);
        let t = cron.add_at("*/5 * * * *", "ping", true, created).unwrap();

        // 10:05 is the first ideal fire; its jitter lands within 30 s after
        // the mark, so 10:05:45 is past the first fire but before the second.
        let tick = millis(2026, 10, 13, 10, 5) + 45_000;
        let fires = cron.tick_at(tick);
        assert_eq!(fires.len(), 1);
        let envelope = fires[0].envelope();
        assert!(
            envelope.contains(&format!("jobId=\"{}\"", t.id)),
            "{envelope}"
        );
        assert!(envelope.contains("recurring=\"true\""));
        assert!(envelope.contains("coalescedCount=\"1\""));
        assert!(envelope.contains("stale=\"false\""));
        assert!(envelope.contains("<prompt>\nping\n</prompt>"));

        // The cursor moved: the same instant does not fire again.
        assert!(cron.tick_at(tick).is_empty());
    }

    #[test]
    fn slept_past_fires_coalesce_into_one_delivery() {
        let cron = CronManager::new();
        let created = millis(2026, 10, 13, 10, 0);
        cron.add_at("*/5 * * * *", "ping", true, created).unwrap();

        // Wake at 10:20:45: 10:05 … 10:20 are all past — each jittered by
        // less than 30 s, so the 10:20 fire (at most 10:20:30) is due too.
        let tick = millis(2026, 10, 13, 10, 20) + 45_000;
        let fires = cron.tick_at(tick);
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].coalesced_count, 4, "{:?}", fires[0]);
        // The cursor is at the last due fire; the next fire is still ahead.
        assert!(cron.tick_at(tick).is_empty());
        assert_eq!(cron.tick_at(millis(2026, 10, 13, 10, 25) + 45_000).len(), 1);
    }

    #[test]
    fn one_shot_fires_once_and_is_deleted() {
        let cron = CronManager::new();
        let created = millis(2026, 10, 13, 10, 0);
        cron.add_at("30 14 13 10 *", "once", false, created)
            .unwrap();

        assert!(cron.tick_at(millis(2026, 10, 13, 10, 0)).is_empty());
        let fires = cron.tick_at(millis(2026, 10, 13, 14, 30));
        assert_eq!(fires.len(), 1);
        assert!(!fires[0].recurring);
        assert_eq!(cron.task_count(), 0, "a spent one-shot is gone");
        assert!(cron.tick_at(millis(2026, 10, 13, 15, 0)).is_empty());
    }

    #[test]
    fn a_stale_task_gets_one_final_fire_and_is_deleted() {
        let cron = CronManager::new();
        // Created eight days ago.
        let created = millis(2026, 10, 13, 10, 0) - (8 * MS_PER_DAY);
        cron.add_at("* * * * *", "old", true, created).unwrap();

        let fires = cron.tick_at(millis(2026, 10, 13, 10, 0));
        assert_eq!(fires.len(), 1);
        assert!(fires[0].stale, "{:?}", fires[0]);
        assert_eq!(
            cron.task_count(),
            0,
            "the stale task is deleted after its final fire"
        );
    }

    #[test]
    fn the_cap_refuses_the_51st_task() {
        let cron = CronManager::new();
        for i in 0..MAX_TASKS {
            cron.add_at("* * * * *", &format!("{i}"), true, 0).unwrap();
        }
        let err = cron
            .add_at("* * * * *", "one too many", true, 0)
            .unwrap_err();
        assert!(
            err.contains("Cron job cap reached (max 50 per session)"),
            "{err}"
        );
        assert_eq!(cron.task_count(), MAX_TASKS);
    }

    #[test]
    fn tasks_round_trip_through_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl.cron.json");
        {
            let cron = CronManager::load(&path, false);
            cron.add_at("*/5 * * * *", "keep me", true, 1234).unwrap();
            cron.add_at("30 14 13 10 *", "once", false, 5678).unwrap();
        }
        let cron = CronManager::load(&path, true);
        let tasks = cron.list();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].prompt, "keep me");
        assert_eq!(tasks[0].created_at, 1234);
        assert!(tasks[0].recurring);
        assert!(!tasks[1].recurring);
        // A fresh (non-resumed) session starts empty and would overwrite the file.
        let cron = CronManager::load(&path, false);
        assert!(cron.list().is_empty());
    }

    #[test]
    fn a_damaged_sidecar_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl.cron.json");
        std::fs::write(&path, "not json").unwrap();
        let cron = CronManager::load(&path, true);
        assert!(cron.list().is_empty());
    }

    #[test]
    fn display_next_fire_is_post_jitter() {
        let cron = CronManager::new();
        let now = millis(2026, 10, 13, 0, 0);
        // A task created now, never fired: the next 9:00 is this morning's
        // (the display fire is computed from the cursor, i.e. creation time,
        // exactly like the due check).
        let t = cron.add_at("0 9 * * *", "dawn", true, now).unwrap();
        let next = cron.next_fire_for(&t, now).unwrap();
        let ideal = millis(2026, 10, 13, 9, 0);
        assert!(next >= ideal && next <= ideal + 15 * 60_000, "{next}");
    }

    #[test]
    fn local_iso_carries_a_numeric_offset() {
        let iso = format_local_iso_with_offset(millis(2026, 10, 13, 9, 57));
        let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}[+-]\d{2}:\d{2}$")
            .unwrap();
        assert!(re.is_match(&iso), "{iso}");
    }

    #[test]
    fn envelope_escapes_attributes_but_not_the_prompt() {
        let xml = render_cron_fire_xml("a&b\"", "0 0 * * *", true, 1, false, "p & <x>");
        assert!(xml.contains("jobId=\"a&amp;b&quot;\""), "{xml}");
        assert!(xml.contains("<prompt>\np & <x>\n</prompt>"), "{xml}");
        assert!(xml.contains("coalescedCount=\"1\""));
    }

    /// Either variable turns cron off, spelled as Rusta's other boolean
    /// variables are; any other value, or none, leaves it on.
    #[test]
    fn kill_switch_flags_parse_like_other_env_booleans() {
        let only = |name: &'static str, value: &'static str| {
            move |var: &str| (var == name).then(|| value.to_string())
        };
        assert!(!cron_disabled_with(|_| None));
        for value in ["1", "true", "YES", " on\n"] {
            for name in ["RUSTA_DISABLE_CRON", "KIMI_DISABLE_CRON"] {
                assert!(cron_disabled_with(only(name, value)), "{name}={value:?}");
            }
        }
        for value in ["", "0", "false", "off", "2"] {
            assert!(
                !cron_disabled_with(only("RUSTA_DISABLE_CRON", value)),
                "{value:?}"
            );
        }
    }

    fn fire(job_id: &str, coalesced_count: usize, stale: bool) -> CronFire {
        CronFire {
            job_id: job_id.to_string(),
            cron: "*/5 * * * *".to_string(),
            recurring: true,
            coalesced_count,
            stale,
            prompt: format!("prompt of {job_id}"),
        }
    }

    #[test]
    fn pending_fires_queue_and_drain() {
        let cron = CronManager::new();
        assert_eq!(cron.pending_fire_count(), 0);
        let due = fire("abcdef01", 1, false);
        assert_eq!(cron.queue_fires(vec![due.clone()]), vec!["abcdef01"]);
        assert!(cron.queue_fires(vec![]).is_empty());
        assert_eq!(cron.pending_fire_count(), 1);
        assert_eq!(cron.drain_fire_markers(), vec![due.envelope()]);
        assert_eq!(cron.pending_fire_count(), 0);
    }

    /// A job that keeps coming due while the session sits idle is delivered
    /// once, its runs counted, instead of once per tick.
    #[test]
    fn fires_of_one_job_queued_over_three_ticks_give_one_envelope() {
        let cron = CronManager::new();
        let created = millis(2026, 10, 13, 10, 0);
        cron.add_at("*/5 * * * *", "ping", true, created).unwrap();
        for minute in [5, 10, 15] {
            // Each run's jitter lands within 30 s after its mark.
            let fires = cron.tick_at(millis(2026, 10, 13, 10, minute) + 45_000);
            assert_eq!(fires.len(), 1, "the {minute}-minute run is due");
            cron.queue_fires(fires);
        }
        let envelopes = cron.drain_fire_markers();
        assert_eq!(envelopes.len(), 1, "{envelopes:?}");
        assert!(
            envelopes[0].contains("coalescedCount=\"3\""),
            "{}",
            envelopes[0]
        );
    }

    /// Merged fires add their runs and keep the stale flag of either.
    #[test]
    fn a_merged_fire_counts_every_run_and_stays_stale() {
        let cron = CronManager::new();
        cron.queue_fires(vec![fire("a", 2, false)]);
        cron.queue_fires(vec![fire("a", 3, true)]);
        cron.queue_fires(vec![fire("a", 1, false)]);
        let envelopes = cron.drain_fire_markers();
        assert_eq!(envelopes, vec![fire("a", 6, true).envelope()]);
    }

    #[test]
    fn two_jobs_give_two_envelopes_in_the_order_they_came_due() {
        let cron = CronManager::new();
        cron.queue_fires(vec![fire("a", 1, false)]);
        cron.queue_fires(vec![fire("b", 1, false), fire("a", 1, false)]);
        assert_eq!(
            cron.drain_fire_markers(),
            vec![
                fire("a", 2, false).envelope(),
                fire("b", 1, false).envelope()
            ]
        );
    }

    /// A job is reported as newly due once, until it is delivered.
    #[test]
    fn each_job_is_announced_once_until_it_is_delivered() {
        let cron = CronManager::new();
        assert_eq!(cron.queue_fires(vec![fire("a", 1, false)]), vec!["a"]);
        assert!(cron.queue_fires(vec![fire("a", 1, false)]).is_empty());
        assert_eq!(
            cron.queue_fires(vec![fire("a", 1, false), fire("b", 1, false)]),
            vec!["b"]
        );
        cron.drain_fire_markers();
        assert_eq!(cron.queue_fires(vec![fire("a", 1, false)]), vec!["a"]);
    }
}
