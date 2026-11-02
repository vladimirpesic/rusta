//! Which vision fallback a session gets, for describing images to a model
//! that cannot see them: on DeepSeek its own vision model, `deepseek-flash`,
//! asked once per image; otherwise Z.AI's vision MCP server where it is
//! configured ([`crate::vision_mcp`]). `read_media` consults it only while the
//! session's model takes no images, such as `deepseek-v4-pro` or `glm-5.3`.

use std::sync::Arc;

use agent_core::agent_loop::llm::ProviderLlm;
use agent_core::vision_fallback::{LlmVisionFallback, VisionFallback};
use lm_bridge::provider::{ChatProvider, ThinkingConfig};
use lm_bridge::providers::DeepseekChatProvider;
use lm_bridge::providers::deepseek_catalog::DeepseekModelInfo;
use lm_bridge::providers::openai_compat::HttpTimeouts;
use mcp_client::manager::McpConnectionManager;

use crate::cli::Cli;
use crate::config::Config;
use crate::vision_mcp;

/// The DeepSeek model that describes images for a DeepSeek model that does
/// not take them.
pub const DEEPSEEK_VISION_MODEL: &str = "deepseek-flash";

/// The vision fallback for a session on `provider`, or `None` when there is
/// none to be had.
#[must_use]
pub fn for_provider(
    cli: &Cli,
    provider: &str,
    config: &Config,
    mcp: &McpConnectionManager,
    timeouts: HttpTimeouts,
) -> Option<Arc<dyn VisionFallback>> {
    if provider == "deepseek"
        && let Some(fallback) = deepseek(cli, timeouts)
    {
        return Some(fallback);
    }
    let server = config
        .mcp_servers
        .iter()
        .find(|server| server.name == vision_mcp::SERVER_NAME && !server.disabled)?;
    Some(Arc::new(vision_mcp::McpVisionFallback::new(
        mcp.clone(),
        vision_mcp::SERVER_NAME,
        server.url.is_some(),
    )))
}

/// DeepSeek's vision model on the session's endpoint and key, with thinking
/// off: a description needs no reasoning, and goes faster and cheaper
/// without it. `None` without a key, or when the catalogue says the model
/// does not take images after all.
fn deepseek(cli: &Cli, timeouts: HttpTimeouts) -> Option<Arc<dyn VisionFallback>> {
    let key = crate::resolve_api_key(cli, "deepseek")?;
    let base_url = crate::resolve_base_url(
        cli,
        "deepseek",
        cli.billing.unwrap_or_default(),
        cli.region.unwrap_or_default(),
    );
    let headers = crate::custom_headers(cli, "deepseek").ok()?;
    let cached = crate::catalogs::cached_models::<DeepseekModelInfo>(&base_url);
    let mut provider =
        DeepseekChatProvider::new(DEEPSEEK_VISION_MODEL, Some(key), base_url, timeouts).ok()?;
    if let Some(models) = cached {
        provider = provider.with_catalog(models);
    }
    if !provider.supports_vision() {
        return None;
    }
    let llm = ProviderLlm::new(Box::new(provider.with_custom_headers(&headers)))
        .with_thinking(ThinkingConfig::Disabled);
    Some(Arc::new(LlmVisionFallback::new(Arc::new(llm))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deepseek_cli(base_url: Option<String>) -> Cli {
        Cli {
            provider: Some("deepseek".to_string()),
            api_key: Some("sk-test".to_string()),
            base_url,
            ..Cli::default()
        }
    }

    fn timeouts() -> HttpTimeouts {
        HttpTimeouts::idle(std::time::Duration::from_secs(5))
    }

    fn no_mcp() -> McpConnectionManager {
        McpConnectionManager::new(Vec::new(), Default::default()).0
    }

    /// DeepSeek gets its own vision model; another provider gets Z.AI's
    /// vision server only where one is configured.
    #[test]
    fn the_fallback_follows_the_provider() {
        let config = Config::default();
        let mcp = no_mcp();
        assert!(for_provider(&deepseek_cli(None), "deepseek", &config, &mcp, timeouts()).is_some());

        let kimi = Cli {
            provider: Some("kimi".to_string()),
            ..Cli::default()
        };
        assert!(for_provider(&kimi, "kimi", &config, &mcp, timeouts()).is_none());

        let mut with_vision = Config::default();
        with_vision.mcp_servers.push(mcp_client::McpServerConfig {
            name: vision_mcp::SERVER_NAME.to_string(),
            command: Some("npx".to_string()),
            ..Default::default()
        });
        assert!(for_provider(&kimi, "glm", &with_vision, &mcp, timeouts()).is_some());
    }

    /// The DeepSeek fallback asks `deepseek-flash`, thinking off, with the
    /// image attached, and its answer is the description.
    #[tokio::test]
    async fn deepseek_describes_images_with_its_vision_model() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
            serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": DEEPSEEK_VISION_MODEL,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
        };
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            chunk(
                serde_json::json!({"role": "assistant", "content": "a red square"}),
                serde_json::Value::Null
            ),
            chunk(serde_json::json!({}), serde_json::json!("stop")),
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let fallback = for_provider(
            &deepseek_cli(Some(server.uri())),
            "deepseek",
            &Config::default(),
            &no_mcp(),
            timeouts(),
        )
        .expect("a DeepSeek fallback");
        let description = fallback
            .describe_image("image/png", "AAAA", None)
            .await
            .unwrap();

        assert_eq!(description, "a red square");
        let requests = server.received_requests().await.unwrap();
        let request: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(request["model"], DEEPSEEK_VISION_MODEL);
        assert_eq!(request["thinking"], serde_json::json!({"type": "disabled"}));
        let parts = request["messages"][0]["content"].as_array().unwrap();
        assert!(
            parts
                .iter()
                .any(|part| part["image_url"]["url"] == "data:image/png;base64,AAAA"),
            "{request}"
        );
    }
}
