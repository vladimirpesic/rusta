//! OAuth 2 for MCP servers: authorization code with PKCE, discovery of the
//! authorization server, and refresh.
//!
//! Discovery follows the MCP authorization spec: the server's protected-resource
//! metadata (RFC 9728) names its authorization server, whose metadata (RFC 8414,
//! or OpenID discovery) gives the endpoints and supported scopes. When that
//! server supports `offline_access`, it is requested so the grant comes with
//! a refresh token, and tokens are refreshed before they expire and when a
//! server rejects them.
//!
//! The metadata is the MCP server's to write, so the requests it leads to are
//! held to an [`AddressPolicy`]: an origin other than the server's own may be on
//! the public internet, or on an internal network of the kind the server itself
//! is on, and nowhere else. Responses are read up to [`MAX_RESPONSE_BYTES`].

use std::{collections::HashMap, path::PathBuf, time::Duration};

use agent_core::address::{self, AddressClass, AddressError};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Errors that can occur during the OAuth flow.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("URL error: {0}")]
    Url(#[from] url::ParseError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("state mismatch in callback")]
    StateMismatch,
    #[error("authorization failed: {0}")]
    AuthorizationFailed(String),
    #[error("missing authorization code")]
    MissingCode,
    #[error("listener timed out")]
    Timeout,
    #[error("shutdown")]
    Shutdown,
    #[error("authorization server discovery failed: {0}")]
    Discovery(String),
    #[error("{0}")]
    Unresolved(String),
    #[error(
        "refusing to send an OAuth request to {0}; an MCP server's metadata may lead only to \
the public internet and to the kind of network the server itself is on"
    )]
    Refused(String),
    #[error("the response from {0} is larger than {MAX_RESPONSE_BYTES} bytes")]
    TooLarge(String),
}

/// The scope that asks for a refresh token.
pub const OFFLINE_ACCESS_SCOPE: &str = "offline_access";

/// How long before expiry a token is refreshed.
pub const REFRESH_AHEAD: Duration = Duration::from_secs(120);

/// The most of a metadata or token response that is read: both are small JSON
/// documents.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Redirects followed while fetching metadata.
const MAX_REDIRECTS: usize = 5;

/// How long one metadata request may take.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one token request may take.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the requests of one MCP server's sign-in may go.
///
/// Discovery and the token requests follow URLs the server's metadata names,
/// so a malicious server could aim them at a service that only the user's
/// machine or network can reach. The server's own origin is as trusted as its
/// configured URL. Any other origin must resolve to public addresses, or to
/// internal ones of a kind the server's own host resolves to: a server on
/// `localhost` may name an authorization server on `localhost`, and a server on
/// a private network one on a private network. Endpoints the user configured
/// are not restricted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressPolicy {
    /// The server's origin; `None` when nothing is restricted.
    server: Option<url::Origin>,
    /// The internal networks the server's own host is on.
    internal: Vec<AddressClass>,
}

impl AddressPolicy {
    /// No restriction, for endpoints the user configured.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self {
            server: None,
            internal: Vec::new(),
        }
    }

    /// The policy for the MCP server at `server_url`.
    pub async fn for_server(server_url: &url::Url) -> Self {
        let mut internal = Vec::new();
        if let Ok(addrs) = address::resolve_allowed(server_url, |_| true).await {
            for addr in addrs {
                let class = AddressClass::of(addr.ip());
                if class.is_internal() && !internal.contains(&class) {
                    internal.push(class);
                }
            }
        }
        Self {
            server: Some(server_url.origin()),
            internal,
        }
    }

    /// Whether an origin other than the server's may resolve to `class`.
    #[must_use]
    pub fn allows(&self, class: AddressClass) -> bool {
        !class.is_internal() || self.internal.contains(&class)
    }

    /// A client for one request to `url` that follows no redirect, and that,
    /// unless the policy trusts `url`, connects only to the addresses it
    /// allowed.
    async fn client_for(
        &self,
        url: &url::Url,
        timeout: Duration,
    ) -> Result<reqwest::Client, OAuthError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(OAuthError::Refused(format!(
                "{url}: only http and https are allowed"
            )));
        }
        let builder = lm_bridge::http::client_builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none());
        let trusted = match &self.server {
            None => true,
            Some(origin) => url.origin() == *origin,
        };
        if trusted {
            return Ok(builder.build()?);
        }
        let addrs = address::resolve_allowed(url, |class| self.allows(class))
            .await
            .map_err(|e| match e {
                AddressError::Unresolved(reason) => OAuthError::Unresolved(reason),
                refused @ AddressError::Refused { .. } => {
                    OAuthError::Refused(format!("{url}: {refused}"))
                }
            })?;
        let host = url.host_str().unwrap_or_default();
        Ok(builder.resolve_to_addrs(host, &addrs).build()?)
    }
}

