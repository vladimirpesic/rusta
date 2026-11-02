//! The Kimi Coding Plan's own services: `POST {base}/search` and
//! `POST {base}/fetch` for the web, `POST {base}/tools` with `chat_title`
//! for a session's title, and `POST {base}/client_configs` for Kimi's rules
//! on model switches, authenticated with the plan's API key.

use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::json;

use crate::errors::LlmError;

use super::kimi_switch_rules::ModelSwitchRules;

/// A search may take this long.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
/// A fetch renders the page, so it gets longer.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// A title is not worth a long wait.
const TITLE_TIMEOUT: Duration = Duration::from_secs(8);
/// Nor is a client config.
const CLIENT_CONFIG_TIMEOUT: Duration = Duration::from_secs(5);

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SearchHit {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub site_name: Option<String>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    search_results: Vec<SearchHit>,
}

#[derive(Deserialize)]
struct TitleResponse {
    #[serde(default)]
    title: Option<String>,
}

/// A client for the search and fetch services of one Kimi Coding host.
#[derive(Debug, Clone)]
pub struct KimiServices {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    headers: HeaderMap,
}

impl KimiServices {
    /// A client for `{base_url}/search` and `{base_url}/fetch` with
    /// `api_key`, sending `headers` (the custom headers) with each request.
    pub fn new(base_url: &str, api_key: &str, headers: HeaderMap) -> Result<Self, LlmError> {
        Ok(Self {
            client: crate::http::client_builder().build()?,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            headers,
        })
    }

    /// Search the web for `query`.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, LlmError> {
        let response = self
            .client
            .post(format!("{}/search", self.base_url))
            .headers(self.headers.clone())
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .timeout(SEARCH_TIMEOUT)
            .json(&json!({ "text_query": query }))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(LlmError::Provider(
                format!("Kimi search failed: HTTP {status}. {detail}")
                    .trim()
                    .to_string(),
            ));
        }
        let body: SearchResponse = response.json().await?;
        Ok(body.search_results)
    }

    /// The page at `url`, rendered as Markdown.
    pub async fn fetch(&self, url: &str) -> Result<String, LlmError> {
        let response = self
            .client
            .post(format!("{}/fetch", self.base_url))
            .headers(self.headers.clone())
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(ACCEPT, HeaderValue::from_static("text/markdown"))
            .timeout(FETCH_TIMEOUT)
            .json(&json!({ "url": url }))
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(LlmError::Provider(
                format!("Kimi fetch failed: HTTP {status}. {body}")
                    .trim()
                    .to_string(),
            ));
        }
        Ok(body)
    }

    /// A title for the chat in `chat_content`: its exchanges as `user: …` and
    /// `assistant: …` lines.
    pub async fn chat_title(&self, chat_content: &str) -> Result<String, LlmError> {
        let response = self
            .client
            .post(format!("{}/tools", self.base_url))
            .headers(self.headers.clone())
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .timeout(TITLE_TIMEOUT)
            .json(&json!({
                "method": "chat_title",
                "params": { "chat_content": chat_content },
            }))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(LlmError::Provider(
                format!("Kimi chat_title failed: HTTP {status}. {detail}")
                    .trim()
                    .to_string(),
            ));
        }
        let body: TitleResponse = response.json().await?;
        body.title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
            .ok_or_else(|| LlmError::Provider("Kimi chat_title gave no title".to_string()))
    }

    /// Kimi's rules for confirming a model switch (`model_switch_confirm`).
    /// `None` when they cannot be read — no answer in time, an error, a
    /// config of another name — and then no switch is confirmed.
    pub async fn model_switch_rules(&self) -> Option<ModelSwitchRules> {
        const NAME: &str = "model_switch_confirm";
        let response = self
            .client
            .post(format!("{}/client_configs", self.base_url))
            .headers(self.headers.clone())
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .timeout(CLIENT_CONFIG_TIMEOUT)
            .json(&json!({ "name": NAME }))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        if body.get("name").and_then(serde_json::Value::as_str) != Some(NAME) {
            return None;
        }
        Some(ModelSwitchRules::from_config(body.get("config")?))
    }
}

