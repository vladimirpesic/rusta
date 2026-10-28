//! Reminders for instruction files (`AGENTS.md`) the model has not seen.
//!
//! The system prompt carries the instruction files of the working directory
//! and its parents. A subdirectory may have its own, with rules for that
//! part of the tree; when a tool touches such a directory and its file is
//! neither in the prompt nor already read, the next request carries a
//! reminder naming it. A reminder that compaction or a clear drops is given
//! again, until the model reads the file.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// File names that hold a directory's instructions, as the project-context
/// loader reads them.
pub const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "agents.md", "CLAUDE.md", "claude.md"];

/// The same names under `.rusta/`.
const DOT_RUSTA: &str = ".rusta";

/// How far up a directory outside the working directory is searched when it
/// has no `.git` to mark its project root.
const MAX_OUTSIDE_LEVELS: usize = 8;

/// What the agent has and has not been told about instruction files.
#[derive(Debug, Default)]
pub struct AgentsMdReminder {
    /// In the system prompt, or read (or written) by the model.
    known: HashSet<PathBuf>,
    /// Found, not yet mentioned.
    queued: BTreeSet<PathBuf>,
    /// Mentioned in a reminder that is still in the context, not yet read.
    reminded: BTreeSet<PathBuf>,
    /// The working directory: its files, and its parents', are in the system
    /// prompt, so a search from below stops here.
    root: Option<PathBuf>,
}

impl AgentsMdReminder {
    /// A tracker for an agent working in `root`, whose system prompt is
    /// `system_prompt`: the files the prompt carries (`<!-- From: … -->`)
    /// are known.
    #[must_use]
    pub fn new(root: &Path, system_prompt: &str) -> Self {
        let mut tracker = Self {
            root: Some(canonical(root)),
            ..Self::default()
        };
        tracker.seed_from_prompt(system_prompt);
        tracker
    }

    /// Record the files a (new) system prompt carries as known.
    pub fn seed_from_prompt(&mut self, system_prompt: &str) {
        for line in system_prompt.lines() {
            let Some(path) = line
                .trim()
                .strip_prefix("<!-- From: ")
                .and_then(|rest| rest.strip_suffix(" -->"))
            else {
                continue;
            };
            let path = canonical(Path::new(path));
            if is_instruction_file(&path) {
                self.queued.remove(&path);
                self.reminded.remove(&path);
                self.known.insert(path);
            }
        }
    }

    /// After a tool call: note an instruction file it read or wrote, and look
    /// for unseen ones in the directories it touched. `cwd` resolves relative
    /// paths; `succeeded` says whether the call worked.
    pub fn observe(&mut self, tool: &str, input: &Value, cwd: &Path, succeeded: bool) {
        let path_arg = |key: &str| {
            input
                .get(key)
                .and_then(Value::as_str)
                .filter(|p| !p.trim().is_empty())
                .map(|p| resolve(cwd, p))
        };
        let dirs: Vec<PathBuf> = match tool {
            "read_file" | "write_file" | "edit_file" | "read_media" => {
                let Some(path) = path_arg("path") else {
                    return;
                };
                if succeeded && is_instruction_file(&path) {
                    let path = canonical(&path);
                    self.queued.remove(&path);
                    self.reminded.remove(&path);
                    self.known.insert(path);
                }
                path.parent().map(Path::to_path_buf).into_iter().collect()
            }
            "list_dir" | "glob" | "grep" => {
                vec![path_arg("path").unwrap_or_else(|| cwd.to_path_buf())]
            }
            "bash" => path_arg("cwd").into_iter().collect(),
            _ => return,
        };
        for dir in dirs {
            self.probe(&dir);
        }
    }

    /// Queue the unseen instruction files from `dir` up to its root.
    fn probe(&mut self, dir: &Path) {
        let Some(anchor) = nearest_existing_dir(dir) else {
            return;
        };
        let anchor = canonical(&anchor);
        let stop = self.stop_for(&anchor);
        let mut current = Some(anchor.as_path());
        let mut levels = 0;
        while let Some(dir) = current {
            for path in instruction_files_in(dir) {
                let path = canonical(&path);
                if !self.known.contains(&path) && !self.reminded.contains(&path) {
                    self.queued.insert(path);
                }
            }
            levels += 1;
            if Some(dir) == stop.as_deref() || (stop.is_none() && levels >= MAX_OUTSIDE_LEVELS) {
                break;
            }
            current = dir.parent();
        }
    }

    /// Where a search from `dir` stops: the working directory when `dir` is
    /// inside it, otherwise the nearest ancestor holding `.git`.
    fn stop_for(&self, dir: &Path) -> Option<PathBuf> {
        if let Some(root) = &self.root
            && dir.starts_with(root)
        {
            return Some(root.clone());
        }
        dir.ancestors()
            .find(|ancestor| ancestor.join(".git").exists())
            .map(Path::to_path_buf)
    }

