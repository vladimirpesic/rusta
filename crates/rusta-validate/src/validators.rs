//! Validator execution and Reflexion feedback — development plan §6.7 (R8).
//!
//! The pieces, in agent-loop order:
//!
//! 1. [`ValidateConfig::run`] executes the configured commands in order
//!    (each a `sh -c` subprocess, stdin closed, wall-clock bounded,
//!    output capped) and yields an [`Outcome`].
//! 2. [`Outcome::feedback`] formats failures for the model: deduplicated
//!    lines, the window from first to last diagnostic, ≤
//!    [`FEEDBACK_MAX_LINES`] lines by construction, compile-error
//!    locations rewritten to clickable `path:line:col` form.
//! 3. [`Gate::assess`] routes the outcome — repair attempts are bounded at
//!    [`REPAIR_BOUND`], then the failure is surfaced to the user — and
//!    [`Verdict::event`] is the phase-machine event the loop fires: the
//!    literal Verifying exit-gate wiring.
//! 4. [`Report::session_event`] journals each run as the §6.10
//!    `ValidationRun` event.
//!
//! Nothing here ever aborts the agent: an unstartable, timing-out, or
//! garbage-printing validator becomes a failing report with an actionable
//! message (plan §6.11 — errors surfaced to the model must contain a remedy).

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rusta_core::{Event, PhaseEvent};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// The Reflexion repair bound — after this many failed repair rounds the
/// failure is surfaced to the user instead of being fed back (§6.7).
pub const REPAIR_BOUND: u32 = 3;

/// Hard cap for the model-facing validation feedback, in lines (§6.7).
pub const FEEDBACK_MAX_LINES: usize = 30;

/// Hard cap for a single feedback line, in characters (sanity — §6.1
/// truncation caps exist for every observation channel).
const FEEDBACK_LINE_WIDTH: usize = 240;

/// Stored output cap per stream, in bytes. Validators can print megabytes;
/// the feedback formatter needs at most the first and last diagnostics.
const OUTPUT_CAP_BYTES: usize = 64 * 1024;

/// Per-command wall-clock limit default, seconds. Generous on purpose:
/// `cargo test` may build before running.
const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// The zero-test capsule (§6.7, the SmallCTL lesson): validation ran green
/// but the suite executed zero tests, which proves nothing.
pub const ZERO_TEST_CAPSULE: &str = "Zero tests ran — green proved nothing: write at least one \
real test that fails without the change and passes with it, then re-run validation.";

/// Configuration misuse — the only error this crate returns (§6.11). All
/// subprocess failures are [`Report`]s, never errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A validator command line is not usable; the remedy is in the message.
    #[error("invalid validator command {command:?}: {cause}")]
    InvalidCommand {
        /// The offending command line.
        command: String,
        /// What is wrong with it.
        cause: String,
    },
}

/// The `[validate]` configuration table (plan §7) — commands run in order
/// after each applied edit batch, plus a per-command wall-clock limit.
///
/// An absent table deserializes to no validators, and validation is then
/// trivially green (§6.7: the gate is *configured* validators).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateConfig {
    /// Validator command lines, executed in listed order by the system
    /// shell. Later commands run even when earlier ones fail — the model
    /// should see everything that is broken at once.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Per-command wall-clock limit in seconds; a validator that exceeds
    /// it is killed and reported as a failure.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