/// Search results as the model reads them: title, site, date, URL and
/// snippet each, then how to cite them.
#[must_use]
pub fn format_hits(query: &str, hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return format!("No results for {query:?}.");
    }
    let mut out = String::new();
    for hit in hits {
        out.push_str(&format!("Title: {}\n", hit.title));
        if let Some(site) = hit.site_name.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("Site: {site}\n"));
        }
        if let Some(date) = hit.date.as_deref().filter(|d| !d.is_empty()) {
            out.push_str(&format!("Date: {date}\n"));
        }
        out.push_str(&format!("URL: {}\nSnippet: {}\n\n", hit.url, hit.snippet));
    }
    out.push_str(
        "When you rely on a result in your answer, cite it inline as a markdown link, e.g. \
         [title](url).",
    );
    out
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn search_posts_the_query_and_reads_the_results() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/coding/v1/search"))
            .and(header("authorization", "Bearer sk-test"))
            .and(header("x-route", "blue"))
            .and(body_json(json!({ "text_query": "rust 2024 edition" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "search_results": [{
                    "site_name": "Rust Blog",
                    "title": "Announcing Rust 2024",
                    "url": "https://blog.rust-lang.org/2025/02/20/Rust-1.85.0.html",
                    "snippet": "The 2024 edition is stable.",
                    "content": "long page text",
                    "date": "2025-02-20"
                }]
            })))
            .mount(&server)
            .await;
        let mut headers = HeaderMap::new();
        headers.insert("x-route", HeaderValue::from_static("blue"));
        let services =
            KimiServices::new(&format!("{}/coding/v1/", server.uri()), "sk-test", headers).unwrap();

        let hits = services.search("rust 2024 edition").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].site_name.as_deref(), Some("Rust Blog"));
        let text = format_hits("rust 2024 edition", &hits);
        assert!(text.starts_with("Title: Announcing Rust 2024\nSite: Rust Blog\nDate: 2025-02-20\nURL: https://blog.rust-lang.org/"), "{text}");
        assert!(text.contains("Snippet: The 2024 edition is stable."));
        assert!(!text.contains("long page text"), "content is left out");
        assert!(text.ends_with("e.g. [title](url)."));
    }

    #[tokio::test]
    async fn fetch_asks_for_markdown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/fetch"))
            .and(header("accept", "text/markdown"))
            .and(body_json(json!({ "url": "https://example.com" })))
            .respond_with(ResponseTemplate::new(200).set_body_string("# Example Domain"))
            .mount(&server)
            .await;
        let services = KimiServices::new(&server.uri(), "sk-test", HeaderMap::new()).unwrap();
        assert_eq!(
            services.fetch("https://example.com").await.unwrap(),
            "# Example Domain"
        );
    }

    #[tokio::test]
    async fn failures_name_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid key"))
            .mount(&server)
            .await;
        let services = KimiServices::new(&server.uri(), "sk-bad", HeaderMap::new()).unwrap();
        let err = services.search("x").await.unwrap_err().to_string();
        assert!(
            err.contains("HTTP 401") && err.contains("invalid key"),
            "{err}"
        );
        let err = services.fetch("https://x").await.unwrap_err().to_string();
        assert!(err.contains("Kimi fetch failed: HTTP 401"), "{err}");
        let err = services.chat_title("x").await.unwrap_err().to_string();
        assert!(err.contains("Kimi chat_title failed: HTTP 401"), "{err}");
    }

    /// The title service gets the chat in the shape it takes, and its title
    /// comes back trimmed; an empty one is no title.
    #[tokio::test]
    async fn chat_title_posts_the_chat_and_reads_the_title() {
        let server = MockServer::start().await;
        let chat = "user: Fix the login test.\nassistant: Fixed.";
        Mock::given(method("POST"))
            .and(path("/coding/v1/tools"))
            .and(header("authorization", "Bearer sk-test"))
            .and(header("accept", "application/json"))
            .and(body_json(json!({
                "method": "chat_title",
                "params": { "chat_content": chat },
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "title": " Fix login test " })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/empty/tools"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "title": "  " })))
            .mount(&server)
            .await;
        let services = KimiServices::new(
            &format!("{}/coding/v1", server.uri()),
            "sk-test",
            HeaderMap::new(),
        )
        .unwrap();
        assert_eq!(services.chat_title(chat).await.unwrap(), "Fix login test");
        let services = KimiServices::new(
            &format!("{}/empty", server.uri()),
            "sk-test",
            HeaderMap::new(),
        )
        .unwrap();
        let err = services.chat_title(chat).await.unwrap_err().to_string();
        assert!(err.contains("gave no title"), "{err}");
    }

    /// The switch rules come from `client_configs` by name; anything but a
    /// config of that name reads as no rules.
    #[tokio::test]
    async fn the_switch_rules_are_read_by_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/coding/v1/client_configs"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_json(json!({ "name": "model_switch_confirm" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "model_switch_confirm",
                "config": {"enable": true, "rules": [
                    {"from": "*", "to": ["k3.1-pro"], "session_type": "old", "hint": {"en": "Mind the switch"}}
                ]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/failing/client_configs"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "name": "model_switch_confirm", "config": {"enable": true, "rules": []}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/other/client_configs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "recommended_effort", "config": {"enable": true, "rules": []}
            })))
            .mount(&server)
            .await;
        let services = |base: &str| {
            KimiServices::new(
                &format!("{}{base}", server.uri()),
                "sk-test",
                HeaderMap::new(),
            )
            .unwrap()
        };
        let rules = services("/coding/v1").model_switch_rules().await.unwrap();
        let rule = rules
            .matching(
                "k3",
                "k3.1-pro",
                super::super::kimi_switch_rules::SessionKind::Old,
            )
            .unwrap();
        assert_eq!(rule.hint(), Some("Mind the switch"));
        assert!(services("/other").model_switch_rules().await.is_none());
        assert!(services("/failing").model_switch_rules().await.is_none());
        assert!(services("/missing").model_switch_rules().await.is_none());
    }

    #[test]
    fn no_results_says_so() {
        assert_eq!(format_hits("q", &[]), "No results for \"q\".");
    }
}
