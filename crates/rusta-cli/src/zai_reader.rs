//! Z.AI native web-reader client for the built-in `web_reader` tool.

use std::sync::Arc;

use agent_core::web_reader_client::WebReaderClient;
use async_trait::async_trait;
use lm_bridge::zai_tools::{WebReaderParams, ZaiToolsClient};

/// Adapter from the agent's `WebReaderClient` trait to `lm_bridge::ZaiToolsClient`.
pub struct ZaiWebReaderClient {
    inner: Arc<ZaiToolsClient>,
}

impl ZaiWebReaderClient {
    pub fn new(inner: ZaiToolsClient) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

#[async_trait]
impl WebReaderClient for ZaiWebReaderClient {
    async fn read(&self, url: &str) -> Result<String, String> {
        let params = WebReaderParams {
            url: url.to_string(),
            return_format: Some("markdown".to_string()),
            ..Default::default()
        };
        self.inner
            .web_reader(&params)
            .await
            .map_err(|e| format!("{e}"))
    }
}