impl Default for ValidateConfig {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

impl ValidateConfig {
    /// Rejects configuration the model could not act on anyway: blank
    /// command lines and a non-positive timeout. Called by [`Self::run`];
    /// also the M8 config-load check.
    pub fn check(&self) -> Result<(), Error> {
        if self.timeout_secs == 0 {
            return Err(Error::InvalidCommand {
                command: "<validate.timeout_secs>".to_owned(),
                cause: "must be at least 1 second".to_owned(),
            });
        }
        for command in &self.commands {
            if command.trim().is_empty() {
                return Err(Error::InvalidCommand {
                    command: command.clone(),
                    cause: "must be a non-empty shell command line".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Runs every configured validator in order and collects the reports.
    /// Validators run sequentially: deterministic ordering, and build-tool
    /// locks make parallel runs a lie anyway. Never returns `Err` for a
    /// failing command — the failure is the [`Outcome`].
    pub async fn run(&self, cwd: &Path) -> Result<Outcome, Error> {
        self.check()?;
        let timeout = Duration::from_secs(self.timeout_secs);
        let mut reports = Vec::with_capacity(self.commands.len());
        for command in &self.commands {
            reports.push(run_one(command, cwd, timeout).await);
        }
        Ok(Outcome { reports })
    }
}

/// Executes one validator command line through the system shell in `cwd`,
/// stdin closed so nothing can block on a terminal, killed at `timeout`
/// (`kill_on_drop`: the dropped wait future takes the child with it — no
/// zombies, no runaways).
async fn run_one(command: &str, cwd: &Path, timeout: Duration) -> Report {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let child = match Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(cause) => return Report::unstartable(command, timeout, cause),
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Report::finished(
            command,
            output.stdout,
            output.stderr,
            exit_code(&output.status),
            timeout,
        ),
        Ok(Err(cause)) => Report::unstartable(command, timeout, cause),
        Err(_elapsed) => Report::timed_out(command, timeout),
    }
}

/// Exit code with the session contract's Unix convention: negative means
/// killed by signal (plan §6.10).
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        -status.signal().unwrap_or(9)
    }
    #[cfg(not(unix))]
    {
        -9
    }
}

/// One validator run: command, exit, and capped combined output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The command line as configured.
    pub command: String,
    /// Process exit code — negative ⇒ killed by signal (Unix convention);
    /// 127 ⇒ the command could not even start.
    pub exit: i32,
    /// Combined `stdout` + `stderr`, each stream capped at
    /// `OUTPUT_CAP_BYTES / 2` bytes with a truncation marker (§6.1 style).
    pub output: String,
    /// The wall-clock limit killed the process.
    pub timed_out: bool,
    /// The per-command limit that was in force, seconds (for summaries).
    pub limit_secs: u64,
}

impl Report {
    /// Green iff the process exited 0 on its own.
    pub fn passed(&self) -> bool {
        self.exit == 0 && !self.timed_out
    }

    /// The test suite ran zero tests (§6.7 zero-test detection).
    pub fn zero_tests(&self) -> bool {
        zero_tests(&self.output)
    }

    /// One-line summary for the §6.10 `ValidationRun` session event.
    pub fn summary(&self) -> String {
        let mut summary = if self.timed_out {
            format!("timed out after {}s (killed)", self.limit_secs)
        } else if self.passed() {
            "ok".to_owned()
        } else {
            match first_diagnostic(&self.output) {
                Some(line) => format!("exit {} · {}", self.exit, clip_chars(&line, 100)),
                None => format!("exit {} · (no output)", self.exit),
            }
        };
        if self.zero_tests() {
            summary.push_str(" · 0 tests ran");
        }
        summary
    }

    /// The §6.10 `ValidationRun` event for this run.
    pub fn session_event(&self) -> Event {
        Event::ValidationRun {
            command: self.command.clone(),
            exit: self.exit,
            summary: self.summary(),
        }
    }

    /// Feedback header: `$ <command> (exit N)` / `(timed out …)`.
    fn header_line(&self) -> String {
        if self.timed_out {
            format!("$ {} (timed out after {}s)", self.command, self.limit_secs)
        } else {
            format!("$ {} (exit {})", self.command, self.exit)
        }
    }

