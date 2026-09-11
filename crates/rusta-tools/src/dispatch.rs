//! The `dispatch` tool — §6.8 entry from the registry.
//!
//! The sub-coder runner ([`ReadOnly`]) reuses the same handler functions as
//! [`crate::exec`] restricted to the five read-only tools, with two
//! deliberate differences: its reads do not credit the read-before-edit
//! ledger (§6.8 isolation — the auto-inject still protects edits), and its
//! repo-map renders without session chat-files (the sub-coder has none).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusta_dispatch::{ExecMode, RunTool, TaskSet, labeled, run};
use rusta_repomap::RepoMap;
use serde_json::{Value, json};

use crate::Tools;
use crate::exec::{ToolOutcome, lock};

/// Execute `dispatch(task | tasks)`.
pub(crate) async fn dispatch(tools: &Tools, input: &Value) -> ToolOutcome {
    let set = match TaskSet::from_input(input) {
        Ok(set) => set,
        Err(err) => {
            return ToolOutcome::error(format!(
                "{err}. Example: {}",
                json!({"tasks": [{"label": "auth", "task": "where is login handled?"}]})
            ));
        }
    };
    let runner = ReadOnly {
        root: tools.root().to_path_buf(),
        repomap: Arc::clone(tools.repomap_arc()),
    };
    let mode = ExecMode::for_backend(tools.backend().kind());
    let reports = run(Arc::clone(tools.backend_arc()), runner, mode, set).await;
    let status = if reports.iter().any(|report| report.failed) {
        rusta_core::Status::Error
    } else {
        rusta_core::Status::Ok
    };
    ToolOutcome {
        status,
        content: labeled(&reports),
        truncated: reports.iter().any(|report| report.truncated),
        read_credit: None,
    }
}

/// The sub-coder tool executor: §6.8's read-only five, no ledger credit.
#[derive(Clone)]
struct ReadOnly {
    root: PathBuf,
    repomap: Arc<Mutex<RepoMap>>,
}

impl RunTool for ReadOnly {
    async fn run(&self, name: String, input: Value) -> String {
        let outcome = match name.as_str() {
            "read" => crate::read::read(&self.root, &input),
            "grep" => crate::search::grep(&self.root, &input),
            "glob" => crate::glob::glob(&self.root, &input),
            "map_drill" => crate::map::drill(&self.root, &input),
            "map_refresh" => {
                let mut map = lock(&self.repomap);
                crate::map::refresh(&mut map, &[])
            }
            other => ToolOutcome::error(format!(
                "{other:?} is not available to sub-coders. Available: read, grep, glob, \
                 map_refresh, map_drill."
            )),
        };
        let flag = match outcome.status {
            rusta_core::Status::Ok => "ok",
            rusta_core::Status::Error => "error",
        };
        format!("TOOL RESULT {name} ({flag})\n{}", outcome.content)
    }
}
