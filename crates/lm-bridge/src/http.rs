//! The one way Rusta builds an HTTP client.
//!
//! reqwest 0.13's `rustls` feature means aws-lc for the cryptography — a C
//! library, and a hazard for the glibc-2.17 zigbuild — and certificate checks
//! against the platform's trust store. Rusta keeps what it shipped with:
//! ring for the cryptography, and the Mozilla roots that `webpki-roots`
//! bundles, so a host without a CA bundle still verifies and nothing depends on
//! what the host has installed.
//!
//! Without the `rustls` feature reqwest has no crypto provider of its own and
//! panics when a client is built without one, so every client must come from
//! here. The configuration is handed to reqwest preconfigured, and reqwest then
//! leaves ALPN alone; it is set here as reqwest would set it, `h2` first.
//!
//! [`read_limited`] is the one way a body of unknown size is read: chunk by
//! chunk, stopping as soon as it would pass its limit.

use std::sync::{Arc, OnceLock};

/// Why [`read_limited`] read no body.
#[derive(Debug, thiserror::Error)]
pub enum ReadLimitedError {
    /// The body is larger than the limit: by its declared length, or by what
    /// arrived before the read stopped.
    #[error("the response is larger than {limit} bytes")]
    TooLarge { limit: usize },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

/// `response`'s body, as long as it is at most `limit` bytes.
///
/// A declared length past the limit is refused before anything is read; a
/// body without one, or that runs past what it declared, is read only up to
/// the limit. Memory never holds more than `limit` bytes of it.
pub async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ReadLimitedError> {
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(ReadLimitedError::TooLarge { limit });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Err(ReadLimitedError::TooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A client builder with Rusta's TLS configuration.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_backend_preconfigured(tls_config().clone())
}

/// A client with Rusta's TLS configuration and reqwest's defaults otherwise.
///
/// # Panics
///
/// Like `reqwest::Client::new`, if the client cannot be built, which with this
/// fixed configuration means a system resource (the DNS resolver) failed.
#[must_use]
pub fn client() -> reqwest::Client {
    client_builder()
        .build()
        .expect("an HTTP client with the bundled TLS configuration")
}

fn tls_config() -> &'static rustls::ClientConfig {
    static CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports TLS 1.2 and 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        config
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body without a declared length, or one longer than it declares,
    /// stops at the limit; one within it is read whole.
    #[tokio::test]
    async fn a_body_is_read_only_up_to_its_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for body in [
                // Chunked, no declared length: 64 KiB in 1 KiB chunks.
                "chunked", // Declares 10 bytes, within the limit.
                "small",   // Declares more than the limit.
                "declared",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await;
                match body {
                    "chunked" => {
                        socket
                            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
                            .await
                            .unwrap();
                        for _ in 0..64 {
                            let chunk = vec![b'x'; 1024];
                            let _ = socket.write_all(b"400\r\n").await;
                            let _ = socket.write_all(&chunk).await;
                            let _ = socket.write_all(b"\r\n").await;
                        }
                        let _ = socket.write_all(b"0\r\n\r\n").await;
                    }
                    "small" => {
                        socket
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\n0123456789")
                            .await
                            .unwrap();
                    }
                    _ => {
                        socket
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 999999\r\n\r\n")
                            .await
                            .unwrap();
                    }
                }
            }
        });
        let client = client();
        let get = || client.get(format!("http://{address}/")).send();

        let chunked = read_limited(get().await.unwrap(), 4096).await;
        assert!(matches!(
            chunked,
            Err(ReadLimitedError::TooLarge { limit: 4096 })
        ));
        let small = read_limited(get().await.unwrap(), 4096).await.unwrap();
        assert_eq!(small, b"0123456789");
        let declared = read_limited(get().await.unwrap(), 4096).await;
        assert!(matches!(declared, Err(ReadLimitedError::TooLarge { .. })));
    }

    #[test]
    fn a_client_builds_without_a_process_wide_crypto_provider() {
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
        let _client = client();
    }

    #[test]
    fn the_configuration_offers_http2_and_bundles_the_mozilla_roots() {
        let config = tls_config();
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
    }
}
