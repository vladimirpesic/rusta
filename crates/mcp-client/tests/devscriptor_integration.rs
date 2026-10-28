//! Live Devscriptor MCP integration test.
//!
//! Spawns the real Devscriptor MCP server through Rusta's own connection
//! manager and asserts the full 75-tool contract: discovery, name qualification,
//! description propagation (needed for tool-disclosure announcements), and
//! schema shape (needed for Kimi/GLM tool serialization).
//!
//! `#[ignore]` so it does not run during normal `cargo test`. Run it with the
//! path to a *full-featured* `devscriptor` binary (the `devscriptor-cli` default
//! features include `lsp` + `mcp`; the `devscriptor-mcp-standalone` development
//! binary does NOT and serves only the 32 core tools):
//!
//! ```bash
//! DEVSCRIPTOR_BIN=/path/to/devscriptor \
//!   cargo test -p mcp-client --test devscriptor_integration -- --ignored --nocapture
//! ```

use std::path::Path;
use std::time::Duration;

use mcp_client::{
    McpServerConfig,
    manager::{McpConnectionManager, McpEvent, McpStatus},
};

const EXPECTED_TOOL_COUNT: usize = 75;

/// A representative tool from each of the three Devscriptor families.
const CORE_TOOL: &str = "scan_codebase";
const LSP_TOOL: &str = "get_hover";
const ARCH_TOOL: &str = "lsp_arch_setup";
const WATCHER_TOOL: &str = "start_watching";

/// The five Context Graph tools (a subset of the 32 core tools).
const CONTEXT_GRAPH_TOOLS: [&str; 5] = [
    "add_context_note",
    "search_context",
    "get_related_context",
    "prune_context",
    "get_context_statistics",
];

/// The server as Rusta injects it, over an empty workspace of the test's
/// own: without `--database` it opens the shared index in the user's data
/// directory, and run from this crate it starts language servers for
/// Rusta's own sources.
fn devscriptor_config(bin: &str, workspace: &Path) -> McpServerConfig {
    McpServerConfig {
        name: "devscriptor".to_string(),
        command: Some(bin.to_string()),
        args: vec![
            "mcp".to_string(),
            "--database".to_string(),
            workspace.join("devscriptor.db").display().to_string(),
        ],
        cwd: Some(workspace.to_path_buf()),
        startup_timeout_ms: Some(60_000),
        tool_timeout_ms: Some(180_000),
        ..Default::default()
    }
}

