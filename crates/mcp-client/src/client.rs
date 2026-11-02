use rmcp::{
    ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientConfig, ContentBlock, Implementation,
        ProtocolVersion, ResourceContents, ToolAnnotations,
    },
    service::{RoleClient, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::process::Command;

use crate::config::McpServerConfig;
use crate::lenient_http::LenientHttpClient;

/// One content block of an MCP tool result, as the server sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpBlock {
    /// Text, including the text of an embedded text resource.
    Text(String),
    /// Base64 media — an image or audio block, or an embedded blob resource
    /// whose type is image, audio or video. `uri` is the resource's, if any.
    Media {
        data: String,
        mime: String,
        uri: Option<String>,
    },
    /// An embedded binary resource of any other type.
    Blob {
        data: String,
        mime: Option<String>,
        uri: String,
    },
    /// A link to a resource the server did not inline.
    Link {
        uri: String,
        name: String,
        mime: Option<String>,
    },
}

/// The outcome of one MCP tool call, before it is shaped for the model.
///
/// `is_error` is independent of the content: a call can succeed at the
/// protocol level and still have failed as a tool. Collapsing the two hands
/// the model an error message dressed as a successful result, with no signal
/// to retry or change approach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolOutput {
    /// The result's content blocks, in order.
    pub blocks: Vec<McpBlock>,
    /// The result's `structuredContent`, if the server sent one.
    pub structured: Option<serde_json::Value>,
    /// The server's `isError` flag; `false` when the server omitted it.
    pub is_error: bool,
}

impl McpToolOutput {
    /// The text blocks, joined by newlines — for callers that want only text.
    #[must_use]
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                McpBlock::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Convert an rmcp result.
    #[must_use]
    pub fn from_result(result: rmcp::model::CallToolResult) -> Self {
        let blocks = result
            .content
            .into_iter()
            .map(|content| match content {
                ContentBlock::Text(t) => McpBlock::Text(t.text),
                ContentBlock::Image(image) => McpBlock::Media {
                    data: image.data,
                    mime: image.mime_type,
                    uri: None,
                },
                ContentBlock::Audio(audio) => McpBlock::Media {
                    data: audio.data,
                    mime: audio.mime_type,
                    uri: None,
                },
                ContentBlock::Resource(resource) => match resource.resource {
                    ResourceContents::TextResourceContents { text, .. } => McpBlock::Text(text),
                    ResourceContents::BlobResourceContents {
                        uri,
                        mime_type,
                        blob,
                        ..
                    } => match mime_type {
                        Some(mime)
                            if ["image/", "audio/", "video/"]
                                .iter()
                                .any(|kind| mime.starts_with(kind)) =>
                        {
                            McpBlock::Media {
                                data: blob,
                                mime,
                                uri: Some(uri),
                            }
                        }
                        mime => McpBlock::Blob {
                            data: blob,
                            mime,
                            uri,
                        },
                    },
                    other => unknown_block(&other),
                },
                ContentBlock::ResourceLink(link) => McpBlock::Link {
                    uri: link.uri,
                    name: link.name,
                    mime: link.mime_type,
                },
                other => unknown_block(&other),
            })
            .collect();
        Self {
            blocks,
            structured: result.structured_content,
            // A server reports a *tool* failure by setting `isError` on an
            // otherwise successful response, not by returning a protocol error
            // — so this flag is the only thing separating "the tool ran and
            // answered" from "the tool failed and this text is why". Omitting
            // it is spec-defined as false.
            is_error: result.is_error.unwrap_or(false),
        }
    }
}

/// A block kind this client does not know yet, kept as its JSON so the model
/// still sees what the server sent rather than nothing.
fn unknown_block(block: &impl serde::Serialize) -> McpBlock {
    McpBlock::Text(serde_json::to_string(block).unwrap_or_default())
}

/// An active connection to an MCP server.
pub struct McpClient {
    service: RunningService<RoleClient, ClientConfig>,
    /// What a stdio server wrote to stderr lately; `None` for HTTP.
    stderr: Option<StderrTail>,
}

/// How much of a stdio server's stderr is kept to quote in an error.
const STDERR_TAIL_BYTES: usize = 4 * 1024;

/// How long a failed start waits for the server's last words on stderr. The
/// transport is gone by then and the child killed, so its stderr is closing.
const STDERR_SETTLE: std::time::Duration = std::time::Duration::from_secs(1);

