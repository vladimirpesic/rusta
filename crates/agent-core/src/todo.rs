use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lm_bridge::message::{Message, Role};

use crate::tools::builtin::todo_list::TODO_LIST_TOOL_NAME;

/// Assistant steps without a change to the todo list, and since the last
/// reminder, after which a list with items still open is reminded about.
pub const STALE_REMINDER_STEPS: usize = 5;

/// The reminder's first words, by which the conversation shows when it was
/// last given.
const STALE_REMINDER_MARKER: &str = "The todo list has not been updated in the last";

/// Status of a single todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TodoStatus {
    Pending,
    Done,
}

/// A single task in the todo list.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub id: u64,
    pub description: String,
    pub status: TodoStatus,
}

/// Shared todo list for a single agent session.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoList {
    items: Vec<TodoItem>,
    next_id: u64,
    #[serde(skip)]
    persist_path: Option<PathBuf>,
}

impl TodoList {
    /// Create an empty todo list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap the list in an `Arc<Mutex<_>>` for sharing across tool calls.
    #[must_use]
    pub fn shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::new()))
    }

    /// Set the path used to persist the todo list.
    pub fn set_persist_path(&mut self, path: impl Into<PathBuf>) {
        self.persist_path = Some(path.into());
    }

    /// Add a new item and return its ID.
    pub fn add(&mut self, description: impl Into<String>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(TodoItem {
            id,
            description: description.into(),
            status: TodoStatus::Pending,
        });
        let _ = self.persist();
        id
    }

    /// Mark an item as done. Returns `true` if the ID existed.
    pub fn mark_done(&mut self, id: u64) -> bool {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.status = TodoStatus::Done;
            let _ = self.persist();
            true
        } else {
            false
        }
    }

    /// Return all items.
    #[must_use]
    pub fn list(&self) -> &[TodoItem] {
        &self.items
    }

    /// Remove all items.
    pub fn clear(&mut self) {
        self.items.clear();
        self.next_id = 0;
        let _ = self.persist();
    }

    /// Whether the list has no pending items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Items not done yet.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == TodoStatus::Pending)
            .count()
    }

    /// The reminder for a list with items still open after `steps` steps
    /// without a change, to inject into the conversation.
    #[must_use]
    pub fn stale_reminder(&self, steps: usize) -> Message {
        let done = self.items.len() - self.open_count();
        Message::user(format!(
            "<system-reminder>\n{STALE_REMINDER_MARKER} {steps} steps ({done}/{total} done). If \
             an item is finished, mark it done now with `todo_list` (action \"done\"); that call \
             can go in the same response as your next tool call. If the list no longer matches \
             the current work, clear it and add what does. Never mention this reminder to the \
             user.\n\nCurrent todo list:\n{list}\n</system-reminder>",
            total = self.items.len(),
            list = self.format(),
        ))
    }

    /// Format the list as human-readable text.
    #[must_use]
    pub fn format(&self) -> String {
        if self.items.is_empty() {
            return "No todos.".to_string();
        }
        self.items
            .iter()
            .map(|i| {
                let marker = match i.status {
                    TodoStatus::Pending => "[ ]",
                    TodoStatus::Done => "[x]",
                };
                format!("{marker} {}: {}", i.id, i.description)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Persist the todo list to the configured sidecar path, if any.
    pub fn persist(&self) -> Result<(), crate::errors::AgentError> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        std::fs::write(path, json).map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        Ok(())
    }

    /// Load the todo list from a sidecar file, and persist later changes to
    /// it.
    ///
    /// A missing file is a session with no todos yet: the list starts empty
    /// and creates the file at its first change.
    pub fn load(path: &Path) -> Result<Self, crate::errors::AgentError> {
        let mut list = if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
            serde_json::from_str(&content)
                .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?
        } else {
            Self::new()
        };
        list.persist_path = Some(path.to_path_buf());
        Ok(list)
    }
}

/// Whether `message` is an assistant step that changed the todo list: a
/// `todo_list` call that adds, marks done or clears.
#[must_use]
pub fn changes_todo_list(message: &Message) -> bool {
    message.role == Role::Assistant
        && message.tool_calls.iter().any(|call| {
            call.name == TODO_LIST_TOOL_NAME
                && call
                    .arguments
                    .as_deref()
                    .and_then(|args| serde_json::from_str::<serde_json::Value>(args).ok())
                    .and_then(|args| args.get("action")?.as_str().map(str::to_string))
                    .is_some_and(|action| matches!(action.as_str(), "add" | "done" | "clear"))
        })
}

