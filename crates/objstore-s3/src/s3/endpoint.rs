//! Where a request goes: the endpoints of a store (`http://host[:port]` or `https://host[:port]`),
//! how a request names the bucket ([`Addressing`]: in the path, or in the host name), and the TLS
//! client of an `https` endpoint.
//!
//! TLS is rustls on ring, the provider of the rest of the workspace. The server's certificate is
//! verified, always: against the system's CA certificates, and those of [`S3Config::ca_pem`]
//! besides, for a store whose certificate a private CA issued. Nothing turns the check off.
//! The system's certificates are read when the store is made, and only if an endpoint is `https`:
//! a store of `http` endpoints alone builds on a host that has none.
//!
//! [`S3Config::ca_pem`]: super::S3Config::ca_pem

use std::sync::Arc;

use reqwest::Url;
use rustls::ClientConfig;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use super::S3ConfigError;

/// How a request names the bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Addressing {
    /// Virtual-hosted style for an AWS S3 endpoint, a host name that ends in `.amazonaws.com` or
    /// `.amazonaws.com.cn` (`https://s3.<region>.amazonaws.com`), when the bucket's name has no
    /// dot (`a.b.s3.<region>.amazonaws.com` is not covered by the certificate AWS serves for
    /// `*.s3.<region>.amazonaws.com`); path style for any other endpoint or bucket.
    #[default]
    Auto,
    /// The bucket is the first segment of the path: `<endpoint>/<bucket>/<key>`. What MinIO,
    /// Ceph RGW and RustFS serve, and AWS S3 still does.
    Path,
    /// The bucket is the first label of the host name: `<scheme>://<bucket>.<host>[:port]/<key>`.
    /// The endpoint's host must be a name, not an IP address.
    VirtualHosted,
}

/// One endpoint of the store, with the bucket in it as the store's [`Addressing`] puts it.
pub(super) struct Target {
    /// The endpoint as the store was given it, for the operator's eyes ([`super::EndpointState`]).
    pub(super) endpoint: String,
    /// The scheme, host (with the bucket for virtual-hosted style) and port requests go to.
    pub(super) base: Url,
    /// `host[:port]` as the `Host` header carries it and the signature covers it.
    pub(super) host: String,
    /// What the path of every request starts with: `/<bucket>` in path style, nothing in
    /// virtual-hosted style.
    pub(super) bucket_path: String,
}

impl Target {
    /// Whether requests to this endpoint go over TLS.
    pub(super) fn is_tls(&self) -> bool {
        self.base.scheme() == "https"
    }
}

/// The endpoint `endpoint` names, if it is `http://host[:port]` or `https://host[:port]`, for
/// `bucket` named as `addressing` says.
pub(super) fn target_of(
    endpoint: &str,
    bucket: &str,
    addressing: Addressing,
) -> Result<Target, S3ConfigError> {
    let bad = |why| S3ConfigError::Endpoint {
        endpoint: endpoint.to_owned(),
        why,
    };
    let not_url = |_| bad("not a URL");
    let mut base = Url::parse(endpoint).map_err(not_url)?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err(bad("the scheme must be http or https"));
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(bad("credentials belong in `credentials`, not the URL"));
    }
    if base.path() != "/" || base.query().is_some() || base.fragment().is_some() {
        return Err(bad("an endpoint has no path, query or fragment"));
    }
    // An `http` or `https` URL always has a host: a name, or an IP address (no domain).
    let hosted = match (addressing, base.domain()) {
        (Addressing::Path, _) => false,
        (Addressing::Auto, Some(name)) => is_aws(name) && !bucket.contains('.'),
        (Addressing::Auto, None) => false,
        (Addressing::VirtualHosted, Some(_)) => true,
        (Addressing::VirtualHosted, None) => {
            return Err(bad(
                "virtual-hosted-style addressing needs a host name, not an IP address",
            ));
        }
    };
    let bucket_path = if hosted {
        // A bucket name is lowercase letters, digits, dots and hyphens, as a host name is, so the
        // bucket before the endpoint's host parses as the endpoint did.
        let (scheme, authority) = (base.scheme(), base.authority());
        base = Url::parse(&format!("{scheme}://{bucket}.{authority}/")).map_err(not_url)?;
        String::new()
    } else {
        format!("/{bucket}")
    };
    // The userinfo was refused above, so the authority is the host with its port, if it is not
    // the scheme's default.
    let host = base.authority().to_owned();
    Ok(Target {
        endpoint: endpoint.to_owned(),
        base,
        host,
        bucket_path,
    })
}

/// The system's CA certificates, those that can be read; none on a host that has none.
pub(super) fn system_roots() -> Vec<CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

/// The TLS client of the store: it trusts the certificates of `ca_pem` and those of `system`
/// (the system's, for a store with an `https` endpoint; none for a store of `http` alone).
pub(super) fn tls_config(
    ca_pem: Option<&[u8]>,
    system: Vec<CertificateDer<'static>>,
) -> Result<ClientConfig, S3ConfigError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(system);
    if let Some(pem) = ca_pem {
        let certs = CertificateDer::pem_slice_iter(pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(S3ConfigError::CaPem)?;
        if certs.is_empty() {
            return Err(S3ConfigError::CaPemEmpty);
        }
        for cert in certs {
            roots.add(cert).map_err(S3ConfigError::CaCertificate)?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(S3ConfigError::Tls)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // The client speaks HTTP/1.1 only.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Whether `name` is a host name of AWS S3, which serves virtual-hosted style for every bucket
/// whose name has no dot. A host name in a URL is in lower case.
fn is_aws(name: &str) -> bool {
    name.ends_with(".amazonaws.com") || name.ends_with(".amazonaws.com.cn")
}

#[cfg(test)]
mod tests;
