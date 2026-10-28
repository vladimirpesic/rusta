use std::path::PathBuf;

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

pub struct WriteFileTool {
    tool: Tool,
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "write_file",
                "Write content to a file, replacing all of it. Creates parent directories as \
                 needed. For a change to part of a file, use edit_file.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute or relative file path" },
                        "content": { "type": "string", "description": "The file's whole content. Never include read_file's line numbers: they are not part of the file." }
                    },
                    "required": ["path", "content"]
                }),
            ),
        }
    }
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for WriteFileTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, input: &serde_json::Value) -> ToolAccesses {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_default();
        ToolAccesses::write_file(path)
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        "write_file"
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let path = match ctx.resolve_allowed_path(std::path::Path::new(path)) {
            Ok(p) => p,
            Err(e) => return ExecutableToolResult::error(e),
        };

        // A whole-file write replaces everything from context that may be many
        // turns old, so it is checked exactly as an edit is. A path that does
        // not exist yet is always allowed — creating a file destroys nothing.
        let current = crate::stale_guard::FileStamp::of(&path).await;
        if let Err(reason) = ctx.stale_guard.lock().unwrap().check(&path, current) {
            return ExecutableToolResult::error(reason.message(&path));
        }

        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ExecutableToolResult::error(format!(
                "Failed to create directories for {}: {}",
                path.display(),
                e
            ));
        }

        match tokio::fs::write(&path, content).await {
            Ok(_) => {
                // The agent has now seen this file: it just wrote its contents,
                // so they can be digested without reading them back.
                if let Ok(metadata) = tokio::fs::metadata(&path).await {
                    let stamp = crate::stale_guard::FileStamp::from_parts(
                        &metadata,
                        Some(content.as_bytes()),
                    );
                    ctx.stale_guard.lock().unwrap().record(path.clone(), stamp);
                }
                ExecutableToolResult::text(format!("Wrote {}", path.display()))
            }
            Err(e) => {
                ExecutableToolResult::error(format!("Failed to write {}: {}", path.display(), e))
            }
        }
    }
}
