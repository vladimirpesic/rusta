//! How MCP tool results reach the model: every content shape a server can
//! send, through the real client and the `McpTool` adapter, against the
//! fixture server in `tests/fixtures/echo_server.py`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::agent_loop::Llm;
use agent_core::agent_loop::types::{ExecutableTool, ExecutableToolContext, ExecutableToolResult};
use agent_core::tool_output::ToolOutputPolicy;
use async_trait::async_trait;
use lm_bridge::message::ContentPart;
use mcp_client::McpServerConfig;
use mcp_client::manager::{McpConnectionManager, McpStatus};
use mcp_client::tool::McpTool;

fn fixture_config() -> McpServerConfig {
    McpServerConfig {
        name: "shapes".to_string(),
        command: Some("python3".to_string()),
        args: vec![
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/echo_server.py")
                .to_string_lossy()
                .to_string(),
        ],
        env: Default::default(),
        url: None,
        token: None,
        startup_timeout_ms: Some(5_000),
        tool_timeout_ms: Some(5_000),
        disabled: false,
        oauth: None,
        deferred: None,
        read_only: false,
        cwd: None,
    }
}

/// A model with the baseline vision policy.
struct VisionModel;

#[async_trait]
impl Llm for VisionModel {
    fn model_name(&self) -> &str {
        "vision"
    }

    fn supports_vision(&self) -> bool {
        true
    }

    async fn chat(
        &self,
        _messages: Vec<lm_bridge::message::Message>,
        _tools: Vec<lm_bridge::tool::Tool>,
        _callbacks: agent_core::agent_loop::LlmCallbacks,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<agent_core::agent_loop::types::StepResult, lm_bridge::errors::LlmError> {
        unreachable!("the adapter never calls the model")
    }
}

struct Harness {
    manager: McpConnectionManager,
    ctx: ExecutableToolContext,
    _spill: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        let (manager, _events) =
            McpConnectionManager::new(vec![fixture_config()], Default::default());
        manager.start_all().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let statuses = manager.statuses().await;
            match statuses.iter().find(|(n, _)| n == "shapes").map(|(_, s)| s) {
                Some(McpStatus::Connected) => break,
                Some(status @ (McpStatus::Failed(_) | McpStatus::Disabled)) => {
                    panic!("fixture server failed to start: {status:?}")
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    panic!("fixture server did not connect in time")
                }
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        let spill = tempfile::tempdir().unwrap();
        let ctx = ExecutableToolContext {
            llm: Some(Arc::new(VisionModel)),
            tool_output: ToolOutputPolicy::new(50_000, spill.path()),
            ..Default::default()
        };
        Self {
            manager,
            ctx,
            _spill: spill,
        }
    }

    async fn call(&self, tool: &str) -> ExecutableToolResult {
        let server = self.manager.get("shapes").expect("server handle");
        let definition = lm_bridge::tool::Tool::new(
            format!("mcp__shapes__{tool}"),
            "",
            serde_json::json!({"type": "object"}),
        );
        McpTool::new(server, tool, definition, None)
            .execute(serde_json::json!({}), &self.ctx, &format!("call-{tool}"))
            .await
    }

    fn spill_dir(&self) -> PathBuf {
        self.ctx.tool_output.spill_dir.clone()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.manager.stop_all();
    }
}

/// Structured content on its own becomes the result's text, pretty-printed;
/// a copy of it in a text block is not repeated; distinct data is kept.
#[tokio::test]
async fn structured_content_is_delivered_once() {
    let harness = Harness::start().await;

    let only = harness.call("structured_only").await;
    assert!(!only.is_error);
    assert_eq!(only.text_content(), "{\n  \"answer\": 42\n}");

    let copy = harness.call("structured_copy").await;
    assert_eq!(copy.text_content(), "{\"answer\": 42}");

    let extra = harness.call("structured_extra").await.text_content();
    assert!(
        extra.starts_with("Found 1 result\nStructured content:\n"),
        "{extra}"
    );
    assert!(extra.contains("\"id\": 7"), "{extra}");
}

/// An MCP screenshot reaches a vision model as an image, captioned with the
/// saved copy's path, after the tool's own text.
#[tokio::test]
async fn images_reach_a_vision_model() {
    let harness = Harness::start().await;
    let result = harness.call("screenshot").await;
    assert!(!result.is_error);
    assert_eq!(result.output.len(), 3, "{:?}", result.output);
    assert_eq!(result.output[0], ContentPart::text("Page captured"));
    let ContentPart::Text { text: caption } = &result.output[1] else {
        panic!("expected a caption, got {:?}", result.output[1]);
    };
    let saved = harness.spill_dir().join("call-screenshot-1.png");
    assert!(caption.contains(&saved.display().to_string()), "{caption}");
    assert!(saved.exists(), "the original is kept");
    assert!(matches!(
        &result.output[2],
        ContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:image/png;base64,")
    ));
}

/// Links keep their URI; audio and binary resources are saved and named.
#[tokio::test]
async fn links_audio_and_blobs_are_named() {
    let harness = Harness::start().await;

    let link = harness.call("link").await.text_content();
    assert_eq!(
        link,
        "[resource: report.pdf file:///tmp/report.pdf (application/pdf)]"
    );

    let sound = harness.call("sound").await.text_content();
    let saved = harness.spill_dir().join("call-sound-0.wav");
    assert!(
        sound.contains(&format!("saved to {}", saved.display())),
        "{sound}"
    );
    assert_eq!(std::fs::read(&saved).unwrap(), b"RIFF");

    let blob = harness.call("blob").await.text_content();
    assert!(
        blob.starts_with(
            "[binary resource file:///data.bin (application/octet-stream, 3 bytes) saved to"
        ),
        "{blob}"
    );
}

/// `isError` survives the richer shape.
#[tokio::test]
async fn tool_failures_stay_errors() {
    let harness = Harness::start().await;
    let failed = harness.call("fail").await;
    assert!(failed.is_error);
    assert!(failed.text_content().contains("file not found"));
}