/// Read `response`'s body, up to [`MAX_RESPONSE_BYTES`].
async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, OAuthError> {
    let too_large = |response: &reqwest::Response| OAuthError::TooLarge(response.url().to_string());
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(too_large(&response));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(too_large(&response));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// What an authorization server publishes about itself (RFC 8414).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AuthServerMetadata {
    #[serde(default)]
    pub issuer: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
}

/// What an MCP server publishes about how to authorize against it (RFC 9728).
#[derive(Debug, Clone, Default, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
}

/// The result of authorization-server discovery for one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub metadata: AuthServerMetadata,
    /// Scopes the protected resource itself advertises, if it said.
    pub resource_scopes: Option<Vec<String>>,
    /// Where requests to the discovered endpoints may go.
    pub policy: AddressPolicy,
}

/// `{origin}/.well-known/{suffix}{path}` for `url`: the path-aware location
/// RFC 8414 and RFC 9728 define, and the origin-only one as a fallback.
fn well_known_candidates(url: &url::Url, suffix: &str) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let mut candidates = Vec::new();
    if !path.is_empty() {
        candidates.push(format!("{origin}/.well-known/{suffix}{path}"));
    }
    candidates.push(format!("{origin}/.well-known/{suffix}"));
    candidates
}

/// GET `url` as JSON, following redirects under `policy`. `None` when it is
/// missing, unreadable or too large; an error when it, or a redirect, leads
/// where `policy` refuses.
async fn fetch_json<T: serde::de::DeserializeOwned>(
    policy: &AddressPolicy,
    url: &str,
) -> Result<Option<T>, OAuthError> {
    let mut current = url::Url::parse(url)?;
    for _ in 0..=MAX_REDIRECTS {
        let client = match policy.client_for(&current, DISCOVERY_TIMEOUT).await {
            Ok(client) => client,
            Err(refused @ OAuthError::Refused(_)) => return Err(refused),
            Err(_) => return Ok(None),
        };
        let Ok(response) = client
            .get(current.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
        else {
            return Ok(None);
        };
        let status = response.status();
        if status.is_redirection() {
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|location| location.to_str().ok())
                .and_then(|location| current.join(location).ok());
            match next {
                Some(next) => {
                    current = next;
                    continue;
                }
                None => return Ok(None),
            }
        }
        if !status.is_success() {
            return Ok(None);
        }
        return Ok(read_limited(response)
            .await
            .ok()
            .and_then(|body| serde_json::from_slice(&body).ok()));
    }
    Ok(None)
}

/// Find the authorization server for the MCP server at `server_url`.
///
/// Its protected-resource metadata names the authorization server; a server
/// without that metadata is taken to be its own. The authorization server's
/// metadata comes from RFC 8414's well-known location, or OpenID discovery.
pub async fn discover(server_url: &str) -> Result<Discovery, OAuthError> {
    let resource = url::Url::parse(server_url)?;
    let policy = AddressPolicy::for_server(&resource).await;
    let mut resource_meta = ProtectedResourceMetadata::default();
    for candidate in well_known_candidates(&resource, "oauth-protected-resource") {
        if let Some(meta) = fetch_json::<ProtectedResourceMetadata>(&policy, &candidate).await? {
            resource_meta = meta;
            break;
        }
    }
    let issuer = match resource_meta.authorization_servers.first() {
        Some(issuer) => url::Url::parse(issuer)?,
        None => {
            let mut origin = resource.clone();
            origin.set_path("");
            origin.set_query(None);
            origin
        }
    };
    let mut candidates = well_known_candidates(&issuer, "oauth-authorization-server");
    candidates.extend(well_known_candidates(&issuer, "openid-configuration"));
    let issuer_path = issuer.path().trim_end_matches('/');
    if !issuer_path.is_empty() {
        // OpenID discovery appends the suffix to the issuer's path instead.
        candidates.push(format!(
            "{}{issuer_path}/.well-known/openid-configuration",
            issuer.origin().ascii_serialization()
        ));
    }
    for candidate in candidates {
        if let Some(metadata) = fetch_json::<AuthServerMetadata>(&policy, &candidate).await? {
            return Ok(Discovery {
                metadata,
                resource_scopes: resource_meta.scopes_supported,
                policy,
            });
        }
    }
    Err(OAuthError::Discovery(format!(
        "no authorization-server metadata found for {issuer}"
    )))
}

