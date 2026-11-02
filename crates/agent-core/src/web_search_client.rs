//! Pluggable web-search client for the built-in `web_search` tool.
//!
//! `rusta-cli` implements it over the provider's own search: Z.AI's
//! `/web_search`, the Kimi Coding Plan's `/search`, or DeepSeek's Search API.

use async_trait::async_trait;

/// Searches the web and returns a plain-text result.
#[async_trait]
pub trait WebSearchClient: Send + Sync {
    /// Search the web for `query` and return a formatted text result.
    /// `question` is the prompt the search serves, for a service that reads
    /// the intent behind a query from it (DeepSeek's `ori_question`).
    async fn search(&self, query: &str, question: Option<&str>) -> Result<String, String>;
}
