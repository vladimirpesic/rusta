use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use rmcp::model::ToolAnnotations;

/// OAuth2 configuration for an MCP server.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct McpOAuthConfig {
    /// OAuth client identifier.
    pub client_id: String,
    /// Optional client secret. Omit for public PKCE clients.
    pub client_secret: Option<String>,
    /// Authorization endpoint URL. Leave empty to discover it from the
    /// server (RFC 9728 protected-resource metadata, then RFC 8414).
    pub auth_url: String,
    /// Token endpoint URL. Leave empty to discover it, as above.
    pub token_url: String,
    /// Space-separated OAuth scopes, split automatically.
    #[serde(default)]
    pub scopes: String,
    /// Local port for the one-shot redirect listener.
    #[serde(default = "default_redirect_port")]
    pub redirect_port: u16,
}

fn default_redirect_port() -> u16 {
    8765
}

/// Configuration for a single MCP server.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct McpServerConfig {
    /// Human-readable name for this server. Must be unique within a session.
    pub name: String,

    /// Command to spawn for stdio MCP servers.
    pub command: Option<String>,

    /// Arguments for the stdio command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Working directory for a spawned stdio server.
    ///
    /// `None` inherits Rusta's own, which is the shell's — *not* the session's
    /// `--cwd`. That difference is not academic: a code-intelligence server
    /// resolves relative paths, walks up to find a project root, and spawns
    /// language servers that do the same, so a server left in the wrong
    /// directory answers confidently about the wrong tree. Point it at the
    /// workspace.
    #[serde(default)]
    pub cwd: Option<PathBuf>,

    /// Extra environment variables for stdio (command) servers. Applied on top
    /// of the inherited environment. Used to hand a spawned server its
    /// credentials without exporting them into every child process — e.g. the
    /// Z.AI vision server reads `Z_AI_API_KEY` from its own environment.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// URL for streamable HTTP/SSE MCP servers.
    pub url: Option<String>,

    /// Optional bearer token for HTTP MCP servers.
    #[serde(default)]
    pub token: Option<String>,

    /// Maximum time to wait for the server to initialize.
    ///
    /// `None` means "not set here" and defers to the global default, which is
    /// the distinction that makes precedence work — a `u64` defaulted at parse
    /// time is indistinguishable from an explicit 30000, so a global setting
    /// could never tell whether it was allowed to apply.
    #[serde(default)]
    pub startup_timeout_ms: Option<u64>,

    /// Maximum time to wait for a single tool call. `None` defers to the global
    /// default, as above.
    #[serde(default)]
    pub tool_timeout_ms: Option<u64>,

    /// If true, the manager will skip this server entirely.
    #[serde(default)]
    pub disabled: bool,

    /// OAuth2 configuration. When present, authentication errors trigger a
    /// browser-based PKCE authorization flow instead of the synthetic
    /// `authenticate` tool.
    #[serde(default)]
    pub oauth: Option<McpOAuthConfig>,

    /// Whether this server's tools are loadable on demand (`select_tools`)
    /// rather than always in the tool table. Unset: the global
    /// [`McpDefaults::default_deferred`] applies, and when that is unset too,
    /// deferred only when the server has more than [`DEFER_TOOL_THRESHOLD`]
    /// tools: a small server's tools stay inline, while a large server such
    /// as Devscriptor would crowd the table.
    #[serde(default)]
    pub deferred: Option<bool>,

    /// Whether this server's tools only read — they change nothing, here or
    /// elsewhere — though the server does not declare it (MCP's
    /// `readOnlyHint`). They are then taken as declared read-only: offered to
    /// the read-only sub-agents and run without approval. A tool that
    /// declares otherwise is taken at its word.
    #[serde(default)]
    pub read_only: bool,
}

/// Tool count above which a server's tools are deferred by default.
pub const DEFER_TOOL_THRESHOLD: usize = 10;