/// The scopes to request: the configured ones, else the resource's advertised
/// ones, plus `offline_access` when the authorization server supports it and
/// it is not already there.
#[must_use]
pub fn request_scopes(configured: &[String], discovery: Option<&Discovery>) -> Vec<String> {
    let mut scopes: Vec<String> = if configured.is_empty() {
        discovery
            .and_then(|d| d.resource_scopes.clone())
            .unwrap_or_default()
    } else {
        configured.to_vec()
    };
    let offers_offline = discovery
        .and_then(|d| d.metadata.scopes_supported.as_ref())
        .is_some_and(|supported| supported.iter().any(|s| s == OFFLINE_ACCESS_SCOPE));
    if offers_offline && !scopes.iter().any(|s| s == OFFLINE_ACCESS_SCOPE) {
        scopes.push(OFFLINE_ACCESS_SCOPE.to_string());
    }
    scopes
}

/// A PKCE verifier/challenge pair.
#[derive(Debug, Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
    pub method: String,
}

impl PkcePair {
    /// Generate a random PKCE pair using the S256 method.
    pub fn new() -> Self {
        let verifier = random_verifier();
        let challenge = s256_challenge(&verifier);
        Self {
            verifier,
            challenge,
            method: "S256".to_string(),
        }
    }
}

impl Default for PkcePair {
    fn default() -> Self {
        Self::new()
    }
}

fn random_verifier() -> String {
    let mut bytes = [0u8; 64];
    fastrand::Rng::new().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn s256_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Build an authorization URL with PKCE and CSRF state parameters.
pub fn build_authorization_url(
    auth_url: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    scopes: &[String],
) -> Result<String, url::ParseError> {
    let mut url = url::Url::parse(auth_url)?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        if !scopes.is_empty() {
            pairs.append_pair("scope", &scopes.join(" "));
        }
    }
    Ok(url.to_string())
}

/// An OAuth access token persisted to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl OAuthToken {
    /// Whether the token expires within `window` (or already has). A token
    /// without an expiry never does.
    #[must_use]
    pub fn expires_within(&self, window: Duration) -> bool {
        let Some(expires_at) = self.expires_at else {
            return false;
        };
        chrono::Duration::from_std(window)
            .ok()
            .and_then(|window| chrono::Utc::now().checked_add_signed(window))
            .is_none_or(|limit| expires_at <= limit)
    }
}

/// Parse a token endpoint's response.
fn parse_token_response(body: &str) -> Result<OAuthToken, OAuthError> {
    let mut raw: HashMap<String, serde_json::Value> = serde_json::from_str(body)?;
    let access_token = raw
        .remove("access_token")
        .and_then(|v| v.as_str().map(ToString::to_string))
        .ok_or_else(|| OAuthError::AuthorizationFailed("missing access_token".to_string()))?;
    let token_type = raw
        .remove("token_type")
        .and_then(|v| v.as_str().map(ToString::to_string));
    let refresh_token = raw
        .remove("refresh_token")
        .and_then(|v| v.as_str().map(ToString::to_string));
    let expires_at = raw.remove("expires_in").and_then(|v| {
        let seconds = v.as_u64()?;
        Some(chrono::Utc::now() + chrono::Duration::seconds(i64::try_from(seconds).ok()?))
    });
    Ok(OAuthToken {
        access_token,
        token_type,
        refresh_token,
        expires_at,
    })
}

