//! Standalone Z.AI `/web_search` and `/reader` HTTP endpoints.
//!
//! Mirrors the resources in `z-ai-sdk-python/src/zai/api_resource/web_search/`
//! and `src/zai/api_resource/web_reader/`.

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::errors::LlmError;

/// api.z.ai's `/web_search` engine, the only one it has.
pub const ZAI_SEARCH_ENGINE: &str = "search-prime";

/// open.bigmodel.cn's advanced `/web_search` engine. It also has `search_std`,
/// `search_pro_sogou` and `search_pro_quark`, but not [`ZAI_SEARCH_ENGINE`].
pub const BIGMODEL_SEARCH_ENGINE: &str = "search_pro";

/// Parameters for a Z.AI web search request.
#[derive(Debug, Clone, Default)]
pub struct WebSearchParams {
    pub query: String,
    /// Unset: the client's default engine, else [`ZAI_SEARCH_ENGINE`].
    pub search_engine: Option<String>,
    pub count: Option<u32>,
    /// Whether the platform first judges whether the query calls for a
    /// search, and searches only if it does. Unset, it searches directly: an
    /// agent's query is always meant to be searched.
    pub search_intent: Option<bool>,
    pub request_id: Option<String>,
    pub user_id: Option<String>,
    pub search_domain_filter: Option<String>,
    pub search_recency_filter: Option<String>,
    pub content_size: Option<String>,
    pub include_image: Option<bool>,
}

/// Parameters for the Z.AI `/reader` endpoint.
#[derive(Debug, Clone, Default)]
pub struct WebReaderParams {
    pub url: String,
    pub request_id: Option<String>,
    pub user_id: Option<String>,
    pub timeout: Option<String>,
    pub no_cache: Option<bool>,
    pub return_format: Option<String>,
    pub retain_images: Option<bool>,
    pub no_gfm: Option<bool>,
    pub keep_img_data_url: Option<bool>,
    pub with_images_summary: Option<bool>,
    pub with_links_summary: Option<bool>,
}

/// Client for the Z.AI standalone tool endpoints.
pub struct ZaiToolsClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    default_engine: Option<String>,
}

impl ZaiToolsClient {
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self, LlmError> {
        let client = crate::http::client_builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            default_engine: None,
        })
    }

    pub fn with_default_engine(mut self, engine: impl Into<String>) -> Self {
        self.default_engine = Some(engine.into());
        self
    }

    /// Call `POST /web_search` and return a formatted result string.
    pub async fn web_search(&self, params: &WebSearchParams) -> Result<String, LlmError> {
        let url = format!("{}/web_search", self.base_url.trim_end_matches('/'));
        let mut body = Map::new();
        body.insert("search_query".to_string(), json!(params.query));
        body.insert(
            "search_engine".to_string(),
            json!(
                params
                    .search_engine
                    .as_deref()
                    .or(self.default_engine.as_deref())
                    .unwrap_or(ZAI_SEARCH_ENGINE)
            ),
        );
        body.insert("count".to_string(), json!(params.count.unwrap_or(10)));
        body.insert(
            "search_intent".to_string(),
            json!(params.search_intent.unwrap_or(false)),
        );
        insert_opt(&mut body, "request_id", &params.request_id);
        insert_opt(&mut body, "user_id", &params.user_id);
        insert_opt(
            &mut body,
            "search_domain_filter",
            &params.search_domain_filter,
        );
        insert_opt(
            &mut body,
            "search_recency_filter",
            &params.search_recency_filter,
        );
        insert_opt(&mut body, "content_size", &params.content_size);
        insert_opt(&mut body, "include_image", &params.include_image);

        let json = self.post_json(&url, Value::Object(body)).await?;
        Ok(format_web_search_response(&json))
    }

    /// Call `POST /reader` and return a formatted result string.
    pub async fn web_reader(&self, params: &WebReaderParams) -> Result<String, LlmError> {
        let url = format!("{}/reader", self.base_url.trim_end_matches('/'));
        let mut body = Map::new();
        body.insert("url".to_string(), json!(params.url));
        insert_opt(&mut body, "request_id", &params.request_id);
        insert_opt(&mut body, "user_id", &params.user_id);
        insert_opt(&mut body, "timeout", &params.timeout);
        insert_opt(&mut body, "no_cache", &params.no_cache);
        insert_opt(&mut body, "return_format", &params.return_format);
        insert_opt(&mut body, "retain_images", &params.retain_images);
        insert_opt(&mut body, "no_gfm", &params.no_gfm);
        insert_opt(&mut body, "keep_img_data_url", &params.keep_img_data_url);
        insert_opt(
            &mut body,
            "with_images_summary",
            &params.with_images_summary,
        );
        insert_opt(&mut body, "with_links_summary", &params.with_links_summary);

        let json = self.post_json(&url, Value::Object(body)).await?;
        Ok(format_reader_response(&json))
    }

    async fn post_json(&self, url: &str, body: Value) -> Result<Value, LlmError> {
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json; charset=UTF-8")
            .header("Accept-Language", "en-US,en")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::ApiConnection(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| LlmError::ApiConnection(e.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::ApiStatus {
                status,
                message: text,
                retry_hint: false,
                retry_after: None,
            });
        }
        serde_json::from_str(&text).map_err(Into::into)
    }
}