    /// The first-to-last-diagnostic window of the output, deduplicated and
    /// trimmed to `budget` lines. When the span does not fit, the head and
    /// tail split the budget and one elision line carries the count — the
    /// model always sees the first error, the last error, and how much it
    /// is not seeing.
    fn window(&self, budget: usize) -> Vec<String> {
        if budget == 0 {
            return Vec::new();
        }
        let lines = deduped_lines(&self.output);
        if lines.is_empty() {
            return vec!["(no output)".to_owned()];
        }
        let anchors: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| is_diagnostic(line).then_some(index))
            .collect();
        let (lo, hi) = match (anchors.first(), anchors.last()) {
            (Some(&first), Some(&last)) => (first, last),
            // Nothing diagnosable in the output — fall back to the first
            // and last line so the model still sees something actionable.
            _ => (0, lines.len() - 1),
        };
        let span = hi - lo + 1;
        if span <= budget {
            lines[lo..=hi].to_vec()
        } else {
            let keep = budget - 1;
            let head = keep.div_ceil(2);
            let tail = keep - head;
            let mut window = lines[lo..lo + head].to_vec();
            window.push(format!("… {} lines elided (first→last error)", span - keep));
            window.extend_from_slice(&lines[hi + 1 - tail..=hi]);
            window
        }
    }

    /// Assembles a finished report: both streams capped, combined, and
    /// marked when truncation happened.
    fn finished(
        command: &str,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit: i32,
        timeout: Duration,
    ) -> Self {
        let cap = OUTPUT_CAP_BYTES / 2;
        let (stdout, stdout_cut) = (truncate(&stdout, cap), stdout.len() > cap);
        let (stderr, stderr_cut) = (truncate(&stderr, cap), stderr.len() > cap);
        let stdout = String::from_utf8_lossy(stdout);
        let stderr = String::from_utf8_lossy(stderr);
        let mut output = String::with_capacity(stdout.len() + stderr.len() + 1);
        output.push_str(&stdout);
        if !stdout.is_empty() && !stderr.is_empty() {
            output.push('\n');
        }
        output.push_str(&stderr);
        if stdout_cut || stderr_cut {
            output.push_str(&format!("\n… [truncated at {cap} bytes per stream]"));
        }
        Self {
            command: command.to_owned(),
            exit,
            output: output.trim_end().to_owned(),
            timed_out: false,
            limit_secs: timeout.as_secs(),
        }
    }

    /// The shell could not start the command at all (missing `sh`,
    /// missing working directory): exit 127, the conventional
    /// "command not found" code, with the cause spelled out.
    fn unstartable(command: &str, timeout: Duration, cause: std::io::Error) -> Self {
        Self {
            command: command.to_owned(),
            exit: 127,
            output: format!("validator failed to start: {cause}"),
            timed_out: false,
            limit_secs: timeout.as_secs(),
        }
    }

    /// The wall-clock limit fired; `kill_on_drop` reaped the process.
    fn timed_out(command: &str, timeout: Duration) -> Self {
        Self {
            command: command.to_owned(),
            exit: -9,
            output: format!(
                "validator exceeded its {}s limit and was killed; fix or raise [validate].timeout_secs",
                timeout.as_secs()
            ),
            timed_out: true,
            limit_secs: timeout.as_secs(),
        }
    }
}

/// `bytes` capped at `cap` bytes, cut on a char boundary when possible so
/// lossy conversion sees whole chars.
fn truncate(bytes: &[u8], cap: usize) -> &[u8] {
    if bytes.len() <= cap {
        return bytes;
    }
    let mut end = cap;
    while end > 0 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    &bytes[..end]
}

/// All validator reports for one validation round.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// One report per configured command, in order.
    pub reports: Vec<Report>,
}

impl Outcome {
    /// Every configured validator exited 0 — the §6.7 exit-gate condition.
    pub fn green(&self) -> bool {
        self.reports.iter().all(Report::passed)
    }

    /// The failing reports, in configured order.
    pub fn failures(&self) -> Vec<&Report> {
        self.reports
            .iter()
            .filter(|report| !report.passed())
            .collect()
    }

    /// The first report whose suite ran zero tests, if any (§6.7).
    pub fn zero_tests(&self) -> Option<&Report> {
        self.reports.iter().find(|report| report.zero_tests())
    }

