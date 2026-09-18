//! The dispatch task set — §6.8 (R9).
//!
//! `dispatch` input is either a single `task` string or `tasks` — an array
//! of `{label, task}` objects, at most four, each with a distinct label.
//! Tasks are independent (a flat parallel fan-out — the "DAG" of the ADR's
//! crate sketch): every actor starts immediately and the joined reports come
//! back in input order, so the observation is deterministic regardless of
//! completion order.

use std::sync::Arc;

use rusta_llm::{Backend, BackendKind};
use serde_json::Value;

use crate::actor::{Report, RunTool, run_actor};

/// Maximum parallel tasks per dispatch call (§6.8).
pub const MAX_TASKS: usize = 4;

/// Label used when the caller passes the single-`task` form.
pub const SINGLE_TASK_LABEL: &str = "research";

/// One research task: a distinct label plus the task text (the `task` key
/// of §6.8's `{label, task}` input shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// Distinct, non-empty label (quoted in the returned report header).
    pub label: String,
    /// The research brief.
    pub brief: String,
}

/// Why a dispatch input was rejected before any actor spawned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskSetError {
    /// Input had neither `task` nor `tasks`, or both, or wrong JSON types.
    #[error(
        "dispatch input must be either {{\"task\": \"...\"}} or {{\"tasks\": [{{\"label\": \"...\", \"task\": \"...\"}}, ...]}}"
    )]
    Malformed,
    /// A `tasks` entry's `task` text was missing, empty, or not a string.
    #[error("every task needs a non-empty \"task\" string")]
    EmptyTask,
    /// A `tasks` entry's `label` was missing, empty, or not a string.
    #[error("every task needs a non-empty \"label\" string")]
    EmptyLabel,
    /// More than [`MAX_TASKS`] tasks.
    #[error("too many tasks: {0} (max {MAX_TASKS})")]
    TooMany(usize),
    /// Two tasks shared a label (reports must be unambiguous).
    #[error("duplicate label {0:?}: each task needs a distinct label")]
    DuplicateLabel(String),
}

/// A validated task set: 1..=[`MAX_TASKS`] tasks with distinct labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSet {
    tasks: Vec<Task>,
}

impl TaskSet {
    /// The single-task form; labeled [`SINGLE_TASK_LABEL`].
    pub fn single(brief: impl Into<String>) -> Self {
        Self {
            tasks: vec![Task {
                label: SINGLE_TASK_LABEL.to_owned(),
                brief: brief.into(),
            }],
        }
    }

    /// Parse the dispatch tool input (§6.8): `{"task": "..."}` or
    /// `{"tasks": [{"label": ..., "task": ...}, ...]}`.
    pub fn from_input(input: &Value) -> Result<Self, TaskSetError> {
        let obj = input.as_object().ok_or(TaskSetError::Malformed)?;
        match (obj.get("task"), obj.get("tasks")) {
            // A37: the array form rejects an empty or whitespace-only brief
            // (`from_items` below); the single-task form did not, so
            // `{"task": ""}` spawned a sub-coder with nothing to research —
            // six turns and a backend round trip to produce an empty report.
            (Some(Value::String(brief)), None) if !brief.trim().is_empty() => {
                Ok(Self::single(brief.trim()))
            }
            (Some(Value::String(_)), None) => Err(TaskSetError::EmptyTask),
            (None, Some(Value::Array(items))) => Self::from_items(items),
            _ => Err(TaskSetError::Malformed),
        }
    }

