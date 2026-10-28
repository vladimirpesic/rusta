use std::path::PathBuf;

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

const DEFAULT_MAX_DEPTH: usize = 1;
const MAX_MAX_DEPTH: usize = 5;
const MAX_ENTRIES: usize = 500;

pub struct ListDirTool {
    tool: Tool,
}

impl ListDirTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "list_dir",
                "List directory entries with optional recursion depth and stat info.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Directory path" },
                        "recursive": { "type": "boolean", "description": "List recursively" },
                        "max_depth": { "type": "integer", "description": "Maximum recursion depth (default 1, max 5)" }
                    },
                    "required": ["path"]
                }),
            ),
        }
    }
}

impl Default for ListDirTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for ListDirTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, input: &serde_json::Value) -> ToolAccesses {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_default();
        ToolAccesses::read_file(path)
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        "silent"
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let path = match ctx.resolve_allowed_path(std::path::Path::new(path)) {
            Ok(p) => p,
            Err(e) => return ExecutableToolResult::error(e),
        };

        let recursive = input
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let max_depth = input
            .get("max_depth")
            .and_then(|v| v.as_u64())
            .map(|d| d as usize)
            .unwrap_or(DEFAULT_MAX_DEPTH)
            .min(MAX_MAX_DEPTH);

        let effective_depth = if recursive { max_depth } else { 0 };

        match list_directory(&path, effective_depth).await {
            Ok(text) => ExecutableToolResult::text(text),
            Err(e) => ExecutableToolResult::error(format!(
                "Failed to list directory {}: {}",
                path.display(),
                e
            )),
        }
    }
}

async fn list_directory(
    path: &std::path::Path,
    max_depth: usize,
) -> Result<String, std::io::Error> {
    let mut entries = Vec::new();
    let mut count = 0usize;
    visit(path, 0, max_depth, "", &mut entries, &mut count).await?;
    if entries.is_empty() {
        return Ok("Empty directory.".to_string());
    }
    Ok(entries.join("\n"))
}

async fn visit(
    path: &std::path::Path,
    depth: usize,
    max_depth: usize,
    prefix: &str,
    out: &mut Vec<String>,
    count: &mut usize,
) -> Result<(), std::io::Error> {
    if *count >= MAX_ENTRIES {
        out.push("... (truncated)".to_string());
        return Ok(());
    }

    let mut dir = tokio::fs::read_dir(path).await?;
    while let Some(entry) = dir.next_entry().await? {
        if *count >= MAX_ENTRIES {
            out.push("... (truncated)".to_string());
            return Ok(());
        }
        *count += 1;

        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = entry.metadata().await.ok();
        let kind = metadata
            .as_ref()
            .map(|m| {
                if m.is_dir() {
                    "dir"
                } else if m.is_file() {
                    "file"
                } else if m.is_symlink() {
                    "symlink"
                } else {
                    "other"
                }
            })
            .unwrap_or("unknown");
        let size = metadata.as_ref().map(|m| m.len());
        let line = match size {
            Some(s) => format!("{prefix}{name} [{kind}, {s} bytes]"),
            None => format!("{prefix}{name} [{kind}]"),
        };
        out.push(line);

        if depth < max_depth && metadata.is_some_and(|m| m.is_dir()) {
            let child = entry.path();
            Box::pin(visit(
                &child,
                depth + 1,
                max_depth,
                &format!("{prefix}  "),
                out,
                count,
            ))
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_dir_lists_entries() {
        let temp = tempfile::tempdir().unwrap();
        tokio::fs::write(temp.path().join("a.txt"), "a")
            .await
            .unwrap();
        tokio::fs::create_dir(temp.path().join("subdir"))
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("subdir/b.txt"), "b")
            .await
            .unwrap();

        let tool = ListDirTool::new();
        let background = std::sync::Arc::new(crate::background::BackgroundManager::new());
        let ctx = ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: temp.path().to_path_buf(),
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
        };
        let result = tool
            .execute(json!({"path": ".", "recursive": true}), &ctx, "test-id")
            .await;
        assert!(!result.is_error);
        let text = result.text_content();
        assert!(text.contains("a.txt"));
        assert!(text.contains("subdir"));
        assert!(text.contains("b.txt"));
    }
}