impl McpServerConfig {
    /// Startup timeout as a `Duration`, falling back to the built-in default.
    pub fn startup_timeout(&self) -> Duration {
        Duration::from_millis(
            self.startup_timeout_ms
                .unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS),
        )
    }

    /// Per-tool-call timeout as a `Duration`, falling back to the built-in default.
    pub fn tool_timeout(&self) -> Duration {
        Duration::from_millis(self.tool_timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT_MS))
    }

    /// Fill in whichever timeouts this server did not set for itself.
    ///
    /// Per-server values always win: a server that names its own budget knows
    /// something the global setting does not.
    pub fn apply_timeout_defaults(&mut self, defaults: &McpDefaults) {
        if self.startup_timeout_ms.is_none() {
            self.startup_timeout_ms = defaults.startup_timeout_ms;
        }
        if self.tool_timeout_ms.is_none() {
            self.tool_timeout_ms = defaults.tool_timeout_ms;
        }
    }

    /// Whether a server exposing `tool_count` tools has them deferred, under
    /// the global `default_deferred` from `[mcp]`.
    ///
    /// Per-server `deferred` always wins; then the global default; then the
    /// size rule — a server that says nothing is deferred only above
    /// [`DEFER_TOOL_THRESHOLD`]. A global `Some(false)` therefore keeps even a
    /// large server inline, which is the point of the knob.
    ///
    /// The caller passes the global value in (rather than the server config
    /// carrying a resolved copy) so the decision is made at connect time, when
    /// the tool count is known — and re-made on every reconnect, so a changed
    /// default is picked up the next time the server connects.
    #[must_use]
    pub fn defers_tools(&self, tool_count: usize, default_deferred: Option<bool>) -> bool {
        self.deferred
            .unwrap_or_else(|| default_deferred.unwrap_or(tool_count > DEFER_TOOL_THRESHOLD))
    }

    /// A tool's annotations as this server's config vouches for them:
    /// declared read-only where the server is [`read_only`](Self::read_only)
    /// and the tool says nothing either way.
    #[must_use]
    pub fn effective_annotations(
        &self,
        annotations: Option<ToolAnnotations>,
    ) -> Option<ToolAnnotations> {
        if !self.read_only {
            return annotations;
        }
        let mut annotations = annotations.unwrap_or_default();
        annotations.read_only_hint.get_or_insert(true);
        Some(annotations)
    }

    /// OAuth scopes as a list.
    pub fn oauth_scopes(&self) -> Vec<String> {
        self.oauth
            .as_ref()
            .map(|o| {
                o.scopes
                    .split_whitespace()
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Built-in startup budget, used when neither the server nor a global default
/// names one.
pub const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 30_000;
/// Built-in per-tool-call budget, used on the same terms.
pub const DEFAULT_TOOL_TIMEOUT_MS: u64 = 60_000;

/// Global MCP defaults, applied to every server that did not set its own.
///
/// Every field `None` means "change nothing", so an unconfigured Rusta keeps
/// the built-in budgets and the size-based deferral rule exactly as before.
///
/// `default_deferred` is not filled into the server configs the way the
/// timeouts are: the deferral decision needs the tool count, which only exists
/// at connect time, so the connection manager carries this value and passes it
/// to [`McpServerConfig::defers_tools`] on every (re)connect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct McpDefaults {
    pub startup_timeout_ms: Option<u64>,
    pub tool_timeout_ms: Option<u64>,
    /// Global default for loading MCP tools on demand. `Some(true)` defers
    /// every server's tools unless the server opts out with its own
    /// `deferred = false`; `Some(false)` keeps every server inline, above the
    /// [`DEFER_TOOL_THRESHOLD`] size rule included.
    pub default_deferred: Option<bool>,
}

impl McpDefaults {
    /// The startup budget for a server whose own sensible value is `fallback` —
    /// an auto-detected server that Rusta configures itself.
    ///
    /// A configured global wins here, unlike for a user's own `[[mcp_servers]]`
    /// entry: `fallback` is Rusta's opinion, not the user's, and a global
    /// setting is the only way to move it.
    #[must_use]
    pub fn startup_or(&self, fallback: u64) -> u64 {
        self.startup_timeout_ms.unwrap_or(fallback)
    }

    /// The per-tool-call budget, on the same terms.
    #[must_use]
    pub fn tool_or(&self, fallback: u64) -> u64 {
        self.tool_timeout_ms.unwrap_or(fallback)
    }
}
