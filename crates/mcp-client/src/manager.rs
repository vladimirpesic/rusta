use std::{
    path::PathBuf,
    sync::Arc,
    sync::Mutex as StdMutex,
    sync::Weak,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use agent_core::agent_loop::types::{ExecutableTool, ToolAccess, ToolAccesses};
use async_trait::async_trait;
use lm_bridge::tool::Tool;
use rmcp::model::ToolAnnotations;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use std::collections::HashMap;

use crate::{
    client::{McpClient, McpError, McpToolInfo, is_auth_error, is_method_missing_error},
    config::{McpDefaults, McpServerConfig},
    oauth::{self, CredentialStore, OAuthError, OAuthToken, PkcePair, random_state, redirect_uri},
    tool::McpTool,
};

/// Extract top-level property names from a JSON Schema object.
fn schema_properties(schema: &serde_json::Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

/// Lifecycle status of a single MCP server connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpStatus {
    /// The manager is about to attempt a connection.
    Pending,
    /// The server is connected and its tools are available.
    Connected,
    /// The server is in a failed state, with a human-readable reason.
    Failed(String),
    /// The server has been explicitly disabled in configuration.
    Disabled,
    /// The server requires authentication before it can be used.
    NeedsAuth,
}

/// Events emitted by the connection manager about server and tool lifecycle.
pub enum McpEvent {
    /// Server status changed.
    Status {
        server: String,
        status: McpStatus,
        message: Option<String>,
    },
    /// Tools became available for the first time or after a reconnect.
    /// `deferred` says whether they are loadable on demand (`select_tools`)
    /// rather than always in the tool table; see
    /// [`McpServerConfig::defers_tools`].
    ToolsAdded {
        server: String,
        tools: Vec<Arc<dyn ExecutableTool>>,
        deferred: bool,
    },
    /// Tools are no longer available because the server disconnected.
    ToolsRemoved { server: String, tools: Vec<String> },
    /// The server requires authentication.
    AuthRequired {
        server: String,
        tool: Arc<dyn ExecutableTool>,
    },
    /// The server is waiting for the user to complete OAuth in a browser.
    OAuthUrl { server: String, url: String },
}

impl std::fmt::Debug for McpEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status {
                server,
                status,
                message,
            } => f
                .debug_struct("Status")
                .field("server", server)
                .field("status", status)
                .field("message", message)
                .finish(),
            Self::ToolsAdded {
                server,
                tools,
                deferred,
            } => f
                .debug_struct("ToolsAdded")
                .field("server", server)
                .field(
                    "tools",
                    &tools
                        .iter()
                        .map(|t| t.tool_definition().name.clone())
                        .collect::<Vec<_>>(),
                )
                .field("deferred", deferred)
                .finish(),
            Self::ToolsRemoved { server, tools } => f
                .debug_struct("ToolsRemoved")
                .field("server", server)
                .field("tools", tools)
                .finish(),
            Self::AuthRequired { server, tool } => f
                .debug_struct("AuthRequired")
                .field("server", server)
                .field("tool", &tool.tool_definition().name)
                .finish(),
            Self::OAuthUrl { server, url } => f
                .debug_struct("OAuthUrl")
                .field("server", server)
                .field("url", url)
                .finish(),
        }
    }
}

/// Manages a set of MCP server connections, reconnections, and tool registration.
#[derive(Clone)]
pub struct McpConnectionManager {
    servers: Vec<Arc<ServerHandle>>,
}

impl McpConnectionManager {
    /// Create a new manager for the given server configurations and return an event receiver.
    ///
    /// `defaults` is the global `[mcp]` section: its `default_deferred` feeds
    /// [`McpServerConfig::defers_tools`] at connect time, so servers injected
    /// after the per-server config was resolved (the auto-detected ones) defer
    /// on the same terms as configured ones.
    pub fn new(
        configs: Vec<McpServerConfig>,
        defaults: McpDefaults,
    ) -> (Self, mpsc::UnboundedReceiver<McpEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self::with_events(configs, defaults, Some(&tx)), rx)
    }

    /// Create a new manager without emitting events (useful in tests).
    #[cfg(test)]
    pub fn new_without_events(configs: Vec<McpServerConfig>, defaults: McpDefaults) -> Self {
        Self::with_events(configs, defaults, None)
    }

    /// A handle for each server, each with the wire-safe part its tools are
    /// qualified with ([`crate::tool_name::server_parts`]).
    fn with_events(
        configs: Vec<McpServerConfig>,
        defaults: McpDefaults,
        event_tx: Option<&mpsc::UnboundedSender<McpEvent>>,
    ) -> Self {
        let names: Vec<&str> = configs.iter().map(|config| config.name.as_str()).collect();
        let parts = crate::tool_name::server_parts(&names);
        let servers = configs
            .into_iter()
            .zip(parts)
            .map(|(config, part)| {
                let server = Arc::new(ServerHandle::new(config, part, defaults, event_tx.cloned()));
                server.set_self_ref(Arc::downgrade(&server));
                server
            })
            .collect();
        Self { servers }
    }

    /// The servers whose names have characters model APIs refuse in tool
    /// names, with the part their tools are qualified with instead.
    #[must_use]
    pub fn renamed_servers(&self) -> Vec<(String, String)> {
        self.servers
            .iter()
            .filter(|server| server.server_part() != server.name())
            .map(|server| (server.name().to_string(), server.server_part().to_string()))
            .collect()
    }

    /// Start managing all configured servers in the background.
    pub async fn start_all(&self) {
        for server in &self.servers {
            let server = server.clone();
            tokio::spawn(async move {
                server.run().await;
            });
        }
    }

    /// Cancel all background management tasks.
    pub fn stop_all(&self) {
        for server in &self.servers {
            server.task_cancel.cancel();
        }
    }

    /// Snapshot of current statuses keyed by server name.
    pub async fn statuses(&self) -> Vec<(String, McpStatus)> {
        let mut out = Vec::with_capacity(self.servers.len());
        for server in &self.servers {
            out.push((server.config.name.clone(), server.status().await));
        }
        out
    }

    /// Find a server by name.
    pub fn get(&self, name: &str) -> Option<Arc<ServerHandle>> {
        self.servers.iter().find(|s| s.name() == name).cloned()
    }
}

