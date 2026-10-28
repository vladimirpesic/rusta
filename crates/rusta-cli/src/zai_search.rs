//! Z.AI native web-search client for the built-in `web_search` tool.
//!
//! Naming note: the agent builtin is `web_search`; the standalone Z.AI HTTP
//! endpoint is `POST /web_search` (see `lm_bridge::zai_tools`); MCP servers may
//! expose the same capability as `zai_web_search`. These names intentionally
//! differ by layer: builtin → HTTP endpoint → MCP server.

use std::sync::Arc;

use agent_core::web_search_client::WebSearchClient;
use async_trait::async_trait;
use lm_bridge::zai_tools::{WebSearchParams, ZaiToolsClient};

/// Adapter from the agent's `WebSearchClient` trait to `lm_bridge::ZaiToolsClient`.
pub struct ZaiWebSearchClient {
    inner: Arc<ZaiToolsClient>,
}

impl ZaiWebSearchClient {
    pub fn new(inner: ZaiToolsClient) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

#[async_trait]
impl WebSearchClient for ZaiWebSearchClient {
    async fn search(&self, query: &str, _question: Option<&str>) -> Result<String, String> {
        let params = WebSearchParams {
            query: query.to_string(),
            ..Default::default()
        };
        self.inner
            .web_search(&params)
            .await
            .map_err(|e| format!("{e}"))
    }
}
