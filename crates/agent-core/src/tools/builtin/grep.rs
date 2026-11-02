use std::path::PathBuf;

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

pub struct GrepTool {
    tool: Tool,
}

impl GrepTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "grep",
                "Search file contents with a regex pattern.",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern" },
                        "path": { "type": "string", "description": "Directory or file to search (default cwd)" },
                        "output_offset": { "type": "integer", "description": "1-based starting match" },
                        "output_limit": { "type": "integer", "description": "Max matches" }
                    },
                    "required": ["pattern"]
                }),
            ),
        }
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for GrepTool {
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
        let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let path = match ctx.resolve_readable_path(std::path::Path::new(path)) {
            Ok(p) => p,
            Err(e) => return ExecutableToolResult::error(e),
        };
        let offset = input
            .get("output_offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1);
        let limit = input
            .get("output_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50);

        let regex = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return ExecutableToolResult::error(format!("Invalid regex: {}", e)),
        };

        let mut matches = Vec::new();
        let walker = ignore::WalkBuilder::new(&path)
            .standard_filters(true)
            .build();

        for entry in walker.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let content = match tokio::fs::read_to_string(path).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (line_num, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    let rel = path.strip_prefix(&ctx.cwd).unwrap_or(path);
                    matches.push(format!("{}:{}:{}", rel.display(), line_num + 1, line));
                    if matches.len() as u64 >= offset + limit - 1 {
                        break;
                    }
                }
            }
        }

        let start = (offset.saturating_sub(1) as usize).min(matches.len());
        let end = (start + limit as usize).min(matches.len());
        let selected: Vec<_> = matches[start..end].to_vec();

        if selected.is_empty() {
            ExecutableToolResult::text("No matches found.")
        } else {
            ExecutableToolResult::text(selected.join("\n"))
        }
    }
}