    /// Model-facing feedback (§6.7): a green one-liner, or `validation
    /// failed` plus each failing command with its first-to-last-diagnostic
    /// window — deduplicated, clickable, and ≤ [`FEEDBACK_MAX_LINES`]
    /// lines by construction. The zero-test capsule rides along whenever
    /// the round ran zero tests.
    pub fn feedback(&self) -> String {
        if self.reports.is_empty() {
            return "no validators configured — validation skipped".to_owned();
        }
        let mut lines: Vec<String> = Vec::with_capacity(FEEDBACK_MAX_LINES);
        let failures = self.failures();
        if failures.is_empty() {
            lines.push(format!(
                "all validators green ({}/{})",
                self.reports.len(),
                self.reports.len()
            ));
        } else {
            lines.push(format!(
                "validation failed ({}/{} commands failed)",
                failures.len(),
                self.reports.len()
            ));
            for (index, report) in failures.iter().enumerate() {
                // Reserve one line per header still to come, this one included.
                let headers_left = failures.len() - index;
                let budget = FEEDBACK_MAX_LINES
                    .saturating_sub(lines.len())
                    .saturating_sub(headers_left - 1);
                if budget == 0 {
                    break;
                }
                lines.push(report.header_line());
                lines.extend(report.window(budget - 1));
            }
        }
        if self.zero_tests().is_some() {
            lines.push(ZERO_TEST_CAPSULE.to_owned());
        }
        // Unreachable by construction; kept as a hard guarantee.
        lines.truncate(FEEDBACK_MAX_LINES);
        lines.join("\n")
    }
}

/// Output lines, normalized (clickable locations, width-capped),
/// blank-free, and deduplicated keeping first occurrences (§6.7 "deduped").
fn deduped_lines(output: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut lines = Vec::new();
    for raw in output.lines() {
        let line = normalize_line(raw);
        if !line.is_empty() && seen.insert(line.clone()) {
            lines.push(line);
        }
    }
    lines
}

/// Trailing-trimmed, width-capped line; rustc's ` --> src/lib.rs:12:23`
/// location markers become the clickable `src/lib.rs:12:23` (§6.7 —
/// compile errors map back to file:line).
fn normalize_line(raw: &str) -> String {
    let trimmed = raw.trim_end();
    let stripped = trimmed.trim_start();
    let body = match stripped.strip_prefix("--> ") {
        Some(location) => location.trim(),
        None => trimmed,
    };
    if body.chars().count() > FEEDBACK_LINE_WIDTH {
        let mut capped: String = body.chars().take(FEEDBACK_LINE_WIDTH).collect();
        capped.push('…');
        capped
    } else {
        body.to_owned()
    }
}

/// Whether a line is a diagnostic anchor — an error, failure, panic, test
/// tally, or a `path:line` location. Anchors delimit the first→last window.
fn is_diagnostic(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("error")
        || lower.contains("failed")
        || lower.contains("panicked")
        || lower.starts_with("test result:")
        || is_location(line)
}

/// `path:line` or `path:line:col` — a clickable compile-error location.
fn is_location(line: &str) -> bool {
    let Some((path, rest)) = line.split_once(':') else {
        return false;
    };
    !path.is_empty()
        && !path.contains(char::is_whitespace)
        && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Zero-test detection (§6.7). Two detectors:
///
/// * libtest tally — at least one `test result:` line, and the sum of
///   passed + failed + ignored + measured across all of them is zero
///   (ignored counts: an ignored test exists, it just did not run).
///   Filtered-out counts do not: nothing was verified.
/// * runner literals — a `0 tests` token pair (jest/vitest style) or
///   pytest/go phrasing (`no tests ran`, `no test files`).
fn zero_tests(output: &str) -> bool {
    let mut tallies = 0u64;
    let mut ran = 0u64;
    for line in output.lines() {
        let Some(rest) = line.trim().strip_prefix("test result:") else {
            continue;
        };
        tallies += 1;
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        for pair in tokens.windows(2) {
            if let Ok(count) = pair[0].parse::<u64>() {
                let counter = pair[1].trim_end_matches(';');
                if matches!(counter, "passed" | "failed" | "ignored" | "measured") {
                    ran += count;
                }
            }
        }
    }
    if tallies > 0 {
        return ran == 0;
    }
    output.lines().any(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        tokens
            .windows(2)
            .any(|pair| pair[0] == "0" && pair[1].starts_with("tests"))
            || line.contains("no tests ran")
            || line.contains("no test files")
    })
}

