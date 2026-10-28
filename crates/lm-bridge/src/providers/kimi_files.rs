//! Kimi's Files API: media uploaded once, then referenced as `ms://<id>`.
//!
//! Kimi Code takes images and video by reference. `POST {base}/files` with
//! the file and a `purpose` of `image` or `video` returns an id, and
//! `ms://<id>` in an `image_url` / `video_url` part points the model at it.
//! Requests stay small, and a file is uploaded once however many requests
//! carry it.
//!
//! Uploads are remembered by content, account and host — in memory, and on
//! disk when a cache directory is given, so a resumed session reuses them.
//! Kimi keeps no listing to check an id against (`GET /files/<id>` is not
//! served), so a remembered id can outlive the file: a request naming it gets
//! `404 resource_not_found_error`, and the caller then [`forget`]s the ids it
//! used and uploads again.
//!
//! [`forget`]: KimiFiles::forget

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine;
use reqwest::Client;
use sha2::{Digest, Sha256};

use crate::errors::LlmError;

use super::openai_common::classify_api_error;

/// What an upload is for, as Kimi's `purpose` field spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Image,
    Video,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
        }
    }
}

/// A file id and whether it came from an earlier upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub id: String,
    /// The id was remembered rather than returned by an upload just now.
    pub remembered: bool,
}

/// Client for Kimi's Files API.
pub struct KimiFiles {
    client: Client,
    base_url: String,
    api_key: String,
    headers: reqwest::header::HeaderMap,
    /// Separates accounts' uploads: a hash of the key, never the key.
    account: String,
    /// Cache key → file id.
    remembered: Mutex<HashMap<String, String>>,
    cache_dir: Option<PathBuf>,
}

impl KimiFiles {
    /// Send `headers` with uploads too, below the ones already set.
    #[must_use]
    pub fn with_custom_headers(mut self, headers: &reqwest::header::HeaderMap) -> Self {
        crate::custom_headers::add_below(&mut self.headers, headers);
        self
    }

    /// A client uploading to `{base_url}/files` with `api_key`, sending
    /// `headers` (custom headers, never a JSON content type) with each upload.
    #[must_use]
    pub fn new(
        client: Client,
        base_url: &str,
        api_key: &str,
        headers: reqwest::header::HeaderMap,
    ) -> Self {
        let account = hex(&Sha256::digest(api_key.as_bytes()))[..16].to_string();
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            headers,
            account,
            remembered: Mutex::new(HashMap::new()),
            cache_dir: None,
        }
    }

    /// Remember uploads on disk under `dir` as well as in memory.
    #[must_use]
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    /// Where uploads are remembered on disk, if anywhere.
    #[must_use]
    pub fn cache_dir(&self) -> Option<&std::path::Path> {
        self.cache_dir.as_deref()
    }

    /// The id of the file whose base64 payload is `base64`, uploading it
    /// unless an earlier upload is remembered.
    pub async fn file_id(
        &self,
        base64: &str,
        mime: &str,
        purpose: Purpose,
    ) -> Result<FileRef, LlmError> {
        let key = self.cache_key(base64, purpose);
        if let Some(id) = self.recall(&key) {
            return Ok(FileRef {
                id,
                remembered: true,
            });
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64.trim())
            .map_err(|e| LlmError::Provider(format!("media is not valid base64: {e}")))?;
        let id = self.upload(bytes, mime, purpose).await?;
        self.remember(&key, &id);
        Ok(FileRef {
            id,
            remembered: false,
        })
    }

    /// Forget every remembered upload whose id is in `ids`, in memory and on
    /// disk, so the next request uploads those files again.
    pub fn forget(&self, ids: &[String]) {
        let stale: Vec<String> = {
            let mut remembered = self.remembered.lock().unwrap();
            let stale: Vec<String> = remembered
                .iter()
                .filter(|(_, id)| ids.contains(id))
                .map(|(key, _)| key.clone())
                .collect();
            for key in &stale {
                remembered.remove(key);
            }
            stale
        };
        if let Some(dir) = &self.cache_dir {
            for key in stale {
                let _ = std::fs::remove_file(dir.join(key));
            }
        }
    }

    async fn upload(
        &self,
        bytes: Vec<u8>,
        mime: &str,
        purpose: Purpose,
    ) -> Result<String, LlmError> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(format!("upload.{}", crate::media::extension_for_mime(mime)))
            .mime_str(mime)
            .map_err(|e| LlmError::Provider(format!("bad media type {mime}: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("purpose", purpose.as_str())
            .part("file", part);
        let response = self
            .client
            .post(format!("{}/files", self.base_url))
            .headers(self.headers.clone())
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(classify_api_error(status, &body, false));
        }
        let id = serde_json::from_str::<serde_json::Value>(&body)?
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|id| is_valid_id(id))
            .ok_or_else(|| {
                LlmError::Provider(format!("Kimi's file upload returned no usable id: {body}"))
            })?;
        Ok(id)
    }

    fn cache_key(&self, base64: &str, purpose: Purpose) -> String {
        let mut hasher = Sha256::new();
        for field in [purpose.as_str(), &self.base_url, &self.account] {
            hasher.update(field.as_bytes());
            hasher.update([0]);
        }
        hasher.update(base64.trim().as_bytes());
        hex(&hasher.finalize())
    }

    fn recall(&self, key: &str) -> Option<String> {
        if let Some(id) = self.remembered.lock().unwrap().get(key) {
            return Some(id.clone());
        }
        let dir = self.cache_dir.as_ref()?;
        let id = std::fs::read_to_string(dir.join(key)).ok()?;
        let id = id.trim().to_string();
        if !is_valid_id(&id) {
            return None;
        }
        self.remembered
            .lock()
            .unwrap()
            .insert(key.to_string(), id.clone());
        Some(id)
    }

    fn remember(&self, key: &str, id: &str) {
        self.remembered
            .lock()
            .unwrap()
            .insert(key.to_string(), id.to_string());
        if let Some(dir) = &self.cache_dir {
            // A failed write costs a re-upload next session, nothing more.
            if std::fs::create_dir_all(dir).is_ok() {
                let _ = std::fs::write(dir.join(key), id);
            }
        }
    }
}