    /// Build from `{label, task}` JSON objects (the `tasks` array body).
    pub fn from_items(items: &[Value]) -> Result<Self, TaskSetError> {
        if items.is_empty() {
            return Err(TaskSetError::Malformed);
        }
        if items.len() > MAX_TASKS {
            return Err(TaskSetError::TooMany(items.len()));
        }
        let mut tasks = Vec::with_capacity(items.len());
        for item in items {
            let obj = item.as_object().ok_or(TaskSetError::Malformed)?;
            let label = obj
                .get("label")
                .and_then(Value::as_str)
                .filter(|l| !l.trim().is_empty())
                .ok_or(TaskSetError::EmptyLabel)?;
            let brief = obj
                .get("task")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
                .ok_or(TaskSetError::EmptyTask)?;
            tasks.push(Task {
                label: label.trim().to_owned(),
                brief: brief.to_owned(),
            });
        }
        let mut seen = Vec::with_capacity(tasks.len());
        for task in &tasks {
            if seen.contains(&task.label) {
                return Err(TaskSetError::DuplicateLabel(task.label.clone()));
            }
            seen.push(task.label.clone());
        }
        Ok(Self { tasks })
    }

    /// The tasks, in input order.
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }
}

/// How actor completions are scheduled (§6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    /// One actor per task, completions overlap (HTTP backend).
    Parallel,
    /// Completions funnel through one gate — the embedded backend's single
    /// inference thread. Correctness preserved; parallelism only on HTTP.
    Serialized,
}

impl ExecMode {
    /// The §6.8 mapping: embedded ⇒ serialized, HTTP ⇒ parallel.
    #[must_use]
    pub fn for_backend(kind: BackendKind) -> Self {
        match kind {
            BackendKind::Http => Self::Parallel,
            #[cfg(feature = "embedded")]
            BackendKind::Embedded => Self::Serialized,
        }
    }
}

/// Run every task to completion and join the reports in input order.
///
/// Actors run on spawned Tokio tasks; a panicked actor degrades to a failed
/// report (its label is kept — labels are recovered positionally), never an
/// aborted dispatch.
pub async fn run<R>(backend: Arc<Backend>, runner: R, mode: ExecMode, set: TaskSet) -> Vec<Report>
where
    R: RunTool + Clone,
{
    let gate = match mode {
        ExecMode::Parallel => None,
        ExecMode::Serialized => Some(Arc::new(tokio::sync::Mutex::new(()))),
    };
    let mut handles = Vec::with_capacity(set.tasks.len());
    for task in &set.tasks {
        let backend = Arc::clone(&backend);
        let runner = runner.clone();
        let gate = gate.clone();
        let task = task.clone();
        handles.push(tokio::spawn(async move {
            run_actor(backend, runner, gate, task).await
        }));
    }
    // Join in input order; a panicked actor keeps its (positional) label.
    let mut reports = Vec::with_capacity(handles.len());
    for (handle, task) in handles.into_iter().zip(set.tasks) {
        let report = match handle.await {
            Ok(report) => report,
            Err(err) => Report::failed(
                &task.label,
                &format!("sub-coder task failed before reporting: {err}"),
            ),
        };
        reports.push(report);
    }
    reports
}

