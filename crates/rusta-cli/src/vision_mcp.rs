//! MCP-based vision fallback for non-vision GLM models.
//!
//! Mirrors `glm-acp-agent/src/tools/vision-mcp-client.ts`: spawns a Z.AI
//! vision MCP server and delegates image description to its `analyze_image`.
//! Its `analyze_video` also serves models that read images but not video.

use std::io::Write;

use agent_core::vision_fallback::VisionFallback;
use async_trait::async_trait;
use base64::Engine;
use lm_bridge::providers::glm::GlmChatProvider;
use mcp_client::{
    manager::McpConnectionManager,
    mcp_arg_remap::{remap_arguments, resolve_tool_name},
};
use serde_json::json;

use crate::endpoints::{Billing, Region};

/// The name the Z.AI vision server is registered under.
pub const SERVER_NAME: &str = "zai_vision";

/// Startup budget for the npx-launched vision server. Its first run downloads
/// `@z_ai/mcp-server`, which routinely outlasts the 30 s default.
const NPX_COLD_START_TIMEOUT_MS: u64 = 120_000;

/// Per-call budget for vision analysis: a base64 image is uploaded and a model
/// describes it, which is slower than a typical tool round-trip.
const VISION_TOOL_TIMEOUT_MS: u64 = 120_000;

/// What the vision server is asked about an image (glm-acp-agent's prompt).
const DESCRIBE_PROMPT: &str = "Describe this image in detail, including any text, code, UI elements, or other visible \
     content.";

/// Vision fallback that calls an MCP server.
pub struct McpVisionFallback {
    manager: McpConnectionManager,
    server_name: String,
    /// The server runs elsewhere (`Z_AI_VISION_MCP_URL`), so it cannot read
    /// a file on this machine and gets the image inline.
    remote: bool,
}

impl McpVisionFallback {
    pub fn new(
        manager: McpConnectionManager,
        server_name: impl Into<String>,
        remote: bool,
    ) -> Self {
        Self {
            manager,
            server_name: server_name.into(),
            remote,
        }
    }
}

/// The image written to a temporary file named for its format, which the
/// vision server reads: Z.AI's takes `image_source` as a local `.png` or
/// `.jpg` path, or an http(s) URL, never a `data:` URL. Deleted when dropped.
fn image_file(mime: &str, base64_data: &str) -> Result<tempfile::NamedTempFile, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .map_err(|e| format!("the image is not valid base64: {e}"))?;
    let suffix = if mime == "image/jpeg" { ".jpg" } else { ".png" };
    let mut file = tempfile::Builder::new()
        .prefix("rusta-vision-")
        .suffix(suffix)
        .tempfile()
        .map_err(|e| format!("could not stage the image for the vision server: {e}"))?;
    file.write_all(&bytes)
        .map_err(|e| format!("could not stage the image for the vision server: {e}"))?;
    Ok(file)
}

#[async_trait]
impl VisionFallback for McpVisionFallback {
    async fn describe_image(
        &self,
        mime: &str,
        base64_data: &str,
        question: Option<&str>,
    ) -> Result<String, String> {
        let server = self
            .manager
            .get(&self.server_name)
            .ok_or_else(|| format!("vision server '{}' not found", self.server_name))?;

        // Tools are called by the server's own names, not the qualified ones
        // the model sees. Z.AI's general tool is `analyze_image`.
        let names = server.tool_names().await;
        let tool_name = ["analyze_image", "image_analysis"]
            .into_iter()
            .find(|name| names.iter().any(|n| n == name))
            .map(str::to_string)
            .or_else(|| resolve_tool_name("image_analysis", &names))
            .ok_or_else(|| "no image analysis tool found on the vision server".to_string())?;

        let staged = if self.remote {
            None
        } else {
            Some(image_file(mime, base64_data)?)
        };
        let image_source = match &staged {
            Some(file) => file.path().to_string_lossy().into_owned(),
            None => format!("data:{mime};base64,{base64_data}"),
        };
        let prompt = agent_core::vision_fallback::vision_request(DESCRIBE_PROMPT, question);
        let args = json!({ "image_source": image_source, "prompt": prompt });
        let properties = server.properties_for(&tool_name).await;
        let args = remap_arguments(&args, &properties);

        let output = server
            .call_tool(&tool_name, Some(args))
            .await
            .map_err(|e| format!("vision MCP call failed: {e}"))?;

        // A vision server reports a refused or failed analysis with `isError`,
        // not a protocol error. Returning its text as the description would put
        // the failure message into the transcript as if it described the image.
        if output.is_error {
            return Err(format!("vision MCP call failed: {}", output.text()));
        }
        Ok(output.text())
    }
}