/// POST `params` to the token endpoint, under `policy`, and parse the token it
/// returns.
async fn token_request(
    token_url: &str,
    policy: &AddressPolicy,
    params: &[(&str, &str)],
) -> Result<OAuthToken, OAuthError> {
    let url = url::Url::parse(token_url)?;
    let client = policy.client_for(&url, TOKEN_TIMEOUT).await?;
    let response = client.post(url).form(params).send().await?;
    let status = response.status();
    let body = read_limited(response).await?;
    let body = String::from_utf8_lossy(&body);
    if !status.is_success() {
        return Err(OAuthError::AuthorizationFailed(format!(
            "token endpoint returned {status}: {body}"
        )));
    }
    parse_token_response(&body)
}

/// Exchange a refresh token for a fresh access token at `token_url`, under
/// `policy`. A response without a new refresh token keeps the old one, as
/// RFC 6749 §6 allows.
pub async fn refresh_token(
    token_url: &str,
    policy: &AddressPolicy,
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<OAuthToken, OAuthError> {
    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    let mut token = token_request(token_url, policy, &params).await?;
    if token.refresh_token.is_none() {
        token.refresh_token = Some(refresh_token.to_string());
    }
    Ok(token)
}

/// Exchange an authorization code for an access token at `token_url`, under
/// `policy`.
pub async fn exchange_code(
    token_url: &str,
    policy: &AddressPolicy,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<OAuthToken, OAuthError> {
    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    token_request(token_url, policy, &params).await
}

/// Start a localhost HTTP listener and wait for the OAuth callback.
///
/// Returns the authorization code after verifying the `state` parameter.
/// The listener keeps accepting connections until a valid callback is
/// received, so spurious browser requests (pre-connects, favicons, etc.)
/// do not starve the real callback.
pub async fn start_callback_listener(
    port: u16,
    expected_state: String,
    shutdown: CancellationToken,
) -> Result<String, OAuthError> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Err(OAuthError::Shutdown),
            result = listener.accept() => {
                let (mut stream, _) = result?;

                let mut buf = [0u8; 4096];
                let n = tokio::select! {
                    _ = shutdown.cancelled() => return Err(OAuthError::Shutdown),
                    result = stream.read(&mut buf) => result?,
                };
                if n == 0 {
                    continue;
                }

                let request = String::from_utf8_lossy(&buf[..n]);
                let (code, error, state) = parse_callback_query(&request);

                let (status, html, done) = if let Some(ref err) = error {
                    ("400 Bad Request", format!("<html><body><h1>Authorization failed</h1><p>{err}</p></body></html>"), Some(Err(OAuthError::AuthorizationFailed(err.clone()))))
                } else if state.is_none() {
                    // No state at all (favicon, pre-connect, etc.) — keep listening.
                    ("400 Bad Request", "<html><body><h1>Authorization failed</h1><p>Missing state parameter.</p></body></html>".to_string(), None)
                } else if state.as_deref() != Some(expected_state.as_str()) {
                    ("400 Bad Request", "<html><body><h1>Authorization failed</h1><p>Invalid state parameter.</p></body></html>".to_string(), Some(Err(OAuthError::StateMismatch)))
                } else if let Some(c) = code {
                    ("200 OK", "<html><body><h1>Authorization successful</h1><p>You can close this tab.</p></body></html>".to_string(), Some(Ok(c)))
                } else {
                    ("400 Bad Request", "<html><body><h1>Authorization failed</h1><p>No authorization code received.</p></body></html>".to_string(), None)
                };

                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                    html.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;

                if let Some(result) = done {
                    return result;
                }
            }
        }
    }
}

fn parse_callback_query(request: &str) -> (Option<String>, Option<String>, Option<String>) {
    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    let path = parts.get(1).copied().unwrap_or("");
    let query = path.split('?').nth(1).unwrap_or("");

    let mut code = None;
    let mut error = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            let value = percent_decode(value);
            match key {
                "code" => code = Some(value),
                "error" => error = Some(value),
                "state" => state = Some(value),
                _ => {}
            }
        }
    }
    (code, error, state)
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(s.as_bytes())
        .map(|(k, _)| k.into_owned())
        .collect()
}