/// Render joined reports with the §6.8 label format:
/// `SUB-CODER "label" REPORT:` followed by the report. ASCII quotes are
/// deliberate — the label must survive small-model round trips verbatim.
pub fn labeled(reports: &[Report]) -> String {
    let mut out = String::new();
    for report in reports {
        out.push_str(&format!(
            "SUB-CODER \"{}\" REPORT:\n{}\n\n",
            report.label,
            report.report.trim()
        ));
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn items(pairs: &[(&str, &str)]) -> Vec<Value> {
        pairs
            .iter()
            .map(|(label, task)| json!({"label": label, "task": task}))
            .collect()
    }

    #[test]
    fn single_task_form() {
        let set = TaskSet::from_input(&json!({"task": "find the config loader"})).expect("valid");
        assert_eq!(set.tasks().len(), 1);
        assert_eq!(set.tasks()[0].label, "research");
        assert_eq!(set.tasks()[0].brief, "find the config loader");
    }

    #[test]
    fn tasks_form_accepts_up_to_four_with_distinct_labels() {
        let set = TaskSet::from_items(&items(&[
            ("auth", "trace the auth flow"),
            ("db", "list migrations"),
            ("api", "map the routes"),
            ("tests", "find coverage gaps"),
        ]))
        .expect("valid");
        assert_eq!(set.tasks().len(), 4);
        let labels: Vec<&str> = set.tasks().iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, vec!["auth", "db", "api", "tests"]);

        let from_input = TaskSet::from_input(&json!({"tasks": items(&[
            ("auth", "a"), ("db", "b"),
        ])}))
        .expect("valid");
        assert_eq!(from_input.tasks().len(), 2);
    }

    #[test]
    fn rejects_more_than_four() {
        let err = TaskSet::from_items(&items(&[
            ("a", "1"),
            ("b", "2"),
            ("c", "3"),
            ("d", "4"),
            ("e", "5"),
        ]))
        .expect_err("too many");
        assert_eq!(err, TaskSetError::TooMany(5));
    }

    #[test]
    fn rejects_duplicate_labels_empty_labels_and_empty_tasks() {
        assert_eq!(
            TaskSet::from_items(&items(&[("dup", "1"), ("dup", "2")])).unwrap_err(),
            TaskSetError::DuplicateLabel("dup".into())
        );
        assert_eq!(
            TaskSet::from_items(&[json!({"label": "", "task": "x"})]).unwrap_err(),
            TaskSetError::EmptyLabel
        );
        assert_eq!(
            TaskSet::from_items(&[json!({"label": "x", "task": "  "})]).unwrap_err(),
            TaskSetError::EmptyTask
        );
        // An empty array is not "too many": that message read
        // "too many tasks: 0 (max 4)", which told the model the opposite of
        // what was wrong. Malformed names the expected shape instead.
        assert_eq!(
            TaskSet::from_items(&[]).unwrap_err(),
            TaskSetError::Malformed
        );
        assert_eq!(
            TaskSet::from_items(&vec![json!({"label": "a", "task": "t"}); 5]).unwrap_err(),
            TaskSetError::TooMany(5)
        );
    }

    #[test]
    fn rejects_malformed_inputs() {
        assert_eq!(
            TaskSet::from_input(&json!({})).unwrap_err(),
            TaskSetError::Malformed
        );
        assert_eq!(
            TaskSet::from_input(&json!({"task": "x", "tasks": []})).unwrap_err(),
            TaskSetError::Malformed
        );
        assert_eq!(
            TaskSet::from_input(&json!({"task": 7})).unwrap_err(),
            TaskSetError::Malformed
        );
        assert_eq!(
            TaskSet::from_input(&json!([1, 2])).unwrap_err(),
            TaskSetError::Malformed
        );
    }

    #[test]
    fn labeled_reports_use_the_plan_header() {
        let reports = vec![
            Report::failed("auth", "boom"),
            Report {
                label: "db".to_owned(),
                report: "migrations live in db/migrate".to_owned(),
                truncated: false,
                failed: false,
                transcript: Vec::new(),
            },
        ];
        let text = labeled(&reports);
        assert_eq!(
            text,
            "SUB-CODER \"auth\" REPORT:\nRESEARCH FAILED: boom\n\n\
             SUB-CODER \"db\" REPORT:\nmigrations live in db/migrate"
        );
    }
}

#[cfg(test)]
mod round8_regressions {
    use super::*;
    use serde_json::json;

    /// A37: the `tasks` array form rejects an empty brief; the single-task
    /// form did not, so `{"task": ""}` spawned a sub-coder with nothing to
    /// research — a full six-turn budget and a backend round trip to produce
    /// an empty report.
    #[test]
    fn an_empty_single_task_is_rejected_like_the_array_form() {
        for empty in ["", "   ", "\n\t "] {
            assert!(
                matches!(
                    TaskSet::from_input(&json!({ "task": empty })),
                    Err(TaskSetError::EmptyTask)
                ),
                "empty single task {empty:?} must be rejected"
            );
        }
        let ok = TaskSet::from_input(&json!({"task": "  find the config loader  "}))
            .expect("a real brief still parses");
        assert_eq!(
            ok.tasks()[0].brief,
            "find the config loader",
            "and is trimmed"
        );
    }
}