    /// The reminder to add before the next request, naming the files found
    /// since the last one; `None` when there are none.
    pub fn take_reminder(&mut self) -> Option<String> {
        if self.queued.is_empty() {
            return None;
        }
        let fresh: Vec<PathBuf> = std::mem::take(&mut self.queued).into_iter().collect();
        let list: Vec<String> = fresh
            .iter()
            .map(|path| format!("- {}", path.display()))
            .collect();
        self.reminded.extend(fresh);
        Some(format!(
            "<system-reminder>\nThese instruction files apply to paths your recent tool calls \
             touched, but are not in your system prompt:\n{}\nRead them before you change \
             anything in those directories.\n</system-reminder>",
            list.join("\n")
        ))
    }

    /// The context lost its reminders (compaction, a clear, an undo): name the
    /// files not yet read again at the next request.
    pub fn forget_reminders(&mut self) {
        let reminded = std::mem::take(&mut self.reminded);
        self.queued.extend(reminded);
    }
}

fn is_instruction_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| INSTRUCTION_FILES.contains(&name))
}

/// The instruction files in `dir` itself and in its `.rusta/`, non-empty.
fn instruction_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for base in [dir.to_path_buf(), dir.join(DOT_RUSTA)] {
        for name in INSTRUCTION_FILES {
            let path = base.join(name);
            if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() > 0) {
                found.push(path);
            }
        }
    }
    found
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn nearest_existing_dir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.is_dir())
        .map(Path::to_path_buf)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
        std::fs::create_dir_all(dir.path().join("crates/core/src")).unwrap();
        std::fs::write(dir.path().join("crates/core/AGENTS.md"), "core rules").unwrap();
        std::fs::write(dir.path().join("crates/core/src/lib.rs"), "fn x() {}").unwrap();
        dir
    }

    fn prompt_carrying(path: &Path) -> String {
        format!("intro\n<!-- From: {} -->\nrules\n", path.display())
    }

    /// Touching a file below a directory with its own instructions names that
    /// file once; the files the prompt carries are never named.
    #[test]
    fn a_subdirectory_file_is_named_once() {
        let dir = project();
        let root = dir.path();
        let mut tracker = AgentsMdReminder::new(root, &prompt_carrying(&root.join("AGENTS.md")));

        tracker.observe(
            "read_file",
            &json!({"path": "crates/core/src/lib.rs"}),
            root,
            true,
        );
        let reminder = tracker
            .take_reminder()
            .expect("the subdirectory file is named");
        let core = canonical(&root.join("crates/core/AGENTS.md"));
        assert!(reminder.contains(&core.display().to_string()), "{reminder}");
        assert!(!reminder.contains(&format!(
            "{}\n",
            canonical(&root.join("AGENTS.md")).display()
        )));
        assert!(reminder.starts_with("<system-reminder>"));

        tracker.observe("grep", &json!({"path": "crates/core"}), root, true);
        assert!(tracker.take_reminder().is_none(), "named once");
    }

    /// A reminder lost to compaction is given again, until the file is read.
    #[test]
    fn a_lost_reminder_returns_until_the_file_is_read() {
        let dir = project();
        let root = dir.path();
        let mut tracker = AgentsMdReminder::new(root, &prompt_carrying(&root.join("AGENTS.md")));
        tracker.observe("list_dir", &json!({"path": "crates/core/src"}), root, true);
        assert!(tracker.take_reminder().is_some());

        tracker.forget_reminders();
        assert!(tracker.take_reminder().is_some(), "compaction dropped it");

        tracker.observe(
            "read_file",
            &json!({"path": root.join("crates/core/AGENTS.md")}),
            root,
            true,
        );
        tracker.forget_reminders();
        assert!(
            tracker.take_reminder().is_none(),
            "read, so no longer named"
        );
    }

    /// A failed read does not count as reading the file.
    #[test]
    fn a_failed_read_does_not_count() {
        let dir = project();
        let root = dir.path();
        let mut tracker = AgentsMdReminder::new(root, &prompt_carrying(&root.join("AGENTS.md")));
        tracker.observe(
            "read_file",
            &json!({"path": "crates/core/AGENTS.md"}),
            root,
            false,
        );
        assert!(tracker.take_reminder().is_some());
    }

    /// A prompt that carries no instruction files — a sub-agent's, when there
    /// is no session prompt to give it — has the working directory's own
    /// named too; `bash` counts through its `cwd`.
    #[test]
    fn an_agent_without_project_instructions_hears_about_the_root() {
        let dir = project();
        let root = dir.path();
        let mut tracker = AgentsMdReminder::new(root, "a sub-agent prompt");
        tracker.observe("bash", &json!({"command": "ls", "cwd": "."}), root, true);
        let reminder = tracker.take_reminder().expect("the root file is named");
        assert!(
            reminder.contains(&canonical(&root.join("AGENTS.md")).display().to_string()),
            "{reminder}"
        );
        tracker.observe("bash", &json!({"command": "ls"}), root, true);
        assert!(tracker.take_reminder().is_none(), "no cwd, nothing touched");
    }

    #[test]
    fn empty_files_and_other_tools_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/AGENTS.md"), "").unwrap();
        let mut tracker = AgentsMdReminder::new(dir.path(), "");
        tracker.observe("list_dir", &json!({"path": "sub"}), dir.path(), true);
        tracker.observe("web_search", &json!({"query": "x"}), dir.path(), true);
        assert!(tracker.take_reminder().is_none());
    }
}
