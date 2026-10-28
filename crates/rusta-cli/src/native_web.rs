//! The provider's own web search and page reader, where it has them: Z.AI's
//! `/web_search` and `/reader`, the Kimi Coding Plan's `/search` and
//! `/fetch`, or DeepSeek's Search and Fetch APIs. They back the built-in
//! `web_search` (and `$web_search`) and `web_reader` tools.

use std::sync::Arc;

use agent_core::web_reader_client::WebReaderClient;
use agent_core::web_search_client::WebSearchClient;
use async_trait::async_trait;
use lm_bridge::providers::deepseek_tools::DeepseekTools;
use lm_bridge::providers::kimi_services::{KimiServices, format_hits};
use lm_bridge::zai_tools::{BIGMODEL_SEARCH_ENGINE, ZAI_SEARCH_ENGINE};

use crate::cli::Cli;
use crate::endpoints::{self, Billing, Region};

/// How long the local fallback of a failed service fetch may take.
const LOCAL_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The clients a provider brings; `None` where it has none.
#[derive(Default)]
pub struct NativeWeb {
    pub search: Option<Arc<dyn WebSearchClient>>,
    pub reader: Option<Arc<dyn WebReaderClient>>,
}

/// The native web clients for `provider` under these settings.
///
/// A configured `search_url` stays the search backend for Kimi: it is an
/// explicit choice over the plan's own search. A GLM Coding Plan brings none:
/// Z.AI's `/web_search` and `/reader` are paid per use and answer a Coding
/// Plan key "Insufficient balance" (1113); the plan searches and reads
/// through its hosted MCP servers instead (see `zai_mcp`). DeepSeek's
/// Search and Fetch serve every DeepSeek key, paid per use; a configured
/// `search_url` stays its search backend too.
pub fn clients(cli: &Cli, provider: &str) -> NativeWeb {
    match provider {
        "glm" | "zai" if cli.billing.unwrap_or_default() == Billing::Api => zai_clients(cli),
        "kimi" if cli.billing.unwrap_or_default() == Billing::Subscription => kimi_clients(cli),
        "deepseek" => deepseek_clients(cli),
        _ => NativeWeb::default(),
    }
}

fn zai_clients(cli: &Cli) -> NativeWeb {
    let key = cli
        .api_key
        .clone()
        .or_else(|| endpoints::env_non_empty("RUSTA_ZAI_API_KEY"))
        .or_else(|| endpoints::env_non_empty("ZAI_API_KEY"))
        .or_else(|| endpoints::env_non_empty("RUSTA_API_KEY"));
    let Some(key) = key else {
        return NativeWeb::default();
    };
    let region = cli.region.unwrap_or_default();
    let base = endpoints::zai_tools_base_url(cli.base_url.as_deref(), region);
    NativeWeb {
        search: lm_bridge::ZaiToolsClient::new(&key, &base)
            .ok()
            .map(|client| {
                Arc::new(crate::zai_search::ZaiWebSearchClient::new(
                    client.with_default_engine(search_engine(region)),
                )) as Arc<dyn WebSearchClient>
            }),
        reader: lm_bridge::ZaiToolsClient::new(&key, &base)
            .ok()
            .map(|client| {
                Arc::new(crate::zai_reader::ZaiWebReaderClient::new(client))
                    as Arc<dyn WebReaderClient>
            }),
    }
}

/// The `/web_search` engine of the platform `region` names.
fn search_engine(region: Region) -> &'static str {
    match region {
        Region::Global => ZAI_SEARCH_ENGINE,
        Region::China => BIGMODEL_SEARCH_ENGINE,
    }
}

/// The Kimi Coding Plan's own services on the session's host, key and
/// custom headers; `None` without a key.
pub fn kimi_services(cli: &Cli) -> Option<KimiServices> {
    let key = crate::resolve_api_key(cli, "kimi")?;
    let base = crate::resolve_base_url(
        cli,
        "kimi",
        cli.billing.unwrap_or_default(),
        cli.region.unwrap_or_default(),
    );
    let headers = crate::custom_headers(cli, "kimi").unwrap_or_default();
    KimiServices::new(&base, &key, headers).ok()
}

fn kimi_clients(cli: &Cli) -> NativeWeb {
    let Some(services) = kimi_services(cli) else {
        return NativeWeb::default();
    };
    let services = Arc::new(services);
    NativeWeb {
        search: cli.search_url.is_none().then(|| {
            Arc::new(KimiWebSearchClient {
                services: Arc::clone(&services),
            }) as Arc<dyn WebSearchClient>
        }),
        reader: Some(Arc::new(ServiceReader(services))),
    }
}

