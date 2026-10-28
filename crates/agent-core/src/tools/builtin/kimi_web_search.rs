//! Kimi-native `$web_search` builtin function.
//!
//! Moonshot Kimi models expose a server-side web search builtin. We surface it
//! as a locally-registered tool so the agent can include it in the tool list;
//! when the model calls it, execution delegates to the generic `web_search`
//! implementation: the Coding Plan's search, or a configured search URL
//! template.

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

use super::web_search::WebSearchTool;

pub struct KimiWebSearchTool {
    tool: Tool,
    inner: WebSearchTool,
}

impl KimiWebSearchTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "$web_search",
                "Search the web: the Kimi Coding Plan's search service, or a configured search URL template (RUSTA_SEARCH_URL / search_url), which takes precedence.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds (default 30)" }
                    },
                    "required": ["query"]
                }),
            ),
            inner: WebSearchTool::new(),
        }
    }
}

impl Default for KimiWebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for KimiWebSearchTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, input: &serde_json::Value) -> ToolAccesses {
        self.inner.accesses(input)
    }

    fn approval_rule(&self, input: &serde_json::Value) -> &str {
        self.inner.approval_rule(input)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        self.inner.execute(input, ctx, _tool_call_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kimi_web_search_is_builtin_function() {
        let tool = KimiWebSearchTool::new();
        assert!(tool.tool_definition().is_builtin_function());
        assert_eq!(tool.tool_definition().name, "$web_search");
    }
}
