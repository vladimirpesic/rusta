//! OAuth 2 for MCP servers: authorization code with PKCE, discovery of the
//! authorization server, and refresh.
//!
//! Discovery follows the MCP authorization spec: the server's protected-resource
//! metadata (RFC 9728) names its authorization server, whose metadata (RFC 8414,
//! or OpenID discovery) gives the endpoints and supported scopes. When that
//! server supports `offline_access`, it is requested so the grant comes with
//! a refresh token, and tokens are refreshed before they expire and when a
//! server rejects them.

use std::{collections::HashMap, path::PathBuf, time::Duration};

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
}

/// The scope that asks for a refresh token.
pub const OFFLINE_ACCESS_SCOPE: &str = "offline_access";

/// How long before expiry a token is refreshed.
pub const REFRESH_AHEAD: Duration = Duration::from_secs(120);

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

/// GET `url` as JSON, `None` when it is missing or unreadable.
async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Option<T> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// Find the authorization server for the MCP server at `server_url`.
///
/// Its protected-resource metadata names the authorization server; a server
/// without that metadata is taken to be its own. The authorization server's
/// metadata comes from RFC 8414's well-known location, or OpenID discovery.
pub async fn discover(client: &reqwest::Client, server_url: &str) -> Result<Discovery, OAuthError> {
    let resource = url::Url::parse(server_url)?;
    let mut resource_meta = ProtectedResourceMetadata::default();
    for candidate in well_known_candidates(&resource, "oauth-protected-resource") {
        if let Some(meta) = fetch_json::<ProtectedResourceMetadata>(client, &candidate).await {
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
        if let Some(metadata) = fetch_json::<AuthServerMetadata>(client, &candidate).await {
            return Ok(Discovery {
                metadata,
                resource_scopes: resource_meta.scopes_supported,
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

/// POST `params` to the token endpoint and parse the token it returns.
async fn token_request(token_url: &str, params: &[(&str, &str)]) -> Result<OAuthToken, OAuthError> {
    let client = lm_bridge::http::client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client.post(token_url).form(params).send().await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(OAuthError::AuthorizationFailed(format!(
            "token endpoint returned {status}: {body}"
        )));
    }
    parse_token_response(&body)
}

/// Exchange a refresh token for a fresh access token. A response without a
/// new refresh token keeps the old one, as RFC 6749 §6 allows.
pub async fn refresh_token(
    token_url: &str,
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
    let mut token = token_request(token_url, &params).await?;
    if token.refresh_token.is_none() {
        token.refresh_token = Some(refresh_token.to_string());
    }
    Ok(token)
}

/// Exchange an authorization code for an access token.
pub async fn exchange_code(
    token_url: &str,
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
    token_request(token_url, &params).await
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

            let found = discover(&lm_bridge::http::client(), &format!("{}/mcp", server.uri()))
                .await
                .unwrap();
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
            let found = discover(&lm_bridge::http::client(), &format!("{}/mcp", server.uri()))
                .await
                .unwrap();
            assert_eq!(
                found.metadata.authorization_endpoint,
                format!("{}/authorize", server.uri())
            );
            assert!(found.resource_scopes.is_none());

            let bare = MockServer::start().await;
            let err = discover(&lm_bridge::http::client(), &bare.uri())
                .await
                .unwrap_err();
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
            let token = refresh_token(&format!("{}/token", server.uri()), "r-1", "cli", None)
                .await
                .unwrap();
            assert_eq!(token.access_token, "fresh");
            assert_eq!(token.refresh_token.as_deref(), Some("r-1"));
            assert!(!token.expires_within(Duration::from_secs(120)));
            assert!(token.expires_within(Duration::from_secs(7_200)));
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