fn deepseek_clients(cli: &Cli) -> NativeWeb {
    let Some(key) = crate::resolve_api_key(cli, "deepseek") else {
        return NativeWeb::default();
    };
    let base = crate::resolve_base_url(
        cli,
        "deepseek",
        cli.billing.unwrap_or_default(),
        cli.region.unwrap_or_default(),
    );
    let Ok(tools) = DeepseekTools::new(&base, &key) else {
        return NativeWeb::default();
    };
    let tools = Arc::new(tools);
    NativeWeb {
        search: cli.search_url.is_none().then(|| {
            Arc::new(DeepseekWebSearchClient {
                tools: Arc::clone(&tools),
            }) as Arc<dyn WebSearchClient>
        }),
        reader: Some(Arc::new(ServiceReader(tools))),
    }
}

/// `web_search` through the Kimi Coding Plan's `/search`.
pub struct KimiWebSearchClient {
    services: Arc<KimiServices>,
}

#[async_trait]
impl WebSearchClient for KimiWebSearchClient {
    async fn search(&self, query: &str, _question: Option<&str>) -> Result<String, String> {
        self.services
            .search(query)
            .await
            .map(|hits| format_hits(query, &hits))
            .map_err(|e| e.to_string())
    }
}

/// `web_search` through DeepSeek's Search API, which also hears the
/// prompt behind the query.
pub struct DeepseekWebSearchClient {
    tools: Arc<DeepseekTools>,
}

#[async_trait]
impl WebSearchClient for DeepseekWebSearchClient {
    async fn search(&self, query: &str, question: Option<&str>) -> Result<String, String> {
        self.tools
            .search(query, question)
            .await
            .map(|hits| format_hits(query, &hits))
            .map_err(|e| e.to_string())
    }
}

/// A provider's service that renders a page as Markdown: the Kimi Coding
/// Plan's `/fetch`, or DeepSeek's Fetch API.
#[async_trait]
trait PageService: Send + Sync {
    async fn fetch_page(&self, url: &str) -> Result<String, lm_bridge::errors::LlmError>;
}

#[async_trait]
impl PageService for KimiServices {
    async fn fetch_page(&self, url: &str) -> Result<String, lm_bridge::errors::LlmError> {
        self.fetch(url).await
    }
}

#[async_trait]
impl PageService for DeepseekTools {
    async fn fetch_page(&self, url: &str) -> Result<String, lm_bridge::errors::LlmError> {
        self.fetch(url).await
    }
}

/// `web_reader` through a provider's page service; a failed fetch falls
/// back to fetching the page directly.
struct ServiceReader<S>(Arc<S>);