/// The recent stderr output of a stdio server, at most [`STDERR_TAIL_BYTES`].
///
/// A server's stderr is its log. Inherited, it lands on Rusta's terminal —
/// over the REPL, and into a headless run's output as coloured warnings —
/// whenever a call fails on the server side, though the model already has the
/// failure as the call's result. So it is captured instead: each line goes to
/// the `mcp_server` tracing target at debug level (`RUST_LOG=mcp_server=debug`
/// shows them), and the tail is quoted when the server fails to start or its
/// connection drops, when it is the explanation.
#[derive(Debug, Clone, Default)]
struct StderrTail(std::sync::Arc<std::sync::Mutex<String>>);

impl StderrTail {
    fn push_line(&self, line: &str) {
        let mut tail = self.0.lock().unwrap();
        tail.push_str(line);
        tail.push('\n');
        if tail.len() > STDERR_TAIL_BYTES {
            let mut cut = tail.len() - STDERR_TAIL_BYTES;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail.drain(..cut);
        }
    }

    /// What was kept, trimmed; `None` when the server wrote nothing.
    fn snapshot(&self) -> Option<String> {
        let tail = self.0.lock().unwrap();
        let trimmed = tail.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

/// `line` without ANSI escape sequences: servers colour their logs for a
/// terminal they no longer write to.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI: ESC '[' parameters, ended by a byte in '@'..='~'.
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Read `stderr` to its end, logging each line under `server` and keeping the
/// tail. Invalid UTF-8 is replaced rather than ending the read: a stderr that
/// is not drained fills its pipe and stalls the server.
fn drain_stderr(
    stderr: tokio::process::ChildStderr,
    server: String,
    tail: StderrTail,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = strip_ansi(String::from_utf8_lossy(&buf).trim_end());
                    tracing::debug!(target: "mcp_server", server = %server, "{line}");
                    tail.push_line(&line);
                }
            }
        }
    })
}

/// How Rusta introduces itself in the MCP `initialize` handshake.
///
/// Serving with `()` takes rmcp's default `Implementation::from_build_env()`,
/// whose `env!` macros expand inside the *rmcp* crate — so every server we
/// connected to was told it was talking to `rmcp 0.11.0`. Devscriptor logs the
/// peer's name, which made Rusta indistinguishable from any other rmcp client.
fn client_info() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("rusta", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::V_2024_11_05)
}

