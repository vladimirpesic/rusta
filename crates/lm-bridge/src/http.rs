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

use std::sync::{Arc, OnceLock};

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