#[async_trait]
impl<S: PageService + 'static> WebReaderClient for ServiceReader<S> {
    async fn read(&self, url: &str) -> Result<String, String> {
        match self.0.fetch_page(url).await {
            Ok(markdown) => Ok(markdown),
            Err(service) => {
                tracing::debug!("{service}; fetching {url} directly");
                agent_core::tools::builtin::fetch::fetch_url_text(url, LOCAL_FETCH_TIMEOUT)
                    .await
                    .map_err(|local| format!("{service}; the direct fetch failed too: {local}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kimi_cli(search_url: Option<&str>) -> Cli {
        Cli {
            provider: Some("kimi".into()),
            api_key: Some("sk-test".into()),
            search_url: search_url.map(str::to_string),
            ..Cli::default()
        }
    }

    /// The Coding Plan brings both clients; a configured `search_url` keeps
    /// the search backend, and pay-per-token Kimi has no services.
    #[test]
    fn kimi_coding_plan_brings_search_and_reader() {
        let web = clients(&kimi_cli(None), "kimi");
        assert!(web.search.is_some() && web.reader.is_some());

        let web = clients(&kimi_cli(Some("https://s.example/?q={query}")), "kimi");
        assert!(web.search.is_none(), "search_url overrides");
        assert!(web.reader.is_some());

        let mut api = kimi_cli(None);
        api.billing = Some(Billing::Api);
        let web = clients(&api, "kimi");
        assert!(web.search.is_none() && web.reader.is_none());

        let web = clients(&kimi_cli(None), "openai");
        assert!(web.search.is_none() && web.reader.is_none());
    }

    fn glm_cli(billing: Billing, region: Region, base_url: Option<String>) -> Cli {
        Cli {
            provider: Some("glm".into()),
            api_key: Some("sk-test".into()),
            billing: Some(billing),
            region: Some(region),
            base_url,
            ..Cli::default()
        }
    }

    /// Z.AI's `/web_search` and `/reader` serve pay-per-token keys; a Coding
    /// Plan key gets "Insufficient balance" from them, and its plan's hosted
    /// MCP servers instead.
    #[test]
    fn glm_brings_the_pay_per_use_clients_to_a_pay_per_token_key_only() {
        let plan = clients(&glm_cli(Billing::Subscription, Region::Global, None), "glm");
        assert!(plan.search.is_none() && plan.reader.is_none());
        let api = clients(&glm_cli(Billing::Api, Region::Global, None), "glm");
        assert!(api.search.is_some() && api.reader.is_some());
    }

    /// The search names the engine of the key's platform.
    #[tokio::test]
    async fn a_glm_search_uses_its_platforms_engine() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        for engine in [ZAI_SEARCH_ENGINE, BIGMODEL_SEARCH_ENGINE] {
            Mock::given(method("POST"))
                .and(path("/web_search"))
                .and(body_partial_json(
                    serde_json::json!({"search_engine": engine}),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "search_result": [{"title": engine, "link": "https://t.example"}]
                })))
                .mount(&server)
                .await;
        }
        for (region, engine) in [
            (Region::Global, ZAI_SEARCH_ENGINE),
            (Region::China, BIGMODEL_SEARCH_ENGINE),
        ] {
            let web = clients(&glm_cli(Billing::Api, region, Some(server.uri())), "glm");
            let text = web.search.unwrap().search("q", None).await.unwrap();
            assert!(text.starts_with(&format!("[1] {engine}\n")), "{text}");
        }
    }

    /// Search results reach the tool rendered; a failed service fetch falls
    /// back to fetching directly.
    #[tokio::test]
    async fn the_kimi_clients_use_the_services() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "search_results": [{"title": "T", "url": "https://t.example", "snippet": "S"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/fetch"))
            .respond_with(ResponseTemplate::new(502).set_body_string("upstream down"))
            .mount(&server)
            .await;
        let services = Arc::new(
            KimiServices::new(&server.uri(), "sk-test", reqwest::header::HeaderMap::new()).unwrap(),
        );

        let search = KimiWebSearchClient {
            services: Arc::clone(&services),
        };
        let text = search.search("q", None).await.unwrap();
        assert!(
            text.starts_with("Title: T\nURL: https://t.example\nSnippet: S"),
            "{text}"
        );

        let reader = ServiceReader(services);
        // The direct fallback refuses a loopback address, so both failures
        // are reported.
        let err = reader.read("http://127.0.0.1:9/page").await.unwrap_err();
        assert!(err.contains("Kimi fetch failed: HTTP 502"), "{err}");
        assert!(err.contains("the direct fetch failed too"), "{err}");
    }

    fn deepseek_cli(search_url: Option<&str>, base_url: Option<String>) -> Cli {
        Cli {
            provider: Some("deepseek".into()),
            api_key: Some("sk-test".into()),
            search_url: search_url.map(str::to_string),
            base_url,
            ..Cli::default()
        }
    }

    /// DeepSeek's Search and Fetch serve every DeepSeek key; a configured
    /// `search_url` keeps the search backend, as on Kimi.
    #[test]
    fn deepseek_brings_search_and_reader() {
        let web = clients(&deepseek_cli(None, None), "deepseek");
        assert!(web.search.is_some() && web.reader.is_some());

        let web = clients(
            &deepseek_cli(Some("https://s.example/?q={query}"), None),
            "deepseek",
        );
        assert!(web.search.is_none(), "search_url overrides");
        assert!(web.reader.is_some());
    }

    /// The DeepSeek clients go to the session's host: a search carries the
    /// prompt behind it, its results reach the tool rendered, and a failed
    /// fetch falls back to fetching directly.
    #[tokio::test]
    async fn the_deepseek_clients_use_its_tools() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .and(body_json(serde_json::json!({
                "query": "q",
                "ori_question": "Why does Luhn double every second digit?"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"title": "T", "url": "https://t.example", "summary": "S"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/fetch"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {"message": "no usable page content", "param": null}
            })))
            .mount(&server)
            .await;
        let web = clients(&deepseek_cli(None, Some(server.uri())), "deepseek");

        let text = web
            .search
            .unwrap()
            .search("q", Some("Why does Luhn double every second digit?"))
            .await
            .unwrap();
        assert!(
            text.starts_with("Title: T\nURL: https://t.example\nSnippet: S"),
            "{text}"
        );
        let err = web
            .reader
            .unwrap()
            .read("http://127.0.0.1:9/page")
            .await
            .unwrap_err();
        assert!(
            err.contains("DeepSeek fetch failed: HTTP 404 Not Found. no usable page content"),
            "{err}"
        );
        assert!(err.contains("the direct fetch failed too"), "{err}");
    }
}