impl McpClient {
    /// Connect to an MCP server described by `config`.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        if let Some(url) = &config.url {
            Self::connect_http(url, config.token.as_deref()).await
        } else if let Some(cmd) = &config.command {
            Self::connect_command(cmd, &config.args, &config.env, config.cwd.as_deref()).await
        } else {
            Err(McpError::Config(
                "MCP server config must specify either `command` or `url`".to_string(),
            ))
        }
    }

    /// Connect via stdio to a spawned child process.
    pub async fn connect_command(
        cmd: &str,
        args: &[String],
        env: &std::collections::HashMap<String, String>,
        cwd: Option<&std::path::Path>,
    ) -> Result<Self, McpError> {
        let command = Self::build_command(cmd, args, env, cwd);
        let (transport, stderr) = TokioChildProcess::builder(command)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(McpError::Transport)?;
        let tail = StderrTail::default();
        let drain = stderr.map(|stderr| drain_stderr(stderr, cmd.to_string(), tail.clone()));
        match client_info().serve(transport).await {
            Ok(service) => Ok(Self {
                service,
                stderr: Some(tail),
            }),
            Err(error) => {
                if let Some(drain) = drain {
                    let _ = tokio::time::timeout(STDERR_SETTLE, drain).await;
                }
                Err(McpError::Init {
                    source: Box::new(error),
                    stderr: tail.snapshot(),
                })
            }
        }
    }

    /// Connect via streamable HTTP/SSE.
    pub async fn connect_http(url: &str, token: Option<&str>) -> Result<Self, McpError> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url);
        if let Some(t) = token {
            config = config.auth_header(t);
        }
        let http_client = lm_bridge::http::client_builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| McpError::Transport(std::io::Error::other(e)))?;
        // Wrapped so that servers which ack notifications with a bodyless `200`
        // instead of `202` do not kill the transport. See `lenient_http`.
        let http_client = LenientHttpClient::new(http_client);
        let transport = StreamableHttpClientTransport::with_client(http_client, config);
        let service = client_info()
            .serve(transport)
            .await
            .map_err(|e| McpError::Init {
                source: Box::new(e),
                stderr: None,
            })?;
        Ok(Self {
            service,
            stderr: None,
        })
    }

    /// The end of what a stdio server has written to stderr, trimmed; `None`
    /// for an HTTP server, or one that wrote nothing.
    pub fn stderr_tail(&self) -> Option<String> {
        self.stderr.as_ref().and_then(StderrTail::snapshot)
    }

    /// Returns true if the underlying transport has closed.
    pub fn is_transport_closed(&self) -> bool {
        self.service.is_transport_closed()
    }

    /// List all tools exposed by the server, including any annotations.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .map_err(|e| McpError::Service(Box::new(e)))?;
        Ok(tools.into_iter().map(convert_tool).collect())
    }

    /// Call a tool by name with the given JSON arguments.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<McpToolOutput, McpError> {
        let args = arguments.and_then(|v| v.as_object().cloned());
        let result = self
            .service
            .call_tool(match args {
                Some(args) => CallToolRequestParams::new(name.to_string()).with_arguments(args),
                None => CallToolRequestParams::new(name.to_string()),
            })
            .await
            .map_err(|e| McpError::Service(Box::new(e)))?;
        Ok(McpToolOutput::from_result(result))
    }

    fn build_command(
        cmd: &str,
        args: &[String],
        env: &std::collections::HashMap<String, String>,
        cwd: Option<&std::path::Path>,
    ) -> Command {
        let mut command = Self::spawn_target(cmd, args);
        command.envs(env);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        command
    }

    /// The program and arguments to actually spawn.
    ///
    /// On Unix this is the configured command unchanged. On Windows a bare name
    /// like `npx` or `uvx` is not executable on its own: those ship as `.cmd`
    /// shims, `CreateProcess` only ever appends `.exe`, and most MCP servers in
    /// the wild are published as exactly `npx -y <package>`. The name is
    /// therefore resolved through `PATH` × `PATHEXT` first, and a resolved
    /// batch script is run through the command interpreter, which is the only
    /// way to start one.
    fn spawn_target(cmd: &str, args: &[String]) -> Command {
        #[cfg(windows)]
        {
            if let Some(resolved) = resolve_windows_program(
                cmd,
                std::env::var("PATH").ok().as_deref(),
                std::env::var("PATHEXT").ok().as_deref(),
                &|p: &std::path::Path| p.is_file(),
            ) {
                let is_batch = resolved
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| {
                        e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat")
                    });
                if is_batch {
                    let mut command = Command::new("cmd");
                    command.arg("/C").arg(&resolved).args(args);
                    return command;
                }
                let mut command = Command::new(&resolved);
                command.args(args);
                return command;
            }
        }
        let mut command = Command::new(cmd);
        command.args(args);
        command
    }
}