fn insert_opt<T: serde::Serialize>(body: &mut Map<String, Value>, key: &str, value: &Option<T>) {
    if let Some(v) = value {
        body.insert(key.to_string(), json!(v));
    }
}

fn format_web_search_response(value: &Value) -> String {
    let mut out = String::new();
    let intent = value.get("search_intent").and_then(|v| {
        v.get("intent")
            .or_else(|| v.get("query"))
            .and_then(|v| v.as_str())
            .or_else(|| v.as_str())
    });
    if let Some(intent) = intent {
        out.push_str("Search intent: ");
        out.push_str(intent);
        out.push('\n');
    }
    let results = value
        .get("search_result")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if results.is_empty() {
        out.push_str("No search results returned.");
        return out;
    }
    for (i, result) in results.iter().enumerate() {
        let title = result.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let link = result.get("link").and_then(|v| v.as_str()).unwrap_or("");
        let media = result.get("media").and_then(|v| v.as_str()).unwrap_or("");
        let publish_date = result
            .get("publish_date")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = result.get("content").and_then(|v| v.as_str()).unwrap_or("");
        out.push_str(&format!("[{}] {}\n", i + 1, title));
        if !link.is_empty() {
            out.push_str(&format!("URL: {}\n", link));
        }
        if !media.is_empty() {
            out.push_str(&format!("Source: {}\n", media));
        }
        if !publish_date.is_empty() {
            out.push_str(&format!("Date: {}\n", publish_date));
        }
        if !content.is_empty() {
            out.push_str(&format!("Summary: {}\n", content));
        }
        out.push('\n');
    }
    out
}

fn format_reader_response(value: &Value) -> String {
    let result = value.get("reader_result").cloned().unwrap_or_default();
    let title = result.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let url = result.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let description = result
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let content = result.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(&format!("# {}\n", title));
    }
    if !url.is_empty() {
        out.push_str(&format!("URL: {}\n", url));
    }
    if !description.is_empty() {
        out.push_str(&format!("\n{}\n", description));
    }
    if !content.is_empty() {
        out.push_str(&format!("\n{}\n", content));
    }
    if out.trim().is_empty() {
        out.push_str("No content returned.");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query is searched as it is, with api.z.ai's engine unless the client
    /// names another: BigModel's `search_intent` would search only a query it
    /// judged to need it, and api.z.ai has no `search_pro` of its own.
    #[tokio::test]
    async fn a_search_goes_out_with_the_platforms_engine_and_no_intent_check() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        for (engine, title) in [
            (ZAI_SEARCH_ENGINE, "Z.AI"),
            (BIGMODEL_SEARCH_ENGINE, "BigModel"),
        ] {
            Mock::given(method("POST"))
                .and(path("/web_search"))
                .and(body_partial_json(json!({
                    "search_query": "rust",
                    "search_engine": engine,
                    "search_intent": false,
                    "count": 10,
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "search_result": [{"title": title, "link": "https://rust-lang.org"}]
                })))
                .mount(&server)
                .await;
        }
        let params = WebSearchParams {
            query: "rust".to_string(),
            ..WebSearchParams::default()
        };
        let zai = ZaiToolsClient::new("k", server.uri()).unwrap();
        assert!(zai.web_search(&params).await.unwrap().contains("[1] Z.AI"));
        let bigmodel = ZaiToolsClient::new("k", server.uri())
            .unwrap()
            .with_default_engine(BIGMODEL_SEARCH_ENGINE);
        assert!(
            bigmodel
                .web_search(&params)
                .await
                .unwrap()
                .contains("[1] BigModel")
        );
    }

    #[test]
    fn test_format_web_search_response() {
        let json = json!({
            "search_intent": {
                "query": "rust programming",
                "intent": "reference",
                "keywords": "rust programming"
            },
            "search_result": [
                { "title": "Rust", "link": "https://rust-lang.org", "content": "A systems language." }
            ]
        });
        let text = format_web_search_response(&json);
        assert!(text.contains("Search intent: reference"));
        assert!(text.contains("Rust"));
        assert!(text.contains("https://rust-lang.org"));
        assert!(text.contains("Summary: A systems language."));
    }

    #[test]
    fn test_format_web_search_response_includes_media_and_date() {
        let json = json!({
            "search_result": [
                {
                    "title": "News",
                    "link": "https://example.com",
                    "media": "Example Times",
                    "publish_date": "2026-06-24",
                    "content": "Headline"
                }
            ]
        });
        let text = format_web_search_response(&json);
        assert!(text.contains("Source: Example Times"));
        assert!(text.contains("Date: 2026-06-24"));
    }

    #[test]
    fn test_format_reader_response() {
        let json = json!({
            "reader_result": {
                "title": "Page title",
                "url": "https://example.com",
                "description": "A page",
                "content": "Page body"
            }
        });
        let text = format_reader_response(&json);
        assert!(text.contains("Page title"));
        assert!(text.contains("https://example.com"));
        assert!(text.contains("Page body"));
    }
}
