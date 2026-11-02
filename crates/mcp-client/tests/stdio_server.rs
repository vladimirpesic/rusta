use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use agent_core::agent_loop::types::ExecutableToolContext;
use mcp_client::McpServerConfig;
use mcp_client::manager::{McpConnectionManager, McpEvent, McpStatus};

/// How long a fixture server may take to start and list its tools. Python
/// starts slowly on a busy Windows runner, several tests' servers at once.
const STARTUP: Duration = Duration::from_secs(30);
const STARTUP_MS: u64 = 30_000;

fn echo_server_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo_server.py")
}

fn one_tool_server_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/one_tool_server.py")
}

fn echo_config() -> McpServerConfig {
    McpServerConfig {
        name: "echo".to_string(),
        command: Some("python3".to_string()),
        args: vec![echo_server_path().to_string_lossy().to_string()],
        env: Default::default(),
        url: None,
        token: None,
        startup_timeout_ms: Some(STARTUP_MS),
        tool_timeout_ms: Some(5_000),
        disabled: false,
        oauth: None,
        deferred: None,
        read_only: false,
        cwd: None,
    }
}

async fn wait_connected(manager: &McpConnectionManager, server: &str) {
    let deadline = tokio::time::Instant::now() + STARTUP;
    while tokio::time::Instant::now() < deadline {
        let statuses = manager.statuses().await;
        if let Some((_, status)) = statuses.iter().find(|(n, _)| n == server) {
            if matches!(status, McpStatus::Connected) {
                return;
            }
            if matches!(status, McpStatus::Failed(_) | McpStatus::Disabled) {
                panic!("MCP server failed to start: {status:?}");
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("MCP server did not connect in time");
}

#[tokio::test]
async fn test_stdio_mcp_server_discovery_and_call() {
    let cfg = McpServerConfig {
        name: "echo".to_string(),
        command: Some("python3".to_string()),
        args: vec![echo_server_path().to_string_lossy().to_string()],
        env: Default::default(),
        url: None,
        token: None,
        startup_timeout_ms: Some(STARTUP_MS),
        tool_timeout_ms: Some(5_000),
        disabled: false,
        oauth: None,
        deferred: None,
        read_only: false,
        cwd: None,
    };

    let (manager, _events) = McpConnectionManager::new(vec![cfg], Default::default());
    manager.start_all().await;

    let deadline = tokio::time::Instant::now() + STARTUP;
    let mut connected = false;
    while tokio::time::Instant::now() < deadline {
        let statuses = manager.statuses().await;
        if let Some((_, status)) = statuses.iter().find(|(n, _)| n == "echo") {
            if matches!(status, McpStatus::Connected) {
                connected = true;
                break;
            }
            if matches!(status, McpStatus::Failed(_) | McpStatus::Disabled) {
                panic!("MCP server failed to start: {status:?}");
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(connected, "MCP server did not connect in time");

    let server = manager.get("echo").expect("echo server handle missing");
    let result = server
        .call_tool("echo", Some(serde_json::json!({"message": "hello"})))
        .await
        .expect("tool call failed");
    assert!(!result.is_error, "echo must not report a tool failure");
    assert!(
        result.text().contains("hello"),
        "unexpected result: {}",
        result.text()
    );

    manager.stop_all();
}

/// A server that dies mid-session must not permanently break tool calls: after the
/// transport drops, a subsequent call heals the connection (via `call_tool`'s
/// reconnect-and-retry and/or the lifecycle loop) and succeeds, instead of failing
/// the turn forever.
#[tokio::test]
async fn test_stdio_mcp_server_survives_midsession_death() {
    let (manager, _events) = McpConnectionManager::new(vec![echo_config()], Default::default());
    manager.start_all().await;
    wait_connected(&manager, "echo").await;

    let server = manager.get("echo").expect("echo server handle missing");

    // Baseline: the server answers before it dies.
    let before = server
        .call_tool("echo", Some(serde_json::json!({"message": "before"})))
        .await
        .expect("baseline echo failed");
    assert!(
        before.text().contains("before"),
        "unexpected baseline: {}",
        before.text()
    );

    // Kill it mid-session: `die` replies, then the process exits and the transport
    // closes. The result is irrelevant (it may arrive, or race the close).
    let _ = server.call_tool("die", Some(serde_json::json!({}))).await;

    // The next echo must recover within a bounded window.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut recovered: Option<mcp_client::McpToolOutput> = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(out) = server
            .call_tool("echo", Some(serde_json::json!({"message": "after"})))
            .await
        {
            recovered = Some(out);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let after = recovered.expect("echo never recovered after a mid-session server death");
    assert!(
        after.text().contains("after"),
        "unexpected post-recovery: {}",
        after.text()
    );

    manager.stop_all();
}

/// An MCP server reports a *tool* failure by setting `isError` on an otherwise
/// successful JSON-RPC response. Dropping that flag hands the model an error
/// message dressed as a successful result: it has no signal to retry or change
/// approach, and may take the failure text as the answer.
#[tokio::test]
async fn test_tool_failure_is_reported_as_an_error_not_a_success() {
    let (manager, _events) = McpConnectionManager::new(vec![echo_config()], Default::default());
    manager.start_all().await;
    wait_connected(&manager, "echo").await;

    let server = manager.get("echo").expect("echo server handle missing");

    let failed = server
        .call_tool("fail", Some(serde_json::json!({})))
        .await
        .expect("the JSON-RPC call itself succeeds — the *tool* is what failed");
    assert!(
        failed.is_error,
        "`isError: true` must survive the transport"
    );
    assert!(
        failed.text().contains("file not found"),
        "the failure text must survive too: {}",
        failed.text()
    );

    // The success path must stay a success — the flag is read, not assumed.
    let ok = server
        .call_tool("echo", Some(serde_json::json!({"message": "hi"})))
        .await
        .expect("echo succeeds");
    assert!(
        !ok.is_error,
        "a successful call must not be marked an error"
    );

    manager.stop_all();
}

/// A stdio server must actually be spawned in its configured directory.
///
/// This is the whole reason `cwd` exists: a code-intelligence server resolves
/// relative paths, walks up to find a project root, and spawns language servers
/// that do the same. Inheriting Rusta's own directory — the shell's, not the
/// session's `--cwd` — makes it answer confidently about the wrong tree.
#[tokio::test]
async fn test_stdio_server_is_spawned_in_its_configured_directory() {
    let temp = tempfile::tempdir().expect("temp dir");
    let dir = temp.path().canonicalize().expect("canonical temp dir");

    let mut cfg = echo_config();
    cfg.cwd = Some(dir.clone());

    let (manager, _events) = McpConnectionManager::new(vec![cfg], Default::default());
    manager.start_all().await;
    wait_connected(&manager, "echo").await;

    let server = manager.get("echo").expect("echo server handle missing");
    let out = server
        .call_tool("cwd", Some(serde_json::json!({})))
        .await
        .expect("cwd tool call");
    assert_eq!(
        std::path::Path::new(out.text().trim())
            .canonicalize()
            .expect("reported cwd"),
        dir,
        "the server ran somewhere other than its configured directory"
    );

    manager.stop_all();
}

/// Whether a server's tools are deferred: its own `deferred` setting when it
/// has one, otherwise the global `[mcp] default_deferred`, otherwise its size
/// — the echo fixture's eleven tools are over the threshold of ten, the
/// one-tool fixture's are not.
#[tokio::test]
async fn tools_are_deferred_by_size_unless_configured() {
    use mcp_client::manager::McpEvent;

    async fn deferred_flag(
        config: McpServerConfig,
        defaults: mcp_client::McpDefaults,
        tool_count: usize,
    ) -> bool {
        let (manager, mut events) = McpConnectionManager::new(vec![config], defaults);
        manager.start_all().await;
        let deadline = tokio::time::Instant::now() + STARTUP;
        let flag = loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .expect("tools were never announced")
                .expect("event channel open");
            if let McpEvent::ToolsAdded {
                tools, deferred, ..
            } = event
            {
                assert_eq!(tools.len(), tool_count);
                break deferred;
            }
        };
        manager.stop_all();
        flag
    }

    assert!(
        deferred_flag(echo_config(), Default::default(), 11).await,
        "eleven tools are deferred"
    );
    let mut inline = echo_config();
    inline.deferred = Some(false);
    assert!(
        !deferred_flag(inline, Default::default(), 11).await,
        "an explicit setting wins"
    );

    let global_on = mcp_client::McpDefaults {
        default_deferred: Some(true),
        ..Default::default()
    };
    let one_tool = McpServerConfig {
        name: "one-tool".to_string(),
        args: vec![one_tool_server_path().to_string_lossy().to_string()],
        ..echo_config()
    };
    assert!(
        deferred_flag(one_tool.clone(), global_on, 1).await,
        "a global default_deferred = true defers even a one-tool server"
    );
    let mut opts_out = one_tool;
    opts_out.deferred = Some(false);
    assert!(
        !deferred_flag(opts_out, global_on, 1).await,
        "a server opts out with its own deferred = false"
    );
    let global_off = mcp_client::McpDefaults {
        default_deferred: Some(false),
        ..Default::default()
    };
    assert!(
        !deferred_flag(echo_config(), global_off, 11).await,
        "a global default_deferred = false keeps even a large server inline"
    );
}

/// A server running `script` from the fixtures, named `name`.
fn fixture_config(name: &str, script: &str) -> McpServerConfig {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(script);
    McpServerConfig {
        name: name.to_string(),
        command: Some("python3".to_string()),
        args: vec![script.to_string_lossy().to_string()],
        startup_timeout_ms: Some(STARTUP_MS),
        tool_timeout_ms: Some(5_000),
        ..Default::default()
    }
}

/// The next event that `pick` accepts, within [`STARTUP`].
async fn next_event<T>(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<McpEvent>,
    pick: impl Fn(McpEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(STARTUP, async {
        loop {
            let event = events.recv().await.expect("the manager is alive");
            if let Some(found) = pick(event) {
                return found;
            }
        }
    })
    .await
    .expect("the event arrived")
}

/// A tool reads only when its server declares it so (`readOnlyHint`); one
/// the server says nothing about is not taken to read only.
#[tokio::test]
async fn a_tool_reads_only_when_its_server_declares_it() {
    let (manager, mut events) = McpConnectionManager::new(vec![echo_config()], Default::default());
    manager.start_all().await;
    wait_connected(&manager, "echo").await;
    let tools = next_event(&mut events, |event| match event {
        McpEvent::ToolsAdded { tools, .. } => Some(tools),
        _ => None,
    })
    .await;
    let read_only = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.tool_definition().name == format!("mcp__echo__{name}"))
            .expect("the fixture lists the tool")
            .is_read_only()
    };
    assert!(read_only("cwd"));
    assert!(!read_only("die"));
    assert!(!read_only("echo"));
    manager.stop_all();
}

/// A server configured `read_only` vouches for the tools that say nothing;
/// one that declares it changes something is taken at its word.
#[tokio::test]
async fn a_read_only_server_vouches_for_the_tools_that_say_nothing() {
    let config = McpServerConfig {
        read_only: true,
        ..echo_config()
    };
    let (manager, mut events) = McpConnectionManager::new(vec![config], Default::default());
    manager.start_all().await;
    wait_connected(&manager, "echo").await;
    let tools = next_event(&mut events, |event| match event {
        McpEvent::ToolsAdded { tools, .. } => Some(tools),
        _ => None,
    })
    .await;
    let tool = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.tool_definition().name == format!("mcp__echo__{name}"))
            .expect("the fixture lists the tool")
    };
    assert!(tool("cwd").is_read_only());
    assert!(tool("echo").is_read_only());
    assert_eq!(tool("echo").approval_rule(&serde_json::json!({})), "silent");
    assert!(!tool("die").is_read_only());
    manager.stop_all();
}

/// Tool names model APIs refuse — a dot, a space, names past 64
/// characters — and a server name with a dot are offered under valid,
/// unique names, and each still reaches its own tool.
#[tokio::test]
async fn odd_tool_names_are_offered_wire_safe_and_reach_their_tools() {
    let (manager, mut events) = McpConnectionManager::new(
        vec![fixture_config("odd.names", "odd_names_server.py")],
        Default::default(),
    );
    assert_eq!(
        manager.renamed_servers(),
        [("odd.names".to_string(), "odd_names".to_string())]
    );
    manager.start_all().await;
    wait_connected(&manager, "odd.names").await;
    let tools = next_event(&mut events, |event| match event {
        McpEvent::ToolsAdded { tools, .. } => Some(tools),
        _ => None,
    })
    .await;

    let names: Vec<String> = tools
        .iter()
        .map(|tool| tool.tool_definition().name.clone())
        .collect();
    assert_eq!(names.len(), 5, "{names:?}");
    assert_eq!(
        names.iter().collect::<HashSet<_>>().len(),
        5,
        "unique: {names:?}"
    );
    for name in &names {
        assert!(
            name.len() <= 64
                && name.starts_with("mcp__odd_names__")
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{name}"
        );
    }
    assert!(names.contains(&"mcp__odd_names__read_file".to_string()));
    assert!(names.contains(&"mcp__odd_names__plain_tool".to_string()));

    for tool in &tools {
        let original = tool
            .tool_definition()
            .description
            .strip_prefix("Tool ")
            .expect("the fixture describes each tool by its name")
            .to_string();
        let result = tool
            .execute(
                serde_json::json!({}),
                &ExecutableToolContext::default(),
                "tc",
            )
            .await;
        assert_eq!(
            result.text_content(),
            format!("called {original}"),
            "{}",
            tool.tool_definition().name
        );
    }
    manager.stop_all();
}

/// When a server stops listing a tool, the rediscovery that follows a
/// "method not found" reports it removed and the new tool added, and the
/// server's list holds each tool once.
#[tokio::test]
async fn rediscovery_reports_a_dropped_tool_removed() {
    let (manager, mut events) = McpConnectionManager::new(
        vec![fixture_config("changing", "changing_tools_server.py")],
        Default::default(),
    );
    manager.start_all().await;
    wait_connected(&manager, "changing").await;
    let first = next_event(&mut events, |event| match event {
        McpEvent::ToolsAdded { tools, .. } => Some(tools),
        _ => None,
    })
    .await;
    assert_eq!(first.len(), 2);

    let server = manager.get("changing").expect("the server handle");
    assert!(
        server
            .call_tool("gone", Some(serde_json::json!({})))
            .await
            .is_err(),
        "the dropped tool still fails after rediscovery"
    );

    let removed = next_event(&mut events, |event| match event {
        McpEvent::ToolsRemoved { tools, .. } => Some(tools),
        _ => None,
    })
    .await;
    assert_eq!(removed, ["mcp__changing__gone"]);
    let added = next_event(&mut events, |event| match event {
        McpEvent::ToolsAdded { tools, .. } => Some(tools),
        _ => None,
    })
    .await;
    let added: Vec<String> = added
        .iter()
        .map(|tool| tool.tool_definition().name.clone())
        .collect();
    assert_eq!(added, ["mcp__changing__fresh"]);
    let known: Vec<String> = server
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(known, ["mcp__changing__keep", "mcp__changing__fresh"]);
    manager.stop_all();
}