/// The first normalized diagnostic line, falling back to the first
/// non-empty line — for one-line summaries.
fn first_diagnostic(output: &str) -> Option<String> {
    let lines = deduped_lines(output);
    lines
        .iter()
        .find(|line| is_diagnostic(line))
        .or_else(|| lines.first())
        .cloned()
}

/// Character clip with an ellipsis marker.
fn clip_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(cap).collect();
    clipped.push('…');
    clipped
}

/// The Verifying exit gate (§6.7): how a validation round routes, plus the
/// phase-machine event to fire. Tracks the Reflexion repair budget — the
/// model gets the first [`REPAIR_BOUND`] failure rounds as observations;
/// after that the same failure is surfaced to the user instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    repairs_used: u32,
    repair_bound: u32,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            repairs_used: 0,
            repair_bound: REPAIR_BOUND,
        }
    }
}

impl Gate {
    /// A fresh gate with the §6.7 repair bound.
    pub fn new() -> Self {
        Self::default()
    }

    /// Repair rounds consumed so far in this task.
    pub fn repairs_used(&self) -> u32 {
        self.repairs_used
    }

    /// Repair rounds still available before surfacing.
    pub fn repairs_left(&self) -> u32 {
        self.repair_bound.saturating_sub(self.repairs_used)
    }

    /// A fresh budget — after a green round, a user interrupt, or a new
    /// task; every user request earns its own repair bound.
    pub fn reset(&mut self) {
        self.repairs_used = 0;
    }

    /// Routes one validation round:
    ///
    /// * green → [`Verdict::Pass`], with the zero-test capsule as `note`
    ///   when a suite ran zero tests (the gate still opens — §6.7 keeps
    ///   the gate at "all validators green"; the capsule prompts real
    ///   tests on the model's next turn);
    /// * red with budget left → [`Verdict::Repair`] — feed `feedback` back
    ///   as an observation before the user sees anything;
    /// * red and exhausted → [`Verdict::Surface`] — show the user.
    pub fn assess(&mut self, outcome: &Outcome) -> Verdict {
        if outcome.green() {
            let note = outcome.zero_tests().map(|_| ZERO_TEST_CAPSULE);
            return Verdict::Pass { note };
        }
        if self.repairs_used < self.repair_bound {
            self.repairs_used += 1;
            Verdict::Repair {
                feedback: outcome.feedback(),
                attempts_left: self.repair_bound - self.repairs_used,
            }
        } else {
            Verdict::Surface {
                feedback: outcome.feedback(),
            }
        }
    }
}

/// How one validation round routes — the agent loop's whole decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// All validators green: fire [`PhaseEvent::ValidationPassed`]
    /// (Verifying → Exploring). `note`, when set, is appended to the
    /// wrap-up for the user *and* the model's next prompt (zero tests).
    Pass {
        /// The zero-test capsule, when a suite ran zero tests.
        note: Option<&'static str>,
    },
    /// Red, budget remaining: fire [`PhaseEvent::ValidationFailed`]
    /// (Verifying → Editing) and feed `feedback` to the model as its next
    /// observation — the Reflexion repair attempt.
    Repair {
        /// Model-facing feedback (≤ 30 lines).
        feedback: String,
        /// Repair rounds left after this one.
        attempts_left: u32,
    },
    /// Red, budget exhausted: still fire [`PhaseEvent::ValidationFailed`],
    /// but stop the loop and show `feedback` to the user (§6.7).
    Surface {
        /// Feedback to display to the user.
        feedback: String,
    },
}