/// Whether a GLM model needs the vision server. GLM-5.3-Flash reads images
/// and video itself; an image-only model still needs `analyze_video`, and a
/// text model needs both tools.
#[must_use]
pub fn needs_vision_server(model: &str) -> bool {
    let policy = GlmChatProvider::media_policy_for(model);
    !(policy.takes_images() && policy.takes_video())
}

/// The GLM models a session may run: the main model, and each sub-agent pool
/// entry on a GLM provider (an entry without a provider inherits the session's).
#[must_use]
pub fn session_models(
    cli: &crate::cli::Cli,
    provider: &str,
    pool: &crate::subagent_pool::SubagentConfig,
) -> Vec<String> {
    let is_glm = |p: &str| matches!(p, "glm" | "zai");
    let mut models = Vec::new();
    if is_glm(provider) {
        let billing = cli.billing.unwrap_or_default();
        if let Some(model) =
            crate::provider_defaults::resolve_model(provider, cli.model.as_deref(), billing).model()
        {
            models.push(model.to_string());
        }
    }
    for entry in pool.models.values() {
        if is_glm(entry.provider.as_deref().unwrap_or(provider)) {
            models.push(entry.model.clone());
        }
    }
    models
}

/// The Vision MCP server's platform: `ZAI` for api.z.ai keys, `ZHIPU` for
/// open.bigmodel.cn keys. The server refuses a key from the other platform.
#[must_use]
pub fn platform_mode(region: Region) -> &'static str {
    match region {
        Region::Global => "ZAI",
        Region::China => "ZHIPU",
    }
}

/// Auto-register a Z.AI vision MCP server when the provider is GLM/Z.AI, a
/// vision server is not already configured, and one of `models` — the GLM
/// models the session may run — cannot read images and video itself.
///
/// Z.AI's own server takes a Coding Plan key, so it is started on that plan
/// only; a server named by `Z_AI_VISION_MCP_URL` is used on any.
pub fn inject_zai_vision_server(
    provider: &str,
    billing: Billing,
    api_key: Option<&str>,
    region: Region,
    models: &[String],
    servers: &mut Vec<mcp_client::McpServerConfig>,
    defaults: &mcp_client::McpDefaults,
) {
    let remote_url = crate::endpoints::env_non_empty("Z_AI_VISION_MCP_URL");
    inject(
        provider, billing, api_key, region, models, remote_url, servers, defaults,
    );
}