/// Persistent store for per-server OAuth tokens.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    /// Return the default store path for a server, if the platform config dir
    /// is available.
    pub fn for_server(name: &str) -> Option<Self> {
        let base = agent_core::user_dirs::config_dir()?
            .join("rusta")
            .join("credentials")
            .join("mcp");
        Some(Self {
            path: base.join(format!("{name}.json")),
        })
    }

    /// Create a store at an explicit path (useful in tests).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Persist a token.
    ///
    /// On Unix the file is created with mode `0o600`; on other platforms it
    /// falls back to the default permissions.
    pub fn save(&self, token: &OAuthToken) -> Result<(), OAuthError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(token)?;

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)?;
            file.write_all(json.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&self.path, json)?;
        }
        Ok(())
    }

    /// Load a token if one exists.
    pub fn load(&self) -> Result<Option<OAuthToken>, OAuthError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&self.path)?;
        Ok(Some(serde_json::from_str(&content)?))
    }

    /// Delete any persisted token.
    pub fn clear(&self) -> Result<(), OAuthError> {
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

/// Convenience: generate a random CSRF state string.
pub fn random_state() -> String {
    let mut bytes = [0u8; 32];
    fastrand::Rng::new().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Convenience: build a redirect URI for a local port.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_verifier_and_challenge() {
        let pair = PkcePair::new();
        assert!(pair.verifier.len() >= 43);
        assert_eq!(pair.method, "S256");
        assert_eq!(pair.challenge, s256_challenge(&pair.verifier));
    }

    #[test]
    fn test_build_authorization_url_contains_params() {
        let url = build_authorization_url(
            "https://auth.example.com/authorize",
            "client-id",
            "http://127.0.0.1:8765",
            "challenge",
            "state",
            &["read".to_string(), "write".to_string()],
        )
        .unwrap();
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-id"));
        assert!(url.contains("code_challenge=challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state"));
        assert!(
            url.contains("scope=read+write"),
            "scopes should be space-separated in the query: {url}"
        );
    }

    #[test]
    fn test_parse_callback_query() {
        let request = "GET /?code=abc&state=xyz HTTP/1.1\r\nHost: localhost\r\n";
        let (code, error, state) = parse_callback_query(request);
        assert_eq!(code.as_deref(), Some("abc"));
        assert_eq!(state.as_deref(), Some("xyz"));
        assert!(error.is_none());
    }

    #[test]
    fn test_parse_callback_query_error() {
        let request = "GET /?error=access_denied&state=xyz HTTP/1.1\r\n";
        let (code, error, state) = parse_callback_query(request);
        assert!(code.is_none());
        assert_eq!(error.as_deref(), Some("access_denied"));
        assert_eq!(state.as_deref(), Some("xyz"));
    }

    #[tokio::test]
    async fn test_callback_listener_success_after_spurious_connection() {
        let port = 0; // let OS assign
        let state = "expected".to_string();
        let shutdown = CancellationToken::new();

        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let handle = tokio::spawn(start_callback_listener(
            port,
            state.clone(),
            shutdown.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Spurious connection without a valid callback.
        let mut junk = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        junk.write_all(
            b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        junk.flush().await.unwrap();
        let _ = junk.readable().await;

        // Real callback.
        let request = format!(
            "GET /?code=thecode&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let code = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(code.unwrap(), "thecode");
    }

    #[tokio::test]
    async fn test_callback_listener_state_mismatch() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(start_callback_listener(
            port,
            "expected".to_string(),
            shutdown.clone(),
        ));

        let request = "GET /?code=thecode&state=wrong HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(OAuthError::StateMismatch)));
    }

    mod discovery {
        use super::super::*;
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn metadata(server: &MockServer, scopes: &[&str]) -> serde_json::Value {
            serde_json::json!({
                "issuer": format!("{}/issuer", server.uri()),
                "authorization_endpoint": format!("{}/authorize", server.uri()),
                "token_endpoint": format!("{}/token", server.uri()),
                "scopes_supported": scopes,
            })
        }

        /// The protected resource names its authorization server, whose
        /// path-aware RFC 8414 metadata gives the endpoints.
        #[tokio::test]
        async fn discovery_follows_protected_resource_metadata() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/.well-known/oauth-protected-resource/mcp"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "resource": format!("{}/mcp", server.uri()),
                    "authorization_servers": [format!("{}/issuer", server.uri())],
                    "scopes_supported": ["mcp:tools"],
                })))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/.well-known/oauth-authorization-server/issuer"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(metadata(&server, &["mcp:tools", "offline_access"])),
                )
                .mount(&server)
                .await;

            let found = discover(&format!("{}/mcp", server.uri())).await.unwrap();
            assert_eq!(
                found.metadata.token_endpoint,
                format!("{}/token", server.uri())
            );
            assert_eq!(found.resource_scopes, Some(vec!["mcp:tools".to_string()]));
            // Nothing configured: the resource's scopes, plus offline_access.
            assert_eq!(
                request_scopes(&[], Some(&found)),
                ["mcp:tools", OFFLINE_ACCESS_SCOPE]
            );
        }

        /// Without resource metadata the server is its own authorization
        /// server, found through OpenID discovery if RFC 8414 is absent.
        #[tokio::test]
        async fn discovery_falls_back_to_the_origin_and_openid() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/.well-known/openid-configuration"))
                .respond_with(ResponseTemplate::new(200).set_body_json(metadata(&server, &[])))
                .mount(&server)
                .await;
            let found = discover(&format!("{}/mcp", server.uri())).await.unwrap();
            assert_eq!(
                found.metadata.authorization_endpoint,
                format!("{}/authorize", server.uri())
            );
            assert!(found.resource_scopes.is_none());

            let bare = MockServer::start().await;
            let err = discover(&bare.uri()).await.unwrap_err();
            assert!(matches!(err, OAuthError::Discovery(_)), "{err}");
        }

        #[test]
        fn offline_access_is_added_only_when_offered() {
            let offered = Discovery {
                metadata: AuthServerMetadata {
                    issuer: None,
                    authorization_endpoint: "a".into(),
                    token_endpoint: "t".into(),
                    scopes_supported: Some(vec!["read".into(), "offline_access".into()]),
                },
                resource_scopes: None,
                policy: AddressPolicy::unrestricted(),
            };
            let configured = vec!["read".to_string()];
            assert_eq!(
                request_scopes(&configured, Some(&offered)),
                ["read", "offline_access"]
            );
            let already = vec!["read".to_string(), "offline_access".to_string()];
            assert_eq!(request_scopes(&already, Some(&offered)), already);
            let mut plain = offered.clone();
            plain.metadata.scopes_supported = Some(vec!["read".into()]);
            assert_eq!(request_scopes(&configured, Some(&plain)), configured);
            assert_eq!(request_scopes(&configured, None), configured);
        }

        /// A refresh sends the refresh-token grant, and keeps the old refresh
        /// token when the response carries no new one.
        #[tokio::test]
        async fn refresh_keeps_the_old_refresh_token() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .and(body_string_contains("grant_type=refresh_token"))
                .and(body_string_contains("refresh_token=r-1"))
                .and(body_string_contains("client_id=cli"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "fresh", "token_type": "Bearer", "expires_in": 3600
                })))
                .mount(&server)
                .await;
            let token = refresh_token(
                &format!("{}/token", server.uri()),
                &AddressPolicy::unrestricted(),
                "r-1",
                "cli",
                None,
            )
            .await
            .unwrap();
            assert_eq!(token.access_token, "fresh");
            assert_eq!(token.refresh_token.as_deref(), Some("r-1"));
            assert!(!token.expires_within(Duration::from_secs(120)));
            assert!(token.expires_within(Duration::from_secs(7_200)));
        }

        /// Protected-resource metadata that names `issuer` as the authorization
        /// server.
        async fn naming_issuer(server: &MockServer, issuer: &str) {
            Mock::given(method("GET"))
                .and(path("/.well-known/oauth-protected-resource/mcp"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "authorization_servers": [issuer],
                })))
                .mount(server)
                .await;
        }

        /// A server on this machine may name an authorization server elsewhere
        /// on this machine, but none on the local link or a private network:
        /// the request is refused before any connection is made.
        #[tokio::test]
        async fn the_metadata_cannot_send_discovery_into_another_network() {
            let issuer = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/.well-known/oauth-authorization-server"))
                .respond_with(ResponseTemplate::new(200).set_body_json(metadata(&issuer, &[])))
                .mount(&issuer)
                .await;
            let server = MockServer::start().await;
            naming_issuer(&server, &issuer.uri()).await;
            let found = discover(&format!("{}/mcp", server.uri())).await.unwrap();
            assert_eq!(
                found.metadata.token_endpoint,
                format!("{}/token", issuer.uri())
            );

            for (internal, class) in [
                ("http://169.254.169.254", "link-local"),
                ("http://10.255.255.1", "private"),
            ] {
                let server = MockServer::start().await;
                naming_issuer(&server, internal).await;
                let err = discover(&format!("{}/mcp", server.uri()))
                    .await
                    .unwrap_err();
                assert!(matches!(err, OAuthError::Refused(_)), "{err}");
                assert!(
                    err.to_string().contains(&format!("the {class} address")),
                    "{err}"
                );
            }
        }

        /// A redirect from the server's own origin is checked like any other
        /// origin the server names.
        #[tokio::test]
        async fn a_redirect_cannot_send_discovery_into_another_network() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/.well-known/oauth-protected-resource/mcp"))
                .respond_with(
                    ResponseTemplate::new(302)
                        .insert_header("location", "http://169.254.169.254/latest/meta-data/"),
                )
                .mount(&server)
                .await;
            let err = discover(&format!("{}/mcp", server.uri()))
                .await
                .unwrap_err();
            assert!(matches!(err, OAuthError::Refused(_)), "{err}");
        }

        /// Metadata larger than the limit is ignored, as if absent.
        #[tokio::test]
        async fn oversized_metadata_is_not_read() {
            let server = MockServer::start().await;
            let mut huge = serde_json::to_vec(&serde_json::json!({
                "authorization_servers": ["http://169.254.169.254"],
            }))
            .unwrap();
            huge.resize(MAX_RESPONSE_BYTES + 1, b' ');
            Mock::given(method("GET"))
                .and(path("/.well-known/oauth-protected-resource/mcp"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(huge, "application/json"))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/.well-known/oauth-authorization-server"))
                .respond_with(ResponseTemplate::new(200).set_body_json(metadata(&server, &[])))
                .mount(&server)
                .await;
            let found = discover(&format!("{}/mcp", server.uri())).await.unwrap();
            assert!(found.resource_scopes.is_none());
            assert_eq!(
                found.metadata.token_endpoint,
                format!("{}/token", server.uri())
            );
        }

        /// A discovered token endpoint in another network is refused; an
        /// oversized token response is an error, not a token.
        #[tokio::test]
        async fn token_requests_keep_to_the_policy_and_the_limit() {
            let server = MockServer::start().await;
            let policy = AddressPolicy::for_server(&url::Url::parse(&server.uri()).unwrap()).await;
            assert!(policy.allows(AddressClass::Loopback));
            assert!(!policy.allows(AddressClass::Private));
            assert!(!policy.allows(AddressClass::LinkLocal));

            let err = refresh_token("http://169.254.169.254/token", &policy, "r-1", "cli", None)
                .await
                .unwrap_err();
            assert!(matches!(err, OAuthError::Refused(_)), "{err}");

            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(vec![b' '; MAX_RESPONSE_BYTES + 1], "application/json"),
                )
                .mount(&server)
                .await;
            let err = refresh_token(
                &format!("{}/token", server.uri()),
                &policy,
                "r-1",
                "cli",
                None,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, OAuthError::TooLarge(_)), "{err}");
        }
    }

    #[test]
    fn test_credential_store_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(temp.path().join("token.json"));
        assert!(store.load().unwrap().is_none());

        let token = OAuthToken {
            access_token: "secret".to_string(),
            token_type: Some("Bearer".to_string()),
            refresh_token: Some("refresh".to_string()),
            expires_at: Some(chrono::Utc::now()),
        };
        store.save(&token).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.access_token, "secret");
        assert_eq!(loaded.token_type.as_deref(), Some("Bearer"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credential file should be owner-only");
        }
    }
}