/// Handle to a single managed MCP server.
pub struct ServerHandle {
    config: McpServerConfig,
    /// The wire-safe part this server's tools are qualified with:
    /// `mcp__<server_part>__<tool>` ([`crate::tool_name`]).
    server_part: String,
    /// The global `[mcp] default_deferred` captured at manager construction.
    /// Not folded into `config` (the deferral decision needs the tool count,
    /// which only exists at connect time); consulted on every (re)connect,
    /// but since the value is frozen at startup, a changed default only
    /// takes effect after a restart.
    default_deferred: Option<bool>,
    status: Mutex<McpStatus>,
    client: Mutex<Option<Arc<McpClient>>>,
    tools: Mutex<Vec<Tool>>,
    /// Original tool name -> input schema property names, used for argument remapping.
    tool_properties: Mutex<HashMap<String, Vec<String>>>,
    task_cancel: CancellationToken,
    connection_cancel: Mutex<CancellationToken>,
    event_tx: Option<mpsc::UnboundedSender<McpEvent>>,
    auth_token: Mutex<Option<String>>,
    /// The OAuth grant behind `auth_token`, when this server uses OAuth: its
    /// refresh token and expiry.
    oauth_grant: Mutex<Option<OAuthToken>>,
    /// The token the live connection authenticated with — the one a 401
    /// during a call rejects.
    connected_token: Mutex<Option<String>>,
    /// Authorization-server discovery, cached for the session.
    discovery: Mutex<Option<oauth::Discovery>>,
    /// Where the OAuth grant is persisted.
    credentials: Option<CredentialStore>,
    /// A browser authorization is under way; a second is not started.
    authorizing: AtomicBool,
    self_ref: StdMutex<Weak<ServerHandle>>,
}

/// Why the watch over a live connection ended.
enum WatchEnd {
    /// The transport closed, or the manager stopped.
    Closed,
    /// A reconnect was requested — new credentials are in place.
    Reconnect,
}

impl ServerHandle {
    fn new(
        config: McpServerConfig,
        server_part: String,
        defaults: McpDefaults,
        event_tx: Option<mpsc::UnboundedSender<McpEvent>>,
    ) -> Self {
        let credentials = CredentialStore::for_server(&config.name);
        Self::with_credentials(config, server_part, defaults, event_tx, credentials)
    }

    /// A handle persisting its OAuth grant in `credentials`.
    fn with_credentials(
        config: McpServerConfig,
        server_part: String,
        defaults: McpDefaults,
        event_tx: Option<mpsc::UnboundedSender<McpEvent>>,
        credentials: Option<CredentialStore>,
    ) -> Self {
        let stored = credentials
            .as_ref()
            .and_then(|store| store.load().ok())
            .flatten();
        let initial_token = config
            .token
            .clone()
            .or_else(|| stored.as_ref().map(|token| token.access_token.clone()));
        let oauth_grant = if config.oauth.is_some() { stored } else { None };
        let initial_status = if config.disabled {
            McpStatus::Disabled
        } else {
            McpStatus::Pending
        };
        Self {
            config,
            server_part,
            default_deferred: defaults.default_deferred,
            status: Mutex::new(initial_status),
            client: Mutex::new(None),
            tools: Mutex::new(Vec::new()),
            tool_properties: Mutex::new(HashMap::new()),
            task_cancel: CancellationToken::new(),
            connection_cancel: Mutex::new(CancellationToken::new()),
            event_tx,
            auth_token: Mutex::new(initial_token),
            oauth_grant: Mutex::new(oauth_grant),
            connected_token: Mutex::new(None),
            discovery: Mutex::new(None),
            credentials,
            authorizing: AtomicBool::new(false),
            self_ref: StdMutex::new(Weak::new()),
        }
    }

    pub fn set_self_ref(&self, weak: Weak<ServerHandle>) {
        *self.self_ref.lock().unwrap() = weak;
    }

