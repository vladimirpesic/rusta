//! Pluggable web-reader client for the built-in `web_reader` tool.
//!
//! `rusta-cli` implements it over the provider's own reader: Z.AI's
//! `/reader`, the Kimi Coding Plan's `/fetch`, or DeepSeek's Fetch API.

use async_trait::async_trait;

/// Reads a web page and returns its content as plain text.
#[async_trait]
pub trait WebReaderClient: Send + Sync {
    /// Fetch `url` and return a formatted text result.
    async fn read(&self, url: &str) -> Result<String, String>;
}