/// Whether `message` carries the stale-list reminder.
#[must_use]
pub fn is_stale_reminder(message: &Message) -> bool {
    message.role == Role::User && message.text_content().contains(STALE_REMINDER_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_list() {
        let mut list = TodoList::new();
        let id = list.add("write tests");
        assert_eq!(list.list().len(), 1);
        assert_eq!(list.list()[0].id, id);
        assert!(matches!(list.list()[0].status, TodoStatus::Pending));
    }

    #[test]
    fn test_mark_done() {
        let mut list = TodoList::new();
        let id = list.add("write tests");
        assert!(list.mark_done(id));
        assert!(matches!(list.list()[0].status, TodoStatus::Done));
        assert!(!list.mark_done(999));
    }

    #[test]
    fn test_clear() {
        let mut list = TodoList::new();
        list.add("a");
        list.add("b");
        list.clear();
        assert!(list.is_empty());
    }

    #[test]
    fn test_format() {
        let mut list = TodoList::new();
        list.add("task one");
        let id2 = list.add("task two");
        list.mark_done(id2);
        let text = list.format();
        assert!(text.contains("[ ] 0: task one"));
        assert!(text.contains("[x] 1: task two"));
    }

    /// A change is an `add`, `done` or `clear` call of `todo_list`; a `list`
    /// call, another tool or unreadable arguments are none.
    #[test]
    fn changes_are_the_calls_that_write_the_list() {
        use lm_bridge::message::ToolCall;
        let step = |name: &str, args: &str| {
            Message::assistant_tool_calls(vec![ToolCall::new("c", name).with_arguments(args)])
        };
        for action in ["add", "done", "clear"] {
            let args = format!(r#"{{"action":"{action}"}}"#);
            assert!(
                changes_todo_list(&step(TODO_LIST_TOOL_NAME, &args)),
                "{action}"
            );
        }
        assert!(!changes_todo_list(&step(
            TODO_LIST_TOOL_NAME,
            r#"{"action":"list"}"#
        )));
        assert!(!changes_todo_list(&step(TODO_LIST_TOOL_NAME, "not json")));
        assert!(!changes_todo_list(&step("bash", r#"{"action":"add"}"#)));
        assert!(!changes_todo_list(&Message::user("todo_list add")));
    }

    /// The reminder counts the steps and what is done, lists the items, and
    /// is recognized again in the conversation.
    #[test]
    fn the_reminder_names_the_steps_and_the_list() {
        let mut list = TodoList::new();
        list.add("write tests");
        let id = list.add("run them");
        list.mark_done(id);
        assert_eq!(list.open_count(), 1);

        let reminder = list.stale_reminder(7);
        let text = reminder.text_content();
        assert!(text.starts_with("<system-reminder>\n"), "{text}");
        assert!(
            text.contains("has not been updated in the last 7 steps (1/2 done)"),
            "{text}"
        );
        assert!(
            text.contains("[ ] 0: write tests\n[x] 1: run them"),
            "{text}"
        );
        assert!(is_stale_reminder(&reminder));
        assert!(!is_stale_reminder(&Message::assistant_text(text)));
    }

    #[test]
    fn test_persist_and_load() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("todos.json");

        {
            let mut list = TodoList::new();
            list.set_persist_path(&path);
            list.add("persist me");
            list.mark_done(0);
        }

        let loaded = TodoList::load(&path).unwrap();
        assert_eq!(loaded.list().len(), 1);
        assert!(matches!(loaded.list()[0].status, TodoStatus::Done));
    }

    /// A sidecar that does not exist yet loads empty, and the first change
    /// creates it.
    #[test]
    fn a_missing_sidecar_is_created_at_the_first_change() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("todos.json");

        let mut list = TodoList::load(&path).unwrap();
        assert!(list.is_empty());
        list.add("persist me");

        let loaded = TodoList::load(&path).unwrap();
        assert_eq!(loaded.list().len(), 1);
        assert_eq!(loaded.list()[0].description, "persist me");
    }
}