impl Verdict {
    /// The phase-machine event this verdict wires into the Verifying exit
    /// gate (plan §6.4 table): green passes, every failure returns to
    /// Editing — repairs and surfaces differ only in audience.
    pub fn event(&self) -> PhaseEvent {
        match self {
            Verdict::Pass { .. } => PhaseEvent::ValidationPassed,
            Verdict::Repair { .. } | Verdict::Surface { .. } => PhaseEvent::ValidationFailed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(command: &str, exit: i32, output: &str) -> Report {
        Report {
            command: command.to_owned(),
            exit,
            output: output.to_owned(),
            timed_out: false,
            limit_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    fn outcome(reports: &[Report]) -> Outcome {
        Outcome {
            reports: reports.to_vec(),
        }
    }

    #[test]
    fn config_defaults_and_serde_round_trip() {
        let empty: ValidateConfig = serde_json::from_str("{}").expect("empty table");
        assert_eq!(empty, ValidateConfig::default());
        assert!(empty.commands.is_empty());
        let loaded: ValidateConfig =
            serde_json::from_str(r#"{"commands": ["cargo test --workspace"]}"#).expect("table");
        assert_eq!(loaded.commands, ["cargo test --workspace"]);
        assert_eq!(loaded.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert!(loaded.check().is_ok());
    }

    #[test]
    fn config_check_rejects_misuse() {
        let blank = ValidateConfig {
            commands: vec!["   ".to_owned()],
            ..ValidateConfig::default()
        };
        assert!(matches!(blank.check(), Err(Error::InvalidCommand { .. })));
        let zero_timeout = ValidateConfig {
            timeout_secs: 0,
            ..ValidateConfig::default()
        };
        assert!(zero_timeout.check().is_err());
    }

    #[test]
    fn feedback_green_one_liner_and_zero_test_capsule() {
        assert_eq!(
            outcome(&[]).feedback(),
            "no validators configured — validation skipped"
        );
        let green = outcome(&[report(
            "cargo test",
            0,
            "test result: ok. 3 passed; 0 failed;",
        )]);
        assert_eq!(green.feedback(), "all validators green (1/1)");
        // Green but zero tests: the capsule rides along (§6.7).
        let hollow = outcome(&[report(
            "cargo test",
            0,
            "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured;",
        )]);
        let feedback = hollow.feedback();
        assert!(feedback.starts_with("all validators green (1/1)"));
        assert!(feedback.contains(ZERO_TEST_CAPSULE));
    }

    #[test]
    fn feedback_hard_caps_at_30_lines_with_first_and_last_error() {
        let errors: Vec<String> = (0..1000)
            .map(|i| format!("error[E{i:04}]: broken thing {i}"))
            .collect();
        let failed = outcome(&[report("cargo check", 1, &errors.join("\n"))]);
        let feedback = failed.feedback();
        let count = feedback.lines().count();
        assert_eq!(count, FEEDBACK_MAX_LINES, "feedback must be ≤ 30 lines");
        assert!(
            feedback.contains("error[E0000]: broken thing 0"),
            "first error kept: {feedback}"
        );
        assert!(
            feedback.contains("error[E0999]: broken thing 999"),
            "last error kept"
        );
        assert!(feedback.contains("973 lines elided (first→last error)"));
        assert!(feedback.contains("$ cargo check (exit 1)"));
        assert!(feedback.contains("validation failed"));
    }

    #[test]
    fn feedback_dedups_repeated_lines() {
        let failed = outcome(&[report(
            "cargo test",
            1,
            "error: x\nerror: x\nerror: x\nerror: y",
        )]);
        let feedback = failed.feedback();
        assert_eq!(feedback.matches("error: x").count(), 1);
        assert!(feedback.contains("error: y"));
    }

    #[test]
    fn feedback_makes_locations_clickable() {
        let failed = outcome(&[report(
            "cargo check",
            1,
            "error[E0308]: mismatched types\n  --> src/lib.rs:12:23\n   |\nerror: aborting due to 1 previous error",
        )]);
        let feedback = failed.feedback();
        assert!(feedback.contains("src/lib.rs:12:23"), "clickable location");
        assert!(!feedback.contains("-->"), "marker rewritten");
    }

    #[test]
    fn feedback_falls_back_without_diagnostics() {
        let noisy = outcome(&[report("weird", 1, "some noise\nmiddle\nrandom tail")]);
        assert!(noisy.feedback().contains("some noise"));
        assert!(noisy.feedback().contains("random tail"));
        let silent = outcome(&[report("weird", 1, "")]);
        assert!(silent.feedback().contains("(no output)"));
    }

    #[test]
    fn zero_tests_detector_matrix() {
        // libtest tally: all-zero sums trip.
        assert!(zero_tests(
            "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured;"
        ));
        // Real tests ran: no trip, doctest sections included.
        assert!(!zero_tests(
            "test result: ok. 3 passed; 0 failed; 0 ignored;\ntest result: ok. 0 passed; 0 failed; 0 ignored;"
        ));
        // Ignored tests exist: no trip.
        assert!(!zero_tests(
            "test result: ok. 0 passed; 0 failed; 1 ignored;"
        ));
        // Everything filtered out: nothing verified — trips.
        assert!(zero_tests(
            "test result: ok. 0 passed; 0 failed; 0 filtered out;"
        ));
        // Compile errors are not a test tally: no trip.
        assert!(!zero_tests(
            "error[E0308]: mismatched types --> src/lib.rs:1:1"
        ));
        // Boundary: "10 tests" must NOT trip the `0 tests` literal.
        assert!(!zero_tests("Tests: 10 tests, 10 passed, 0 failed"));
        // Other runners' zero-test phrasing trips.
        assert!(zero_tests("Tests: 0 tests, 0 suites"));
        assert!(zero_tests("no tests ran in 0.01s"));
        assert!(zero_tests("?   pkg [no test files]"));
        assert!(!zero_tests(""));
    }

    #[test]
    fn gate_bounded_repairs_then_surface() {
        let mut gate = Gate::new();
        let red = outcome(&[report("cargo test", 1, "error: boom")]);
        for expected_left in [2, 1, 0] {
            match gate.assess(&red) {
                Verdict::Repair {
                    attempts_left,
                    feedback,
                } => {
                    assert_eq!(attempts_left, expected_left);
                    assert!(feedback.contains("error: boom"));
                }
                other => panic!("expected repair, got {other:?}"),
            }
        }
        assert!(matches!(gate.assess(&red), Verdict::Surface { .. }));
        assert_eq!(gate.repairs_used(), REPAIR_BOUND);
        gate.reset();
        assert!(matches!(gate.assess(&red), Verdict::Repair { .. }));
        assert_eq!(gate.repairs_left(), REPAIR_BOUND - 1);
    }

    #[test]
    fn gate_wires_the_verifying_exit_gate() {
        let mut gate = Gate::new();
        let green = outcome(&[report("cargo check", 0, "ok")]);
        let Verdict::Pass { note } = gate.assess(&green) else {
            panic!("expected pass");
        };
        assert_eq!(note, None);
        let hollow = outcome(&[report("cargo test", 0, "test result: ok. 0 passed;")]);
        let Verdict::Pass { note } = gate.assess(&hollow) else {
            panic!("expected pass");
        };
        assert_eq!(note, Some(ZERO_TEST_CAPSULE));
        let red = outcome(&[report("cargo test", 1, "error: boom")]);
        assert_eq!(gate.assess(&red).event(), PhaseEvent::ValidationFailed);
        gate.reset();
        // The events actually move the phase machine (§6.4 table).
        let mut machine = rusta_core::Machine::resume_at(rusta_core::State::Verifying);
        machine
            .fire(gate.assess(&green).event())
            .expect("verifying exits on green");
        assert_eq!(machine.state(), rusta_core::State::Exploring);
        let mut machine = rusta_core::Machine::resume_at(rusta_core::State::Verifying);
        machine
            .fire(gate.assess(&red).event())
            .expect("verifying regresses on red");
        assert_eq!(machine.state(), rusta_core::State::Editing);
    }

    #[test]
    fn summaries_and_session_event() {
        assert_eq!(report("cargo check", 0, "ok").summary(), "ok");
        assert_eq!(
            report("cargo test", 1, "error: boom").summary(),
            "exit 1 · error: boom"
        );
        assert_eq!(
            report("cargo test", 3, "").summary(),
            "exit 3 · (no output)"
        );
        let timed = Report {
            timed_out: true,
            exit: -9,
            ..report("cargo test", 0, "")
        };
        assert_eq!(timed.summary(), "timed out after 600s (killed)");
        let hollow = report("cargo test", 0, "test result: ok. 0 passed;");
        assert_eq!(hollow.summary(), "ok · 0 tests ran");
        let Event::ValidationRun {
            command,
            exit,
            summary,
        } = hollow.session_event()
        else {
            panic!("wrong event");
        };
        assert_eq!((command.as_str(), exit), ("cargo test", 0));
        assert_eq!(summary, "ok · 0 tests ran");
    }

    #[test]
    fn truncate_cuts_on_char_boundaries() {
        // 4-byte emoji split at byte 5 lands mid-char; the cut backs up.
        let text = "ab😀cd".as_bytes();
        assert_eq!(truncate(text, 5), b"ab");
        assert_eq!(truncate(text, 2), b"ab");
        assert_eq!(truncate(text, 100), text);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_collects_green_and_red_reports() {
        let config = ValidateConfig {
            commands: vec![
                "printf 'all fine\\n'".to_owned(),
                "printf 'error: boom\\n' >&2; exit 7".to_owned(),
            ],
            ..ValidateConfig::default()
        };
        let outcome = config.run(std::path::Path::new(".")).await.expect("runs");
        assert_eq!(outcome.reports.len(), 2);
        assert_eq!(outcome.reports[0].exit, 0);
        assert!(outcome.reports[0].passed());
        assert_eq!(outcome.reports[1].exit, 7);
        assert!(!outcome.green());
        let feedback = outcome.feedback();
        // Only the failing command appears; the green one is summarized in
        // the header count.
        assert!(!feedback.contains("all fine"));
        assert!(feedback.contains("error: boom"));
        assert!(feedback.contains("$ printf 'error: boom\\n' >&2; exit 7 (exit 7)"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_times_out_and_kills_the_validator() {
        let config = ValidateConfig {
            commands: vec!["sleep 30".to_owned()],
            timeout_secs: 1,
        };
        let outcome = config.run(std::path::Path::new(".")).await.expect("runs");
        let report = &outcome.reports[0];
        assert!(report.timed_out);
        assert_eq!(report.exit, -9);
        assert!(!report.passed());
        assert!(report.summary().contains("timed out after 1s"));
        assert!(outcome.feedback().contains("timed out after 1s"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_reports_unstartable_commands() {
        let config = ValidateConfig {
            commands: vec!["true".to_owned()],
            ..ValidateConfig::default()
        };
        let outcome = config
            .run(std::path::Path::new("/nonexistent/rusta/validator/cwd"))
            .await
            .expect("runs");
        let report = &outcome.reports[0];
        assert_eq!(report.exit, 127);
        assert!(report.output.contains("validator failed to start"));
        assert!(!outcome.green());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_caps_huge_output() {
        let config = ValidateConfig {
            commands: vec!["head -c 200000 /dev/zero | tr '\\0' a".to_owned()],
            ..ValidateConfig::default()
        };
        let outcome = config.run(std::path::Path::new(".")).await.expect("runs");
        let report = &outcome.reports[0];
        assert!(report.passed());
        assert!(report.output.len() < OUTPUT_CAP_BYTES + 128);
        assert!(report.output.contains("[truncated at"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_rejects_misconfigured_tables_before_spawning() {
        let config = ValidateConfig {
            commands: vec![String::new()],
            ..ValidateConfig::default()
        };
        assert!(config.run(std::path::Path::new(".")).await.is_err());
    }
}