/// [`inject_zai_vision_server`], given the remote server's URL
/// (`Z_AI_VISION_MCP_URL`) when there is one. Its timeouts are the global
/// `[mcp]` ones in `defaults` where set, else the npx server's own or the
/// built-in ones.
#[allow(clippy::too_many_arguments)]
fn inject(
    provider: &str,
    billing: Billing,
    api_key: Option<&str>,
    region: Region,
    models: &[String],
    remote_url: Option<String>,
    servers: &mut Vec<mcp_client::McpServerConfig>,
    defaults: &mcp_client::McpDefaults,
) {
    if !matches!(provider, "glm" | "zai") {
        return;
    }
    if servers.iter().any(|s| s.name == SERVER_NAME) {
        return;
    }
    if !models.iter().any(|m| needs_vision_server(m)) {
        tracing::info!(
            "[mcp:{SERVER_NAME}] not started: {} read images and video natively",
            models.join(", ")
        );
        return;
    }

    // Only auto-inject if the user has a Coding Plan key, or if they
    // explicitly pointed us at a vision MCP URL.
    let has_key = api_key.is_some() && billing == Billing::Subscription;
    if !has_key && remote_url.is_none() {
        return;
    }

    let server = if let Some(url) = remote_url {
        mcp_client::McpServerConfig {
            name: SERVER_NAME.to_string(),
            command: None,
            args: Vec::new(),
            url: Some(url),
            token: api_key.map(ToString::to_string),
            startup_timeout_ms: defaults.startup_timeout_ms,
            tool_timeout_ms: defaults.tool_timeout_ms,
            ..Default::default()
        }
    } else {
        // The `@z_ai/mcp-server` package reads its credential from the
        // `Z_AI_API_KEY` environment variable of its own process — not a bearer
        // token or an argument. Rusta's key may live only in the config file
        // (after `rusta init`) or under a different env name (`ZAI_API_KEY`),
        // so pass it explicitly to this server rather than relying on the
        // ambient environment, which otherwise leaves the vision server dead
        // with "Z_AI_API_KEY environment variable is required".
        let mut env = std::collections::HashMap::new();
        if let Some(key) = api_key {
            env.insert("Z_AI_API_KEY".to_string(), key.to_string());
        }
        // Required by the server: which platform issued the key.
        env.insert("Z_AI_MODE".to_string(), platform_mode(region).to_string());
        mcp_client::McpServerConfig {
            name: SERVER_NAME.to_string(),
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "@z_ai/mcp-server@latest".to_string()],
            url: None,
            token: None,
            env,
            // `npx -y` fetches the package on first use, so a cold start is a
            // download, not a process spawn. The 30 s default is a fine budget
            // for an already-installed server and far too short for that first
            // run, where it surfaces as an unexplained startup failure.
            startup_timeout_ms: Some(defaults.startup_or(NPX_COLD_START_TIMEOUT_MS)),
            tool_timeout_ms: Some(defaults.tool_or(VISION_TOOL_TIMEOUT_MS)),
            ..Default::default()
        }
    };
    servers.push(server);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-pixel PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    /// The fixture standing in for Z.AI's vision server, connected.
    async fn vision_server() -> McpConnectionManager {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vision_server.py");
        let config = mcp_client::McpServerConfig {
            name: SERVER_NAME.to_string(),
            command: Some("python3".to_string()),
            args: vec![fixture.display().to_string()],
            startup_timeout_ms: Some(5_000),
            tool_timeout_ms: Some(5_000),
            ..Default::default()
        };
        let (manager, _events) = McpConnectionManager::new(vec![config], Default::default());
        manager.start_all().await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !matches!(
            manager.statuses().await.first(),
            Some((_, mcp_client::manager::McpStatus::Connected))
        ) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fixture never connected"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        manager
    }

    /// The image reaches Z.AI's `analyze_image`, called by the server's own
    /// name, as a local file it can read, with a prompt; the file is gone
    /// once the description is back.
    #[tokio::test]
    async fn the_vision_server_reads_the_image_from_a_local_file() {
        let manager = vision_server().await;
        let fallback = McpVisionFallback::new(manager.clone(), SERVER_NAME, false);

        let description = fallback
            .describe_image("image/png", PNG, None)
            .await
            .unwrap();

        let staged = description
            .strip_prefix("analyzed ")
            .and_then(|rest| rest.split(" (").next())
            .expect("the fixture names the file it read");
        assert!(staged.ends_with(".png"), "{description}");
        assert!(
            description.ends_with(&format!("(70 bytes): {DESCRIBE_PROMPT}")),
            "{description}"
        );
        assert!(
            !std::path::Path::new(staged).exists(),
            "the staged image is removed"
        );
        manager.stop_all();
    }

    /// A failed analysis is an error, not a description: a remote server
    /// gets the image inline, which the fixture, like Z.AI's own server,
    /// refuses.
    #[tokio::test]
    async fn a_refused_analysis_is_an_error() {
        let manager = vision_server().await;
        let fallback = McpVisionFallback::new(manager.clone(), SERVER_NAME, true);

        let err = fallback
            .describe_image("image/png", PNG, None)
            .await
            .unwrap_err();

        assert!(err.contains("Image file not found"), "{err}");
        manager.stop_all();
    }

    fn text_model() -> Vec<String> {
        vec!["glm-5.3".to_string()]
    }

    #[test]
    fn test_skips_injection_for_non_glm() {
        let mut servers = Vec::new();
        inject(
            "kimi",
            Billing::Subscription,
            Some("key"),
            Region::Global,
            &text_model(),
            None,
            &mut servers,
            &mcp_client::McpDefaults::default(),
        );
        assert!(servers.is_empty());
    }

    /// GLM-5.3-Flash reads images and video itself, so the server is not
    /// started for it — unless another model in the session needs it.
    #[test]
    fn a_native_media_model_does_not_start_the_server() {
        let mut servers = Vec::new();
        let flash = vec!["glm-5.3-flash".to_string()];
        inject(
            "glm",
            Billing::Subscription,
            Some("key"),
            Region::Global,
            &flash,
            None,
            &mut servers,
            &mcp_client::McpDefaults::default(),
        );
        assert!(servers.is_empty());

        let with_pool = vec!["glm-5.3-flash".to_string(), "glm-5.3".to_string()];
        inject(
            "glm",
            Billing::Subscription,
            Some("key"),
            Region::Global,
            &with_pool,
            None,
            &mut servers,
            &mcp_client::McpDefaults::default(),
        );
        assert_eq!(servers.len(), 1, "a text-only pool model needs the server");

        assert!(needs_vision_server("glm-4.6v"), "image-only: analyze_video");
        assert!(needs_vision_server("glm-5.3"));
        assert!(!needs_vision_server("glm-5.3-flash"));
    }

    /// The pool's GLM models count; a pool entry on another provider does not.
    #[test]
    fn session_models_cover_the_main_model_and_the_glm_pool() {
        use crate::subagent_pool::{SubagentConfig, SubagentModel};
        let cli = crate::cli::Cli {
            model: Some("glm-5.3-flash".to_string()),
            ..crate::cli::Cli::default()
        };
        let mut pool = SubagentConfig::default();
        pool.models.insert(
            "fast".to_string(),
            SubagentModel {
                model: "glm-5.3".to_string(),
                ..SubagentModel::default()
            },
        );
        pool.models.insert(
            "kimi".to_string(),
            SubagentModel {
                model: "k3".to_string(),
                provider: Some("kimi".to_string()),
                ..SubagentModel::default()
            },
        );
        assert_eq!(
            session_models(&cli, "glm", &pool),
            ["glm-5.3-flash", "glm-5.3"]
        );
        assert_eq!(session_models(&cli, "kimi", &pool), Vec::<String>::new());
    }

    #[test]
    fn test_injects_when_api_key_present() {
        let mut servers = Vec::new();
        inject(
            "glm",
            Billing::Subscription,
            Some("key"),
            Region::Global,
            &text_model(),
            None,
            &mut servers,
            &mcp_client::McpDefaults::default(),
        );
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "zai_vision");
        // The server refuses to start without its platform.
        assert_eq!(
            servers[0].env.get("Z_AI_MODE").map(String::as_str),
            Some("ZAI")
        );
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
        // The key must reach the spawned server as Z_AI_API_KEY, or it dies with
        // "Z_AI_API_KEY environment variable is required".
        assert_eq!(
            servers[0].env.get("Z_AI_API_KEY").map(String::as_str),
            Some("key")
        );
        // A cold `npx -y` start downloads the package; the 30 s default would
        // time out before it ever speaks.
        assert_eq!(
            servers[0].startup_timeout_ms,
            Some(NPX_COLD_START_TIMEOUT_MS)
        );
        assert_eq!(servers[0].tool_timeout_ms, Some(VISION_TOOL_TIMEOUT_MS));
    }

    #[test]
    fn test_preserves_existing_server() {
        let mut servers = vec![mcp_client::McpServerConfig {
            name: "zai_vision".to_string(),
            command: Some("custom".to_string()),
            args: Vec::new(),
            url: None,
            token: None,
            ..Default::default()
        }];
        inject(
            "glm",
            Billing::Subscription,
            Some("key"),
            Region::Global,
            &text_model(),
            None,
            &mut servers,
            &mcp_client::McpDefaults::default(),
        );
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].command.as_deref(), Some("custom"));
    }

    /// Z.AI's own server takes a Coding Plan key; a server the user named with
    /// `Z_AI_VISION_MCP_URL` is used on any plan.
    #[test]
    fn a_pay_per_token_key_starts_only_a_named_server() {
        let mut servers = Vec::new();
        let run = |servers: &mut Vec<mcp_client::McpServerConfig>, url: Option<&str>| {
            inject(
                "glm",
                Billing::Api,
                Some("key"),
                Region::Global,
                &text_model(),
                url.map(str::to_string),
                servers,
                &mcp_client::McpDefaults::default(),
            );
        };
        run(&mut servers, None);
        assert!(servers.is_empty());

        run(&mut servers, Some("https://vision.example/mcp"));
        assert_eq!(servers.len(), 1);
        assert_eq!(
            servers[0].url.as_deref(),
            Some("https://vision.example/mcp")
        );
    }

    /// A mainland key belongs to the ZHIPU platform.
    #[test]
    fn a_china_region_key_runs_on_zhipu() {
        let mut servers = Vec::new();
        inject(
            "glm",
            Billing::Subscription,
            Some("key"),
            Region::China,
            &text_model(),
            None,
            &mut servers,
            &mcp_client::McpDefaults::default(),
        );
        assert_eq!(
            servers[0].env.get("Z_AI_MODE").map(String::as_str),
            Some("ZHIPU")
        );
    }

    /// The global `[mcp]` timeouts reach the server as they do a configured
    /// one: over the npx server's own, and in place of the built-in ones for a
    /// remote server.
    #[test]
    fn the_global_timeouts_reach_the_server() {
        let global = mcp_client::McpDefaults {
            startup_timeout_ms: Some(5_000),
            tool_timeout_ms: Some(600_000),
            ..mcp_client::McpDefaults::default()
        };
        for url in [None, Some("https://vision.example/mcp".to_string())] {
            let mut servers = Vec::new();
            inject(
                "glm",
                Billing::Subscription,
                Some("key"),
                Region::Global,
                &text_model(),
                url.clone(),
                &mut servers,
                &global,
            );
            assert_eq!(servers[0].startup_timeout_ms, Some(5_000), "{url:?}");
            assert_eq!(servers[0].tool_timeout_ms, Some(600_000), "{url:?}");
        }

        let mut servers = Vec::new();
        inject(
            "glm",
            Billing::Subscription,
            Some("key"),
            Region::Global,
            &text_model(),
            Some("https://vision.example/mcp".to_string()),
            &mut servers,
            &mcp_client::McpDefaults::default(),
        );
        assert_eq!(servers[0].startup_timeout_ms, None, "the built-in budget");
        assert_eq!(servers[0].tool_timeout_ms, None);
    }
}