/// Whether `error` says a referenced file does not exist — a remembered
/// upload Kimi no longer has.
#[must_use]
pub fn is_missing_file(error: &LlmError) -> bool {
    matches!(
        error,
        LlmError::ApiStatus { status, message, .. }
            if *status == reqwest::StatusCode::NOT_FOUND && message.contains("resource_not_found")
    )
}

/// Whether `error` is an authentication or permission failure, which an
/// inline fallback would only hit again on the chat request.
#[must_use]
pub fn is_auth_error(error: &LlmError) -> bool {
    matches!(
        error,
        LlmError::ApiStatus { status, .. }
            if *status == reqwest::StatusCode::UNAUTHORIZED
                || *status == reqwest::StatusCode::FORBIDDEN
    )
}

/// Ids are opaque, but they end up in a file name and a URL.
fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn files(server: &MockServer) -> KimiFiles {
        KimiFiles::new(
            crate::http::client(),
            &server.uri(),
            "sk-test",
            reqwest::header::HeaderMap::new(),
        )
    }

    async fn upload_answering(server: &MockServer, id: &str) {
        Mock::given(method("POST"))
            .and(path("/files"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id, "object": "file", "status": "ready"
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn an_upload_is_made_once_and_remembered() {
        let server = MockServer::start().await;
        upload_answering(&server, "file-1").await;
        let files = files(&server);
        let first = files
            .file_id("iVBORw0KGgo=", "image/png", Purpose::Image)
            .await
            .unwrap();
        assert_eq!(
            first,
            FileRef {
                id: "file-1".into(),
                remembered: false
            }
        );
        let second = files
            .file_id("iVBORw0KGgo=", "image/png", Purpose::Image)
            .await
            .unwrap();
        assert_eq!(
            second,
            FileRef {
                id: "file-1".into(),
                remembered: true
            }
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        // A multipart form with the purpose and the decoded file.
        let body = &requests[0].body;
        let contains = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
        assert!(contains(b"name=\"purpose\""));
        assert!(contains(b"\r\n\r\nimage\r\n"));
        assert!(contains(b"filename=\"upload.png\""));
        assert!(contains(b"\x89PNG"));
    }

    /// The disk cache survives the client, and `forget` clears both layers.
    #[tokio::test]
    async fn uploads_are_remembered_on_disk_until_forgotten() {
        let server = MockServer::start().await;
        upload_answering(&server, "file-2").await;
        let dir = tempfile::tempdir().unwrap();
        files(&server)
            .with_cache_dir(dir.path())
            .file_id("AAAA", "video/mp4", Purpose::Video)
            .await
            .unwrap();

        let later = files(&server).with_cache_dir(dir.path());
        let recalled = later
            .file_id("AAAA", "video/mp4", Purpose::Video)
            .await
            .unwrap();
        assert!(recalled.remembered);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        later.forget(&["file-2".to_string()]);
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        let again = later
            .file_id("AAAA", "video/mp4", Purpose::Video)
            .await
            .unwrap();
        assert!(!again.remembered);
    }

    /// Different accounts never share an upload.
    #[test]
    fn cache_keys_separate_accounts_purposes_and_hosts() {
        let a = KimiFiles::new(
            crate::http::client(),
            "https://a/v1",
            "key-1",
            Default::default(),
        );
        let b = KimiFiles::new(
            crate::http::client(),
            "https://a/v1",
            "key-2",
            Default::default(),
        );
        let c = KimiFiles::new(
            crate::http::client(),
            "https://c/v1",
            "key-1",
            Default::default(),
        );
        let key = a.cache_key("AAAA", Purpose::Image);
        assert_ne!(key, b.cache_key("AAAA", Purpose::Image));
        assert_ne!(key, c.cache_key("AAAA", Purpose::Image));
        assert_ne!(key, a.cache_key("AAAA", Purpose::Video));
        assert!(!key.contains("key-1"));
    }

    #[tokio::test]
    async fn upload_failures_are_classified() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/files"))
            .respond_with(ResponseTemplate::new(401).set_body_string(
                r#"{"error":{"message":"Invalid Authentication","type":"invalid_authentication_error"}}"#,
            ))
            .mount(&server)
            .await;
        let err = files(&server)
            .file_id("AAAA", "image/png", Purpose::Image)
            .await
            .unwrap_err();
        assert!(is_auth_error(&err), "{err}");
    }

    #[test]
    fn missing_files_are_recognized() {
        let missing = LlmError::ApiStatus {
            status: reqwest::StatusCode::NOT_FOUND,
            message: r#"{"error":{"message":"The requested resource was not found","type":"resource_not_found_error"}}"#.into(),
            retry_hint: false,
            retry_after: None,
        };
        assert!(is_missing_file(&missing));
        assert!(!is_valid_id("../etc"));
        assert!(is_valid_id("fctrba3eudyi11eayy41"));
    }
}
