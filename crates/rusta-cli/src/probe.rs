//! Probing an OpenAI-compatible endpoint with a key: its `GET /models`, and
//! DeepSeek's `GET /user/balance`.
//!
//! Asking `base_url/models` with the key proves both in one request that
//! costs no tokens — chat is billed, model lists and balances are not. The
//! Kimi and DeepSeek checks share the HTTP and status classification here and
//! keep only their own wording for each outcome.

use reqwest::StatusCode;

/// What an authenticated `GET` answered, classified the way every caller
/// agrees on.
#[derive(Debug)]
pub enum ProbeAnswer {
    /// 2xx: the response body, unparsed.
    Accepted(String),
    /// 401/403: the key was refused.
    Rejected,
    /// Any other status, carried for the caller to word (a 402 here means a
    /// spent balance; a 5xx means the endpoint could not be asked properly).
    Other(StatusCode),
}

/// Ask `base_url` for its models with `key` (and the custom `headers`).
/// `Err` is returned when the endpoint could not be asked at all.
pub async fn probe_models(
    base_url: &str,
    key: &str,
    headers: &reqwest::header::HeaderMap,
) -> Result<ProbeAnswer, String> {
    probe(base_url, "models", key, headers).await
}

/// `GET base_url/path` with `key` (and the custom `headers`). `Err` is
/// returned when the endpoint could not be asked at all.
pub async fn probe(
    base_url: &str,
    path: &str,
    key: &str,
    headers: &reqwest::header::HeaderMap,
) -> Result<ProbeAnswer, String> {
    let client = lm_bridge::http::client_builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(format!("{}/{path}", base_url.trim_end_matches('/')))
        .headers(headers.clone())
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Ok(ProbeAnswer::Rejected);
    }
    if !status.is_success() {
        return Ok(ProbeAnswer::Other(status));
    }
    response
        .text()
        .await
        .map(ProbeAnswer::Accepted)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classification every caller relies on: accepted with the body,
    /// refused on 401/403, anything else carried as-is.
    #[tokio::test]
    async fn statuses_are_classified() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .mount(&server)
            .await;
        let none = reqwest::header::HeaderMap::new();
        let answer = probe_models(&server.uri(), "k", &none).await.unwrap();
        let ProbeAnswer::Accepted(body) = answer else {
            panic!("200 is accepted");
        };
        assert_eq!(body, r#"{"data":[]}"#);

        for status in [401, 402, 403, 500] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/models"))
                .respond_with(ResponseTemplate::new(status).set_body_string("{}"))
                .mount(&server)
                .await;
            let answer = probe_models(&server.uri(), "k", &none).await.unwrap();
            match status {
                401 | 403 => assert!(matches!(answer, ProbeAnswer::Rejected)),
                _ => assert!(matches!(answer, ProbeAnswer::Other(s) if s.as_u16() == status)),
            }
        }

        assert!(
            probe_models("http://127.0.0.1:9", "k", &none)
                .await
                .is_err()
        );
    }
}