/// Resolve a bare program name against `PATH` and `PATHEXT`, Windows-style.
///
/// Returns `None` when the name already carries an extension or a path
/// separator (nothing to resolve), or when nothing matches — in both cases the
/// caller should spawn the name as given and let the OS report the failure.
///
/// Written as a pure function over its inputs, and compiled on every platform,
/// so the resolution order is testable without a Windows host.
// Only called under `cfg(windows)`, but compiled and tested everywhere so the
// resolution order does not depend on a Windows host to verify.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn resolve_windows_program(
    cmd: &str,
    path_var: Option<&str>,
    pathext: Option<&str>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> Option<std::path::PathBuf> {
    if cmd.contains(['/', '\\']) || std::path::Path::new(cmd).extension().is_some() {
        return None;
    }
    // The documented default, for a machine where PATHEXT is unset.
    let pathext = pathext.unwrap_or(".COM;.EXE;.BAT;.CMD");
    for dir in path_var.unwrap_or("").split(';').filter(|d| !d.is_empty()) {
        for ext in pathext.split(';').filter(|e| !e.is_empty()) {
            let candidate = std::path::Path::new(dir).join(format!("{cmd}{ext}"));
            if exists(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Tool information returned by [`McpClient::list_tools`].
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub tool: lm_bridge::tool::Tool,
    pub annotations: Option<ToolAnnotations>,
}

fn convert_tool(tool: rmcp::model::Tool) -> McpToolInfo {
    McpToolInfo {
        tool: lm_bridge::tool::Tool {
            name: tool.name.into_owned(),
            description: tool.description.map(|d| d.into_owned()).unwrap_or_default(),
            parameters: serde_json::Value::Object(tool.input_schema.as_ref().clone()),
        },
        annotations: tool.annotations,
    }
}

/// Returns true if the error looks like an authentication failure.
pub fn is_auth_error(error: &ServiceError) -> bool {
    let text = format!("{error}");
    text.contains("401")
        || text.contains("403")
        || text.to_lowercase().contains("unauthorized")
        || text.to_lowercase().contains("forbidden")
}

/// Returns true if the error is a JSON-RPC method-not-found or invalid-params error.
pub fn is_method_missing_error(error: &ServiceError) -> bool {
    match error {
        ServiceError::McpError(err) => matches!(
            err.code.0,
            -32603..=-32601 // MethodNotFound | InvalidParams | InternalError
        ),
        _ => false,
    }
}

/// Errors that can occur when using an MCP client.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP configuration error: {0}")]
    Config(String),

    #[error("MCP transport error: {0}")]
    Transport(#[source] std::io::Error),

    #[error("MCP initialization failed: {source}{}", stderr_suffix(.stderr.as_deref()))]
    Init {
        #[source]
        source: Box<rmcp::service::ClientInitializeError>,
        /// The end of the server's stderr, for a stdio server that wrote any.
        stderr: Option<String>,
    },

    #[error("MCP service error: {0}")]
    Service(#[source] Box<rmcp::service::ServiceError>),

    /// The server rejected its credentials during a call; the message says
    /// how re-authorization proceeds.
    #[error("{0}")]
    Unauthorized(String),
}

/// A server's stderr tail, set off from the error it explains.
fn stderr_suffix(stderr: Option<&str>) -> String {
    stderr.map_or_else(String::new, |tail| format!("\nstderr: {tail}"))
}

impl McpError {
    /// Whether the error may resolve itself on reconnect.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Service(_))
    }

    /// Whether the error is fatal and the manager should stop retrying.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Config(_))
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn test_config_is_fatal() {
        let err = McpError::Config("missing command".to_string());
        assert!(err.is_fatal());
        assert!(!err.is_transient());
    }

    #[test]
    fn test_transport_is_transient_not_fatal() {
        let err = McpError::Transport(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(!err.is_fatal());
        assert!(err.is_transient());
    }
}

#[cfg(test)]
mod stderr_tests {
    use super::*;

    #[test]
    fn colour_codes_are_stripped_from_a_log_line() {
        assert_eq!(
            strip_ansi("\u{1b}[2m2026-09-29\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m rmcp::service"),
            "2026-09-29  WARN rmcp::service"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    /// The tail keeps the latest output, whole characters only.
    #[test]
    fn the_tail_keeps_the_end_and_stays_bounded() {
        let tail = StderrTail::default();
        assert_eq!(tail.snapshot(), None);
        for n in 0..2_000 {
            tail.push_line(&format!("line {n} é"));
        }
        let kept = tail.snapshot().unwrap();
        assert!(kept.len() <= STDERR_TAIL_BYTES, "{}", kept.len());
        assert!(kept.ends_with("line 1999 é"), "{kept}");
        assert!(!kept.contains("line 0 é"));
    }

    /// A server that dies during the handshake explains itself on stderr, and
    /// the error quotes it instead of printing it over the terminal.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_start_quotes_the_servers_stderr() {
        let error = McpClient::connect_command(
            "sh",
            &[
                "-c".to_string(),
                "echo 'fatal: no index at /nowhere/devscriptor.db' >&2; exit 3".to_string(),
            ],
            &std::collections::HashMap::new(),
            None,
        )
        .await
        .err()
        .expect("a server that exits cannot initialize");
        let text = error.to_string();
        assert!(text.starts_with("MCP initialization failed"), "{text}");
        assert!(
            text.contains("stderr: fatal: no index at /nowhere/devscriptor.db"),
            "{text}"
        );
    }
}

#[cfg(test)]
mod tests {
    /// Rusta must introduce itself, not the SDK.
    ///
    /// `().serve(transport)` inherits `Implementation::from_build_env()`, whose
    /// `env!` expands inside the rmcp crate — so every server saw `rmcp 0.11.0`.
    #[test]
    fn client_info_names_rusta_and_its_own_version() {
        let info = super::client_info();
        assert_eq!(info.client_info.name, "rusta");
        assert_eq!(info.client_info.version, env!("CARGO_PKG_VERSION"));
        assert_ne!(
            info.client_info.name, "rmcp",
            "must not inherit the SDK identity"
        );
    }

    /// The handshake must report *our* version, not the SDK's.
    ///
    /// Compared against the compile-time crate version rather than a literal:
    /// pinning the literal would fail this test on every release bump — telling
    /// you only that you had bumped the version, which you knew — while still
    /// missing the failure that matters, a hand-written string drifting from
    /// the package.
    #[test]
    fn client_version_is_the_crate_version() {
        let reported = super::client_info().client_info.version;
        assert_eq!(reported, env!("CARGO_PKG_VERSION"));
        assert_ne!(
            reported, "0.11.0",
            "reporting rmcp's version means we inherited its identity"
        );
    }

    use std::sync::Arc;

    use super::*;

    #[test]
    fn test_convert_tool() {
        let schema: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"type":"object","properties":{"q":{"type":"string"}}}"#)
                .unwrap();
        let rmcp_tool = rmcp::model::Tool::new("search", "search the web", Arc::new(schema));
        let info = convert_tool(rmcp_tool);
        assert_eq!(info.tool.name, "search");
        assert_eq!(info.tool.description, "search the web");
    }

    /// A stdio server inherits Rusta's own working directory unless told
    /// otherwise — the shell's, not the session's `--cwd`. A code-intelligence
    /// server left there resolves relative paths and hunts for a project root in
    /// the wrong tree, so `cwd` must actually reach the child.
    #[test]
    fn test_build_command_sets_the_working_directory() {
        let env = std::collections::HashMap::new();
        let cmd = McpClient::build_command(
            "devscriptor",
            &["mcp".to_string()],
            &env,
            Some(std::path::Path::new("/srv/app")),
        );
        assert_eq!(
            cmd.as_std().get_current_dir(),
            Some(std::path::Path::new("/srv/app"))
        );
    }

    #[test]
    fn test_build_command_without_a_cwd_inherits() {
        let env = std::collections::HashMap::new();
        let cmd = McpClient::build_command("x", &[], &env, None);
        assert_eq!(cmd.as_std().get_current_dir(), None);
    }

    /// `npx`, `uvx` and friends ship as `.cmd` shims on Windows, and
    /// `CreateProcess` only ever appends `.exe` — so a config saying
    /// `command = "npx"` fails with ENOENT unless the name is resolved through
    /// PATHEXT first. Most published MCP servers are exactly `npx -y <pkg>`.
    // `PathBuf::join` uses the host separator, so these cases use
    // separator-neutral directories. What is under test is the PATH × PATHEXT
    // search order, not Windows path syntax.
    #[test]
    fn test_a_bare_name_resolves_through_pathext() {
        let want = std::path::Path::new("/node").join("npx.CMD");
        let present = |p: &std::path::Path| p == want;
        assert_eq!(
            resolve_windows_program("npx", Some("/node"), Some(".EXE;.CMD"), &present),
            Some(want.clone())
        );
    }

    /// PATHEXT order decides, so a real executable wins over a shim.
    #[test]
    fn test_pathext_order_is_honoured() {
        let exe = std::path::Path::new("/bin").join("tool.EXE");
        let cmd = std::path::Path::new("/bin").join("tool.CMD");
        let present = |p: &std::path::Path| p == exe || p == cmd;
        assert_eq!(
            resolve_windows_program("tool", Some("/bin"), Some(".EXE;.CMD"), &present),
            Some(exe.clone())
        );
    }

    /// PATH order decides between directories.
    #[test]
    fn test_earlier_path_entries_win() {
        let first = std::path::Path::new("/a").join("x.CMD");
        let second = std::path::Path::new("/b").join("x.CMD");
        let present = |p: &std::path::Path| p == first || p == second;
        assert_eq!(
            resolve_windows_program("x", Some("/a;/b"), Some(".CMD"), &present),
            Some(first.clone())
        );
    }

    /// A name that is already qualified is left alone — resolving it could pick
    /// a different file than the user named.
    #[test]
    fn test_qualified_names_are_not_resolved() {
        let always = |_: &std::path::Path| true;
        for cmd in ["node.exe", r"C:\tools\srv", "./srv", "dir/srv"] {
            assert_eq!(
                resolve_windows_program(cmd, Some("/bin"), Some(".EXE"), &always),
                None,
                "{cmd} must be spawned as given"
            );
        }
    }

    #[test]
    fn test_an_unresolvable_name_falls_through() {
        let none = |_: &std::path::Path| false;
        assert_eq!(
            resolve_windows_program("nope", Some("/bin"), Some(".EXE"), &none),
            None
        );
    }

    /// A machine with no PATHEXT still gets the documented default set.
    #[test]
    fn test_missing_pathext_uses_the_documented_default() {
        let want = std::path::Path::new("/bin").join("npx.CMD");
        let present = |p: &std::path::Path| p == want;
        assert_eq!(
            resolve_windows_program("npx", Some("/bin"), None, &present),
            Some(want.clone())
        );
    }
}
