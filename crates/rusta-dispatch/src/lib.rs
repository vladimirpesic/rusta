//! Sub-coder dispatch for Rusta — development plan §6.8 (R9).
//!
//! `rusta-dispatch` spawns isolated, read-only sub-coders over Tokio. Each
//! research task runs in a fresh context (core prompt + task, own turn cap of
//! six, toolset restricted to `read`/`grep`/`glob`/`map_refresh`/`map_drill`);
//! only its ≤400-token labeled report re-enters the main context — full
//! sub-transcripts go to the session log alone. On the embedded backend (one
//! loaded model, one inference thread) requests serialize for correctness;
//! true parallelism exists only on the HTTP backend.
//!
//! Dependency direction is deliberate: this crate knows nothing about the
//! tool registry. It executes sub-coder tool calls through the [`RunTool`]
//! trait, which the host (`rusta-tools`) implements — so the registry wraps
//! dispatch, never the other way around.

mod actor;
mod dag;
mod toolcall;

pub use actor::{
    REPORT_TOKEN_CAP, Report, RunTool, SUB_CODER_TOOLS, SUB_CODER_TURN_CAP, run_actor,
};
pub use dag::{ExecMode, MAX_TASKS, SINGLE_TASK_LABEL, Task, TaskSet, TaskSetError, labeled, run};
pub use toolcall::{ToolCall, ToolCalls, parse_tool_calls};