/// Connect and wait until the server leaves the `Pending` state.
async fn connect(
    bin: &str,
    workspace: &Path,
) -> (
    McpConnectionManager,
    std::sync::Arc<mcp_client::manager::ServerHandle>,
) {
    let (manager, _events) =
        McpConnectionManager::new(vec![devscriptor_config(bin, workspace)], Default::default());
    manager.start_all().await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let statuses = manager.statuses().await;
        match statuses.first().map(|(_, s)| s.clone()) {
            Some(McpStatus::Connected) => break,
            Some(McpStatus::Failed(e)) => panic!("devscriptor MCP server failed: {e}"),
            _ if tokio::time::Instant::now() >= deadline => {
                panic!("timed out waiting for devscriptor MCP server to connect")
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }

    let server = manager.get("devscriptor").expect("server handle");
    // Tool discovery happens right after connect; give it a moment to populate.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while server.tool_names().await.is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (manager, server)
}

#[tokio::test]
#[ignore = "requires a full-featured devscriptor binary via DEVSCRIPTOR_BIN"]
async fn devscriptor_exposes_all_75_tools_with_usable_definitions() {
    let Ok(bin) = std::env::var("DEVSCRIPTOR_BIN") else {
        eprintln!("DEVSCRIPTOR_BIN not set; skipping");
        return;
    };

    let workspace = tempfile::tempdir().unwrap();
    let (manager, server) = connect(&bin, workspace.path()).await;

    // 1. All 75 tools are discovered (list_all_tools follows any pagination).
    let names = server.tool_names().await;
    assert_eq!(
        names.len(),
        EXPECTED_TOOL_COUNT,
        "expected {EXPECTED_TOOL_COUNT} tools, got {}. A binary built without the \
         `lsp` feature serves only the 32 core tools. Discovered: {names:?}",
        names.len()
    );

    // 2. Every tool family is present.
    for tool in [CORE_TOOL, LSP_TOOL, ARCH_TOOL, WATCHER_TOOL] {
        assert!(names.contains(&tool.to_string()), "missing tool {tool}");
    }
    for tool in CONTEXT_GRAPH_TOOLS {
        assert!(
            names.contains(&tool.to_string()),
            "missing context-graph tool {tool}"
        );
    }

    let definitions = server.tool_definitions().await;
    assert_eq!(definitions.len(), EXPECTED_TOOL_COUNT);

    for def in &definitions {
        // 3. Names are qualified as mcp__<server>__<tool> so the agent can route them.
        assert!(
            def.name.starts_with("mcp__devscriptor__"),
            "tool name not qualified: {}",
            def.name
        );

        // 4. Descriptions are propagated — tool-disclosure announcements rely on
        //    them for unambiguous `select_tools` selection.
        assert!(
            !def.description.trim().is_empty(),
            "tool {} has no description",
            def.name
        );

        // 5. Schemas are well-formed objects with no local $ref, so both the
        //    Kimi normalizer and GLM's pass-through serialization accept them.
        assert_eq!(
            def.parameters.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "tool {} schema root is not an object",
            def.name
        );
        assert!(
            !def.parameters.to_string().contains("\"$ref\""),
            "tool {} schema contains a $ref",
            def.name
        );
    }

    // 6. `scan_codebase` really takes `path`/`extensions` (not the documented
    //    `file_extensions`), which is what the arg-remap table relies on.
    let scan_props = server.properties_for(CORE_TOOL).await;
    assert!(
        scan_props.contains(&"path".to_string()),
        "scan props: {scan_props:?}"
    );
    assert!(
        scan_props.contains(&"extensions".to_string()),
        "scan_codebase should expose `extensions`, got {scan_props:?}"
    );

    // 7. LSP position tools take file_path/line/character.
    let hover_props = server.properties_for(LSP_TOOL).await;
    for p in ["file_path", "line", "character"] {
        assert!(
            hover_props.contains(&p.to_string()),
            "get_hover missing {p}: {hover_props:?}"
        );
    }

    manager.stop_all();
}

#[tokio::test]
#[ignore = "requires a full-featured devscriptor binary via DEVSCRIPTOR_BIN"]
async fn devscriptor_read_only_tool_call_succeeds() {
    let Ok(bin) = std::env::var("DEVSCRIPTOR_BIN") else {
        eprintln!("DEVSCRIPTOR_BIN not set; skipping");
        return;
    };

    let workspace = tempfile::tempdir().unwrap();
    let (manager, server) = connect(&bin, workspace.path()).await;

    // `health_check` takes no arguments and touches no user files.
    let output = server
        .call_tool("health_check", Some(serde_json::json!({})))
        .await
        .expect("health_check should succeed");
    assert!(!output.is_error, "health_check reported a tool failure");
    assert!(
        !output.text().trim().is_empty(),
        "health_check returned empty output"
    );

    manager.stop_all();
}

/// Devscriptor declares what each tool does, and Rusta acts on it: the
/// queries run without asking and go to read-only sub-agents, and the tools
/// that change something ask.
#[tokio::test]
#[ignore = "requires a full-featured devscriptor binary via DEVSCRIPTOR_BIN"]
async fn devscriptor_declares_which_tools_only_read() {
    let Ok(bin) = std::env::var("DEVSCRIPTOR_BIN") else {
        eprintln!("DEVSCRIPTOR_BIN not set; skipping");
        return;
    };

    let workspace = tempfile::tempdir().unwrap();
    let (manager, mut events) = McpConnectionManager::new(
        vec![devscriptor_config(&bin, workspace.path())],
        Default::default(),
    );
    manager.start_all().await;
    let tools = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            match events.recv().await.expect("the manager is alive") {
                McpEvent::ToolsAdded { tools, .. } => return tools,
                McpEvent::Status {
                    status: McpStatus::Failed(e),
                    ..
                } => panic!("devscriptor MCP server failed: {e}"),
                _ => {}
            }
        }
    })
    .await
    .expect("Devscriptor's tools arrive");
    assert_eq!(tools.len(), EXPECTED_TOOL_COUNT);

    let tool = |name: &str| {
        tools
            .iter()
            .find(|t| t.tool_definition().name == format!("mcp__devscriptor__{name}"))
            .unwrap_or_else(|| panic!("no tool {name}"))
    };
    let no_input = serde_json::json!({});
    for name in [
        "semantic_search",
        "get_statistics",
        "get_references",
        "lsp_arch_detect",
        "search_context",
    ] {
        assert!(tool(name).is_read_only(), "{name}");
        assert_eq!(tool(name).approval_rule(&no_input), "silent", "{name}");
    }
    for name in [
        "rename_symbol",
        "format_document",
        "scan_codebase",
        "add_context_note",
        "lsp_arch_setup",
    ] {
        assert!(!tool(name).is_read_only(), "{name}");
        assert_ne!(tool(name).approval_rule(&no_input), "silent", "{name}");
    }
    assert_eq!(tools.iter().filter(|t| t.is_read_only()).count(), 59);

    manager.stop_all();
}