    async fn self_arc(&self) -> Arc<ServerHandle> {
        self.self_ref
            .lock()
            .unwrap()
            .upgrade()
            .expect("ServerHandle self-reference was not initialized")
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// The wire-safe part this server's tools are qualified with.
    pub fn server_part(&self) -> &str {
        &self.server_part
    }

    pub async fn status(&self) -> McpStatus {
        self.status.lock().await.clone()
    }

    #[cfg(test)]
    pub async fn auth_token(&self) -> Option<String> {
        self.auth_token.lock().await.clone()
    }

    async fn set_status(&self, status: McpStatus, message: Option<String>) {
        let changed = {
            let mut guard = self.status.lock().await;
            let changed = *guard != status;
            *guard = status.clone();
            changed
        };
        if changed {
            self.emit(McpEvent::Status {
                server: self.config.name.clone(),
                status,
                message,
            });
        }
    }

    fn emit(&self, event: McpEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }

    /// Main lifecycle loop: connect, discover tools, watch for close, reconnect.
    async fn run(self: Arc<Self>) {
        if self.config.disabled {
            return;
        }

        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(30);

        loop {
            if self.task_cancel.is_cancelled() {
                break;
            }

            let connection_cancel = self.reset_connection_cancel().await;
            self.set_status(McpStatus::Pending, None).await;
            // An expiring grant is renewed before it is sent, not after the
            // server rejects it.
            if self.grant_expires_within(oauth::REFRESH_AHEAD).await {
                self.refresh_grant().await;
            }

            match self.try_connect(&connection_cancel).await {
                Ok(client) => {
                    let client = Arc::new(client);
                    *self.client.lock().await = Some(client.clone());

                    // Clear any stale tool definitions from a previous connection
                    // so rediscovery re-emits the current set.
                    self.clear_tools().await;

                    match self.discover_and_emit(&client).await {
                        Ok(discovered) => {
                            self.set_status(McpStatus::Connected, None).await;
                            self.emit_added(discovered);
                            backoff = Duration::from_secs(1);

                            // Block until the transport closes or we are asked to reconnect.
                            let end = self.watch_close(&client, &connection_cancel).await;

                            // Clean up tools and schedule a reconnect.
                            self.clear_tools().await;
                            *self.client.lock().await = None;
                            if matches!(end, WatchEnd::Reconnect) {
                                // New credentials: reconnect now, not after a backoff.
                                continue;
                            }
                            // What the server said last is why, if it crashed.
                            let stderr = client
                                .stderr_tail()
                                .map_or_else(String::new, |tail| format!("\nstderr: {tail}"));
                            self.set_status(
                                McpStatus::Failed(format!("transport closed{stderr}")),
                                Some(format!("transport closed; reconnecting{stderr}")),
                            )
                            .await;

                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(max_backoff);
                        }
                        Err(e) => {
                            let is_fatal = e.is_fatal();
                            let token_acquired = self.handle_connection_error(e).await;
                            if is_fatal {
                                break;
                            }
                            if token_acquired {
                                backoff = Duration::from_secs(1);
                                continue;
                            }
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(max_backoff);
                        }
                    }
                }
                Err(e) => {
                    let is_fatal = e.is_fatal();
                    let token_acquired = self.handle_connection_error(e).await;
                    if is_fatal {
                        break;
                    }
                    if token_acquired {
                        // OAuth succeeded; reconnect immediately with the new token.
                        backoff = Duration::from_secs(1);
                        continue;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }

    /// Handle a connection error. Returns `true` if an OAuth flow was started
    /// and completed successfully, meaning the caller should reconnect immediately.
    async fn handle_connection_error(&self, error: McpError) -> bool {
        let is_auth = matches!(&error, McpError::Service(err) if is_auth_error(err.as_ref()))
            || matches!(&error, McpError::Init { source, .. } if {
                let text = format!("{source}");
                text.contains("401") || text.contains("403")
            });
        let msg = format!("{error}");
        if !is_auth {
            self.set_status(McpStatus::Failed(msg.clone()), Some(msg))
                .await;
            return false;
        }
        // A refresh token can mint a new access token without the user.
        if self.refresh_grant().await {
            return true;
        }

        if let Some(oauth) = &self.config.oauth {
            self.set_status(McpStatus::NeedsAuth, Some(msg)).await;
            if self.authorizing.swap(true, Ordering::SeqCst) {
                // A sign-in started from a rejected tool call is under way.
                return false;
            }
            let outcome = self.run_oauth_flow(oauth).await;
            self.authorizing.store(false, Ordering::SeqCst);
            match outcome {
                Ok(token) => {
                    self.adopt_grant(token).await;
                    true
                }
                Err(e) => {
                    warn!(
                        "OAuth flow for MCP server '{}' failed: {e}",
                        self.config.name
                    );
                    self.set_status(
                        McpStatus::Failed(format!("OAuth failed: {e}")),
                        Some(format!("OAuth failed: {e}")),
                    )
                    .await;
                    false
                }
            }
        } else {
            self.set_status(McpStatus::NeedsAuth, Some(msg)).await;
            self.emit(McpEvent::AuthRequired {
                server: self.config.name.clone(),
                tool: Arc::new(McpAuthTool::new(self.self_arc().await)) as Arc<dyn ExecutableTool>,
            });
            false
        }
    }

    /// The authorization server's metadata, discovered from the server's URL
    /// once per session. `None` for a server without a URL, or when the
    /// server publishes none.
    async fn discovery(&self) -> Option<oauth::Discovery> {
        let url = self.config.url.as_deref()?;
        let mut cached = self.discovery.lock().await;
        if cached.is_none() {
            let client = lm_bridge::http::client_builder()
                .timeout(Duration::from_secs(15))
                .build()
                .ok()?;
            match oauth::discover(&client, url).await {
                Ok(found) => *cached = Some(found),
                Err(e) => {
                    info!(
                        "MCP server '{}': no OAuth metadata discovered ({e})",
                        self.config.name
                    );
                    return None;
                }
            }
        }
        cached.clone()
    }

    /// The authorization and token endpoints: configured, or discovered.
    async fn oauth_endpoints(
        &self,
        oauth: &crate::config::McpOAuthConfig,
    ) -> Result<(String, String, Option<oauth::Discovery>), OAuthError> {
        let discovery = self.discovery().await;
        let pick = |configured: &str, discovered: Option<&String>, what: &str| {
            if !configured.is_empty() {
                Ok(configured.to_string())
            } else {
                discovered.cloned().ok_or_else(|| {
                    OAuthError::Discovery(format!(
                        "no {what} configured for MCP server '{}', and none discovered",
                        self.config.name
                    ))
                })
            }
        };
        let auth_url = pick(
            &oauth.auth_url,
            discovery
                .as_ref()
                .map(|d| &d.metadata.authorization_endpoint),
            "auth_url",
        )?;
        let token_url = pick(
            &oauth.token_url,
            discovery.as_ref().map(|d| &d.metadata.token_endpoint),
            "token_url",
        )?;
        Ok((auth_url, token_url, discovery))
    }

    /// Whether the OAuth grant expires within `window` and can be refreshed.
    async fn grant_expires_within(&self, window: Duration) -> bool {
        self.oauth_grant
            .lock()
            .await
            .as_ref()
            .is_some_and(|grant| grant.refresh_token.is_some() && grant.expires_within(window))
    }

    /// Refresh the OAuth grant with its refresh token. Returns whether a fresh
    /// access token is now in place.
    async fn refresh_grant(&self) -> bool {
        let Some(oauth) = &self.config.oauth else {
            return false;
        };
        let refresh = self
            .oauth_grant
            .lock()
            .await
            .as_ref()
            .and_then(|grant| grant.refresh_token.clone());
        let Some(refresh) = refresh else {
            return false;
        };
        let token_url = match self.oauth_endpoints(oauth).await {
            Ok((_, token_url, _)) => token_url,
            Err(e) => {
                warn!(
                    "cannot refresh the OAuth token for MCP server '{}': {e}",
                    self.config.name
                );
                return false;
            }
        };
        match oauth::refresh_token(
            &token_url,
            &refresh,
            &oauth.client_id,
            oauth.client_secret.as_deref(),
        )
        .await
        {
            Ok(token) => {
                info!(
                    "refreshed the OAuth token for MCP server '{}'",
                    self.config.name
                );
                self.adopt_grant(token).await;
                true
            }
            Err(e) => {
                warn!(
                    "refreshing the OAuth token for MCP server '{}' failed: {e}",
                    self.config.name
                );
                false
            }
        }
    }

    /// Make `token` the credentials: in memory, and persisted.
    async fn adopt_grant(&self, token: OAuthToken) {
        *self.auth_token.lock().await = Some(token.access_token.clone());
        if let Some(store) = &self.credentials
            && let Err(e) = store.save(&token)
        {
            warn!(
                "could not save the OAuth token for MCP server '{}': {e}",
                self.config.name
            );
        }
        *self.oauth_grant.lock().await = Some(token);
    }

    async fn run_oauth_flow(
        &self,
        oauth: &crate::config::McpOAuthConfig,
    ) -> Result<OAuthToken, OAuthError> {
        let (auth_url, token_url, discovery) = self.oauth_endpoints(oauth).await?;
        let pkce = PkcePair::new();
        let state = random_state();
        let redirect = redirect_uri(oauth.redirect_port);
        let scopes = oauth::request_scopes(&self.config.oauth_scopes(), discovery.as_ref());
        let url = oauth::build_authorization_url(
            &auth_url,
            &oauth.client_id,
            &redirect,
            &pkce.challenge,
            &state,
            &scopes,
        )?;
        self.emit(McpEvent::OAuthUrl {
            server: self.config.name.clone(),
            url,
        });

        let shutdown = self.task_cancel.child_token();
        let code = tokio::time::timeout(
            Duration::from_secs(300),
            oauth::start_callback_listener(oauth.redirect_port, state, shutdown),
        )
        .await
        .map_err(|_| OAuthError::Timeout)??;

        oauth::exchange_code(
            &token_url,
            &code,
            &redirect,
            &pkce.verifier,
            &oauth.client_id,
            oauth.client_secret.as_deref(),
        )
        .await
    }

    async fn reset_connection_cancel(&self) -> CancellationToken {
        let token = self.task_cancel.child_token();
        *self.connection_cancel.lock().await = token.clone();
        token
    }

    async fn try_connect(
        &self,
        connection_cancel: &CancellationToken,
    ) -> Result<McpClient, McpError> {
        let token = self.auth_token.lock().await.clone();
        *self.connected_token.lock().await = token.clone();
        let connect_fut: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<McpClient, McpError>> + Send + '_>,
        > = if let Some(url) = &self.config.url {
            Box::pin(McpClient::connect_http(url, token.as_deref()))
        } else if let Some(cmd) = &self.config.command {
            Box::pin(McpClient::connect_command(
                cmd,
                &self.config.args,
                &self.config.env,
                self.config.cwd.as_deref(),
            ))
        } else {
            return Err(McpError::Config(
                "MCP server config must specify either `command` or `url`".to_string(),
            ));
        };

        tokio::select! {
            _ = connection_cancel.cancelled() => {
                Err(McpError::Config("connection cancelled".to_string()))
            }
            result = tokio::time::timeout(self.config.startup_timeout(), connect_fut) => {
                result.map_err(|_| {
                    McpError::Service(Box::new(rmcp::service::ServiceError::Timeout {
                        timeout: self.config.startup_timeout(),
                    }))
                })?
            }
        }
    }

    /// Announce newly discovered tools.
    fn emit_added(&self, (tools, deferred): (Vec<Arc<dyn ExecutableTool>>, bool)) {
        if !tools.is_empty() {
            self.emit(McpEvent::ToolsAdded {
                server: self.config.name.clone(),
                tools,
                deferred,
            });
        }
    }

    /// List the server's tools, remember them in place of the ones known
    /// before, and return the new ones and whether this server's tools are
    /// deferred. A known tool the server no longer lists is reported removed.
    async fn discover_and_emit(
        &self,
        client: &Arc<McpClient>,
    ) -> Result<(Vec<Arc<dyn ExecutableTool>>, bool), McpError> {
        let infos = tokio::time::timeout(self.config.tool_timeout(), client.list_tools())
            .await
            .map_err(|_| {
                McpError::Service(Box::new(rmcp::service::ServiceError::Timeout {
                    timeout: self.config.tool_timeout(),
                }))
            })??;

        let self_arc = self.self_arc().await;
        let deferred = self.config.defers_tools(infos.len(), self.default_deferred);
        let mut added = Vec::new();
        let mut guard = self.tools.lock().await;
        let mut properties_guard = self.tool_properties.lock().await;
        let old_names: std::collections::HashSet<String> =
            guard.iter().map(|t| t.name.clone()).collect();
        properties_guard.clear();
        guard.clear();

        let mut taken = std::collections::HashSet::new();
        for McpToolInfo { tool, annotations } in infos {
            let annotations = self.config.effective_annotations(annotations);
            let qualified =
                crate::tool_name::unique_tool_name(&self.server_part, &tool.name, &taken);
            taken.insert(qualified.clone());
            let definition = Tool {
                name: qualified.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            };
            if !old_names.contains(&qualified) {
                let mcp_tool = Arc::new(McpTool::new(
                    self_arc.clone(),
                    tool.name.clone(),
                    definition.clone(),
                    annotations.clone(),
                )) as Arc<dyn ExecutableTool>;
                added.push(mcp_tool);
            }
            properties_guard.insert(tool.name.clone(), schema_properties(&tool.parameters));
            guard.push(definition);
        }
        let mut removed: Vec<String> = old_names.difference(&taken).cloned().collect();
        if !removed.is_empty() {
            removed.sort();
            self.emit(McpEvent::ToolsRemoved {
                server: self.config.name.clone(),
                tools: removed,
            });
        }
        Ok((added, deferred))
    }

    async fn clear_tools(&self) {
        let current_tools = {
            let guard = self.tools.lock().await;
            guard.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
        };
        if !current_tools.is_empty() {
            self.emit(McpEvent::ToolsRemoved {
                server: self.config.name.clone(),
                tools: current_tools,
            });
        }
        self.tools.lock().await.clear();
        self.tool_properties.lock().await.clear();
    }

    /// Return the original tool names currently known for this server.
    pub async fn tool_names(&self) -> Vec<String> {
        self.tool_properties.lock().await.keys().cloned().collect()
    }

    /// Return the input property names for a given original tool name, if known.
    pub async fn properties_for(&self, tool_name: &str) -> Vec<String> {
        self.tool_properties
            .lock()
            .await
            .get(tool_name)
            .cloned()
            .unwrap_or_default()
    }

    /// Wait until the connection closes or a reconnect is requested. An OAuth
    /// grant about to expire is refreshed on the way, and the connection is
    /// then remade with the new token (it was sent when connecting).
    async fn watch_close(
        &self,
        client: &Arc<McpClient>,
        connection_cancel: &CancellationToken,
    ) -> WatchEnd {
        let interval = Duration::from_millis(500);
        let mut refresh_at = self.refresh_deadline().await;
        loop {
            let refresh_due = async {
                match refresh_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = self.task_cancel.cancelled() => return WatchEnd::Closed,
                _ = connection_cancel.cancelled() => return WatchEnd::Reconnect,
                _ = tokio::time::sleep(interval) => {
                    if client.is_transport_closed() {
                        return WatchEnd::Closed;
                    }
                }
                () = refresh_due => {
                    if self.refresh_grant().await {
                        return WatchEnd::Reconnect;
                    }
                    refresh_at = None;
                }
            }
        }
    }

    /// When the OAuth grant should be refreshed: [`oauth::REFRESH_AHEAD`]
    /// before it expires. `None` without a refreshable, expiring grant.
    async fn refresh_deadline(&self) -> Option<tokio::time::Instant> {
        let grant = self.oauth_grant.lock().await;
        let grant = grant.as_ref()?;
        grant.refresh_token.as_ref()?;
        let expires_at = grant.expires_at?;
        let until_expiry = (expires_at - chrono::Utc::now())
            .to_std()
            .unwrap_or_default();
        Some(tokio::time::Instant::now() + until_expiry.saturating_sub(oauth::REFRESH_AHEAD))
    }

    /// Call a tool by its original (unqualified) name with timeout, rediscovery, and one retry.
    pub async fn call_tool(
        &self,
        original_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<crate::client::McpToolOutput, McpError> {
        let client = {
            let guard = self.client.lock().await;
            guard
                .as_ref()
                .ok_or_else(|| McpError::Config("server is not connected".to_string()))?
                .clone()
        };

        let timeout = self.config.tool_timeout();
        let result =
            tokio::time::timeout(timeout, client.call_tool(original_name, arguments.clone()))
                .await
                .map_err(|_| {
                    McpError::Service(Box::new(rmcp::service::ServiceError::Timeout { timeout }))
                })?;

        match result {
            Ok(output) => Ok(output),
            // Rejected credentials mid-session: the token expired or was
            // revoked after the connection was made.
            Err(McpError::Service(err)) if is_auth_error(err.as_ref()) => {
                self.recover_unauthorized(&client, original_name, arguments, timeout)
                    .await
            }
            Err(McpError::Service(err)) if is_method_missing_error(&err) => {
                warn!(
                    "MCP server '{}' reported missing method for tool '{original_name}'; rediscovering",
                    self.config.name
                );
                let discovered = self.discover_and_emit(&client).await?;
                self.emit_added(discovered);
                tokio::time::timeout(timeout, client.call_tool(original_name, arguments))
                    .await
                    .map_err(|_| {
                        McpError::Service(Box::new(rmcp::service::ServiceError::Timeout {
                            timeout,
                        }))
                    })?
            }
            // The transport died mid-call — the child process almost certainly
            // exited, so the request never completed on a live connection and
            // recovering it cannot double-execute. Reconnect once and retry on the
            // fresh client, turning a dropped MCP server into a slow call rather
            // than a failed turn. Every failure where the transport is still open
            // (a real tool error, a timeout) falls through and is returned as-is.
            Err(other) if client.is_transport_closed() => {
                match self.await_reconnected_client().await {
                    Some(fresh) => {
                        warn!(
                            "MCP server '{}' transport closed mid-call for tool '{original_name}'; reconnected, retrying",
                            self.config.name
                        );
                        tokio::time::timeout(timeout, fresh.call_tool(original_name, arguments))
                            .await
                            .map_err(|_| {
                                McpError::Service(Box::new(rmcp::service::ServiceError::Timeout {
                                    timeout,
                                }))
                            })?
                    }
                    None => Err(other),
                }
            }
            Err(other) => Err(other),
        }
    }

    /// A call was rejected as unauthorized. With a refresh token, refresh,
    /// reconnect and retry the call once. Otherwise forget the rejected token
    /// — compare-and-clear, so credentials that replaced it meanwhile
    /// survive — mark the server as needing authorization, start
    /// re-authorization, and tell the model how it proceeds.
    async fn recover_unauthorized(
        &self,
        client: &Arc<McpClient>,
        original_name: &str,
        arguments: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<crate::client::McpToolOutput, McpError> {
        let rejected = self.connected_token.lock().await.clone();
        if self.refresh_grant().await
            && let Some(fresh) = self.reconnect_now(client).await
        {
            info!(
                "MCP server '{}' rejected an expired token; refreshed it and retrying \
                     '{original_name}'",
                self.config.name
            );
            return tokio::time::timeout(timeout, fresh.call_tool(original_name, arguments))
                .await
                .map_err(|_| {
                    McpError::Service(Box::new(rmcp::service::ServiceError::Timeout { timeout }))
                })?;
        }
        self.forget_rejected(rejected.as_deref()).await;
        self.set_status(
            McpStatus::NeedsAuth,
            Some("the server rejected its credentials during a tool call".to_string()),
        )
        .await;
        self.begin_reauthorization().await;
        Err(McpError::Unauthorized(self.reauthorization_hint()))
    }

    /// Forget `rejected` wherever it is still the current credential: in
    /// memory, in the OAuth grant, and on disk.
    async fn forget_rejected(&self, rejected: Option<&str>) {
        let Some(rejected) = rejected else {
            return;
        };
        {
            let mut current = self.auth_token.lock().await;
            if current.as_deref() == Some(rejected) {
                *current = None;
            }
        }
        let mut grant = self.oauth_grant.lock().await;
        if grant.as_ref().is_some_and(|g| g.access_token == rejected) {
            *grant = None;
            if let Some(store) = &self.credentials {
                let stored = store.load().ok().flatten();
                if stored.is_some_and(|token| token.access_token == rejected)
                    && let Err(e) = store.clear()
                {
                    warn!(
                        "could not clear the rejected token of MCP server '{}': {e}",
                        self.config.name
                    );
                }
            }
        }
    }

    /// Start getting new credentials: the OAuth sign-in, in the background
    /// (the browser step can take minutes, and a tool call must not wait for
    /// it), or — without OAuth — the `authenticate` tool.
    async fn begin_reauthorization(&self) {
        if self.config.oauth.is_none() {
            self.emit(McpEvent::AuthRequired {
                server: self.config.name.clone(),
                tool: Arc::new(McpAuthTool::new(self.self_arc().await)) as Arc<dyn ExecutableTool>,
            });
            return;
        }
        if self.authorizing.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.self_arc().await;
        tokio::spawn(async move {
            if let Some(oauth) = this.config.oauth.as_ref() {
                match this.run_oauth_flow(oauth).await {
                    Ok(token) => {
                        this.adopt_grant(token).await;
                        // Reconnect with the new token.
                        this.connection_cancel.lock().await.cancel();
                    }
                    Err(e) => {
                        warn!(
                            "OAuth flow for MCP server '{}' failed: {e}",
                            this.config.name
                        );
                        this.set_status(
                            McpStatus::Failed(format!("OAuth failed: {e}")),
                            Some(format!("OAuth failed: {e}")),
                        )
                        .await;
                    }
                }
            }
            this.authorizing.store(false, Ordering::SeqCst);
        });
    }

    /// What the model is told when a server needs re-authorization.
    fn reauthorization_hint(&self) -> String {
        let name = &self.config.name;
        if self.config.oauth.is_some() {
            format!(
                "MCP server '{name}' rejected its credentials (HTTP 401) and needs \
                 re-authorization. Rusta started its sign-in again: once the authorization at \
                 the URL it printed is complete, retry the call."
            )
        } else {
            let auth_tool =
                crate::tool_name::qualified_tool_name(&self.server_part, "authenticate");
            format!(
                "MCP server '{name}' rejected its credentials (HTTP 401) and needs \
                 re-authorization. Ask the user for a valid token, pass it to `{auth_tool}`, \
                 then retry the call."
            )
        }
    }

    /// Remake the connection now and return the new client, once one other
    /// than `old` is live — or `None` if none is within ten seconds.
    async fn reconnect_now(&self, old: &Arc<McpClient>) -> Option<Arc<McpClient>> {
        self.connection_cancel.lock().await.cancel();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(client) = self.client.lock().await.as_ref()
                && !Arc::ptr_eq(client, old)
                && !client.is_transport_closed()
            {
                return Some(Arc::clone(client));
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
        }
    }

    /// Wait, bounded, for the lifecycle loop to reconnect after a mid-call
    /// transport death and return a fresh, live client — or `None` if none becomes
    /// healthy in time. Nudges the loop to reconnect now (the same mechanism
    /// [`Self::authenticate`] uses) instead of waiting for the close watcher.
    async fn await_reconnected_client(&self) -> Option<Arc<McpClient>> {
        self.connection_cancel.lock().await.cancel();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(client) = self.client.lock().await.as_ref()
                && !client.is_transport_closed()
            {
                return Some(Arc::clone(client));
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
        }
    }

    /// Provide a bearer token for this server and trigger a reconnect.
    pub async fn authenticate(&self, token: String) {
        info!("Authenticating MCP server '{}'", self.config.name);
        *self.auth_token.lock().await = Some(token);
        // Cancel the current connection attempt so the lifecycle loop reconnects with the new token.
        self.connection_cancel.lock().await.cancel();
    }

    /// Return tool definitions currently known for this server, with qualified names.
    pub async fn tool_definitions(&self) -> Vec<Tool> {
        self.tools.lock().await.clone()
    }
}

/// Synthetic tool that supplies an authentication token to a server and triggers reconnect.
struct McpAuthTool {
    server: Arc<ServerHandle>,
    definition: Tool,
}

impl McpAuthTool {
    fn new(server: Arc<ServerHandle>) -> Self {
        let name = crate::tool_name::qualified_tool_name(server.server_part(), "authenticate");
        let definition = Tool {
            name,
            description: format!(
                "Provide a bearer token for the '{}' MCP server and reconnect.",
                server.name()
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "token": {
                        "type": "string",
                        "description": "Bearer token for the MCP server"
                    }
                },
                "required": ["token"]
            }),
        };
        Self { server, definition }
    }
}

#[async_trait]
impl ExecutableTool for McpAuthTool {
    fn tool_definition(&self) -> &Tool {
        &self.definition
    }

    fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
        ToolAccesses::none()
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        "silent"
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &agent_core::agent_loop::types::ExecutableToolContext,
        _tool_call_id: &str,
    ) -> agent_core::agent_loop::types::ExecutableToolResult {
        match input.get("token").and_then(|v| v.as_str()) {
            Some(token) => {
                self.server.authenticate(token.to_string()).await;
                agent_core::agent_loop::types::ExecutableToolResult::text(
                    "Authentication token set; reconnecting to MCP server.",
                )
            }
            None => agent_core::agent_loop::types::ExecutableToolResult::error(
                "Missing required 'token' argument",
            ),
        }
    }
}

pub fn infer_accesses(
    tool: &Tool,
    annotations: Option<&ToolAnnotations>,
    _server_name: &str,
) -> ToolAccesses {
    let text = format!("{} {}", tool.name, tool.description).to_lowercase();

    // Explicit annotations take precedence when present.
    if let Some(a) = annotations {
        if a.read_only_hint == Some(true) {
            if text.contains("file") || text.contains("path") || text.contains("dir") {
                return ToolAccesses::new(vec![ToolAccess::Read(PathBuf::from("."))]);
            }
            return ToolAccesses::none();
        }
        if a.destructive_hint == Some(true) {
            return ToolAccesses::read_write_tree(PathBuf::from("."));
        }
    }

    if text.contains("search")
        || text.contains("web")
        || text.contains("fetch")
        || text.contains("request")
    {
        return ToolAccesses::none();
    }

    if text.contains("read")
        || text.contains("list")
        || text.contains("get")
        || text.contains("find")
        || text.contains("open")
        || text.contains("view")
    {
        if text.contains("write")
            || text.contains("create")
            || text.contains("delete")
            || text.contains("update")
            || text.contains("edit")
        {
            return ToolAccesses::read_write_tree(PathBuf::from("."));
        }
        return ToolAccesses::new(vec![ToolAccess::Read(PathBuf::from("."))]);
    }

    if text.contains("write")
        || text.contains("create")
        || text.contains("delete")
        || text.contains("update")
        || text.contains("edit")
        || text.contains("shell")
        || text.contains("bash")
        || text.contains("execute")
        || text.contains("run")
    {
        return ToolAccesses::read_write_tree(PathBuf::from("."));
    }

    // Unknown tools default to a conservative read-write tree on the working directory.
    ToolAccesses::read_write_tree(PathBuf::from("."))
}

pub fn infer_approval_rule(tool: &Tool, annotations: Option<&ToolAnnotations>) -> &'static str {
    let text = format!("{} {}", tool.name, tool.description).to_lowercase();
    let runs_commands = text.contains("shell")
        || text.contains("bash")
        || text.contains("execute")
        || text.contains("run");

    // What the server declares comes first. A tool it says only reads needs no
    // approval, as `read_file` and `grep` need none; one it says changes
    // something is asked about, however much its name sounds like a search.
    match annotations.and_then(|a| a.read_only_hint) {
        Some(true) => return "silent",
        Some(false) if runs_commands => return "run_command",
        Some(false) => return "write_file",
        None => {}
    }

    if text.contains("search")
        || text.contains("web")
        || text.contains("fetch")
        || text.contains("request")
    {
        return "silent";
    }

    if runs_commands {
        return "run_command";
    }

    if text.contains("write")
        || text.contains("create")
        || text.contains("delete")
        || text.contains("update")
        || text.contains("edit")
    {
        return "write_file";
    }

    if text.contains("read")
        || text.contains("list")
        || text.contains("get")
        || text.contains("find")
        || text.contains("open")
        || text.contains("view")
    {
        return "read_file";
    }

    // Default unknown tools to the write-file approval rule to keep them user-gated.
    "write_file"
}

#[cfg(test)]
mod tests {
    use lm_bridge::tool::Tool;
    use rmcp::model::ToolAnnotations;

    use super::{infer_accesses, infer_approval_rule};

    fn tool(name: &str, description: &str) -> Tool {
        Tool::new(
            name.to_string(),
            description.to_string(),
            serde_json::json!({}),
        )
    }

    #[test]
    fn test_infer_web_search_is_unrestricted() {
        let t = tool("web_search", "Search the web");
        assert!(infer_accesses(&t, None, "srv").is_empty());
        assert_eq!(infer_approval_rule(&t, None), "silent");
    }

    #[test]
    fn test_infer_read_only_file_tool() {
        let t = tool("read_file", "Read a file from disk");
        let accesses = infer_accesses(&t, None, "srv");
        assert!(!accesses.is_empty());
        assert_eq!(infer_approval_rule(&t, None), "read_file");
    }

    #[test]
    fn test_infer_write_tool_requires_write_approval() {
        let t = tool("write_file", "Write a file to disk");
        assert_eq!(infer_approval_rule(&t, None), "write_file");
    }

    #[test]
    fn test_infer_shell_tool_requires_run_command_approval() {
        let t = tool("run_shell", "Execute a shell command");
        assert_eq!(infer_approval_rule(&t, None), "run_command");
    }

    /// A tool its server declares read-only runs without asking, whatever its
    /// name; one declared as changing something is asked about, though its
    /// name sounds like a search.
    #[test]
    fn the_servers_declaration_decides_the_approval() {
        let read_only = ToolAnnotations::new().read_only(true);
        let writes = ToolAnnotations::new().read_only(false);
        let t = tool("filesystem", "Access the filesystem");
        assert_eq!(infer_approval_rule(&t, Some(&read_only)), "silent");
        let t = tool("get_statistics", "Get database statistics");
        assert_eq!(infer_approval_rule(&t, Some(&read_only)), "silent");
        let t = tool("search_and_replace", "Search and replace text in files");
        assert_eq!(infer_approval_rule(&t, Some(&writes)), "write_file");
        let t = tool("run_tests", "Run the test suite");
        assert_eq!(infer_approval_rule(&t, Some(&writes)), "run_command");
        let t = tool("filesystem", "Access the filesystem");
        assert_eq!(
            infer_approval_rule(&t, Some(&ToolAnnotations::new())),
            infer_approval_rule(&t, None),
            "a declaration without the hint changes nothing"
        );
    }

    #[tokio::test]
    async fn test_config_token_is_seeded_as_auth_token() {
        use crate::config::McpServerConfig;
        let config = McpServerConfig {
            name: "test".to_string(),
            command: None,
            args: Vec::new(),
            env: Default::default(),
            url: Some("http://example.com".to_string()),
            token: Some("secret".to_string()),
            startup_timeout_ms: Some(1000),
            tool_timeout_ms: Some(1000),
            disabled: false,
            oauth: None,
            deferred: None,
            read_only: false,
            cwd: None,
        };
        let manager =
            super::McpConnectionManager::new_without_events(vec![config], Default::default());
        let server = manager.get("test").unwrap();
        assert_eq!(server.auth_token().await.as_deref(), Some("secret"));
    }

    #[test]
    fn deferral_follows_the_config_then_the_global_then_the_server_size() {
        use crate::config::{DEFER_TOOL_THRESHOLD, McpServerConfig};
        let mut config = McpServerConfig::default();
        // No global: the size rule stands exactly as before.
        assert!(!config.defers_tools(1, None), "a one-tool server is inline");
        assert!(!config.defers_tools(DEFER_TOOL_THRESHOLD, None));
        assert!(config.defers_tools(DEFER_TOOL_THRESHOLD + 1, None));
        config.deferred = Some(true);
        assert!(config.defers_tools(1, None));
        config.deferred = Some(false);
        assert!(!config.defers_tools(100, None));

        // Global on: even a one-tool server defers...
        config.deferred = None;
        assert!(config.defers_tools(1, Some(true)));
        assert!(config.defers_tools(DEFER_TOOL_THRESHOLD + 1, Some(true)));
        // ...unless the server opts out; per-server always wins.
        config.deferred = Some(false);
        assert!(!config.defers_tools(1, Some(true)));
        assert!(!config.defers_tools(100, Some(true)));
        config.deferred = Some(true);
        assert!(config.defers_tools(1, Some(false)));

        // Global off: the size rule is overridden, a big server stays inline.
        config.deferred = None;
        assert!(!config.defers_tools(DEFER_TOOL_THRESHOLD + 1, Some(false)));
    }

    /// A 401 mid-session, against a mock MCP-over-HTTP server.
    mod unauthorized_calls {
        use std::sync::Arc;
        use std::time::Duration;

        use serde_json::{Value, json};
        use tokio::sync::mpsc;
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

        use super::super::{McpEvent, McpStatus, ServerHandle};
        use crate::client::McpError;
        use crate::config::{McpOAuthConfig, McpServerConfig};
        use crate::oauth::{CredentialStore, OAuthToken};

        /// Answers MCP's JSON-RPC over HTTP; `tools/call` succeeds only with
        /// the bearer token `accepted`.
        struct McpOverHttp {
            accepted: &'static str,
        }

        impl Respond for McpOverHttp {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
                let reply = |result: Value| {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0", "id": body["id"], "result": result
                    }))
                };
                match body["method"].as_str().unwrap_or("") {
                    "initialize" => reply(json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "mock", "version": "1"}
                    })),
                    "tools/list" => reply(json!({"tools": [{
                        "name": "whoami",
                        "description": "Who is calling",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]})),
                    "tools/call" => {
                        let auth = request
                            .headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        if auth == format!("Bearer {}", self.accepted) {
                            reply(json!({"content": [{"type": "text", "text": "it is you"}]}))
                        } else {
                            ResponseTemplate::new(401).set_body_string("Unauthorized")
                        }
                    }
                    _ => ResponseTemplate::new(202),
                }
            }
        }

        async fn mock_server(accepted: &'static str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wiremock::matchers::path("/mcp"))
                .respond_with(McpOverHttp { accepted })
                .mount(&server)
                .await;
            // No server-initiated stream, and no session to delete.
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(405))
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(405))
                .mount(&server)
                .await;
            server
        }

        fn config(
            url: String,
            token: Option<&str>,
            oauth: Option<McpOAuthConfig>,
        ) -> McpServerConfig {
            McpServerConfig {
                name: "remote".to_string(),
                url: Some(url),
                token: token.map(str::to_string),
                startup_timeout_ms: Some(5_000),
                tool_timeout_ms: Some(5_000),
                oauth,
                ..Default::default()
            }
        }

        async fn start(
            config: McpServerConfig,
            credentials: Option<CredentialStore>,
        ) -> (Arc<ServerHandle>, mpsc::UnboundedReceiver<McpEvent>) {
            let (tx, rx) = mpsc::unbounded_channel();
            let part = crate::tool_name::server_part(&config.name);
            let handle = Arc::new(ServerHandle::with_credentials(
                config,
                part,
                Default::default(),
                Some(tx),
                credentials,
            ));
            handle.set_self_ref(Arc::downgrade(&handle));
            tokio::spawn(Arc::clone(&handle).run());
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while handle.status().await != McpStatus::Connected {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "never connected: {:?}",
                    handle.status().await
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            (handle, rx)
        }

        /// Without OAuth, a rejected token is forgotten, the server is marked
        /// as needing authorization, the `authenticate` tool is offered, and
        /// the model is told what to do.
        #[tokio::test]
        async fn a_rejected_token_is_cleared_and_the_model_told() {
            let mcp = mock_server("valid").await;
            let (handle, mut events) = start(
                config(format!("{}/mcp", mcp.uri()), Some("stale"), None),
                None,
            )
            .await;

            let err = handle
                .call_tool("whoami", Some(json!({})))
                .await
                .expect_err("the stale token is rejected");
            let McpError::Unauthorized(message) = &err else {
                panic!("expected Unauthorized, got {err:?}");
            };
            assert!(message.contains("mcp__remote__authenticate"), "{message}");
            assert_eq!(handle.status().await, McpStatus::NeedsAuth);
            assert_eq!(
                handle.auth_token().await,
                None,
                "the rejected token is cleared"
            );
            let mut offered = false;
            while let Ok(event) = events.try_recv() {
                offered |= matches!(event, McpEvent::AuthRequired { .. });
            }
            assert!(offered, "the authenticate tool is offered");
            handle.task_cancel.cancel();
        }

        /// With a refresh token, a rejected call is refreshed, reconnected
        /// and retried — the model never sees the 401.
        #[tokio::test]
        async fn an_expired_token_is_refreshed_and_the_call_retried() {
            let mcp = mock_server("fresh").await;
            Mock::given(method("POST"))
                .and(wiremock::matchers::path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": "fresh", "expires_in": 3600
                })))
                .mount(&mcp)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let store = CredentialStore::new(dir.path().join("remote.json"));
            store
                .save(&OAuthToken {
                    access_token: "old".to_string(),
                    token_type: Some("Bearer".to_string()),
                    refresh_token: Some("r-1".to_string()),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                })
                .unwrap();
            let oauth = McpOAuthConfig {
                client_id: "cli".to_string(),
                auth_url: format!("{}/authorize", mcp.uri()),
                token_url: format!("{}/token", mcp.uri()),
                ..Default::default()
            };
            let (handle, _events) = start(
                config(format!("{}/mcp", mcp.uri()), None, Some(oauth)),
                Some(store.clone()),
            )
            .await;

            let output = handle
                .call_tool("whoami", Some(json!({})))
                .await
                .expect("refreshed and retried");
            assert_eq!(output.text(), "it is you");
            assert_eq!(store.load().unwrap().unwrap().access_token, "fresh");
            assert_eq!(
                store.load().unwrap().unwrap().refresh_token.as_deref(),
                Some("r-1")
            );
            handle.task_cancel.cancel();
        }
    }
}
