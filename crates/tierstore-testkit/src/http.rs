//! A reqwest client for a test's own plain-HTTP requests.
//!
//! The workspace builds reqwest with rustls and no crypto provider of reqwest's own: `objstore-s3`
//! hands its client a rustls configuration on ring. A client built on reqwest's defaults would look
//! for a process-wide provider, find none and panic, so a test builds its client here: it is given
//! a rustls configuration on ring that trusts no certificate, for `http://` requests, and nothing
//! process-wide is set.

use std::sync::Arc;

/// A client builder with its own TLS configuration (module docs).
///
/// # Panics
/// Never: ring supports the default protocol versions.
#[expect(
    clippy::expect_used,
    reason = "test support: ring always supports rustls's default protocol versions, as documented under Panics"
)]
pub fn builder() -> reqwest::ClientBuilder {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    reqwest::Client::builder().tls_backend_preconfigured(tls)
}

/// A client with reqwest's defaults but its TLS configuration (module docs).
///
/// # Panics
/// Never: a client with a rustls configuration and no other setting builds.
#[must_use]
#[expect(
    clippy::expect_used,
    reason = "test support: the client has nothing that can fail to build, as documented under Panics"
)]
pub fn client() -> reqwest::Client {
    builder()
        .build()
        .expect("a client with a rustls configuration")
}
