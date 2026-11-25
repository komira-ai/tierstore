//! The S3 client over TLS, against the loopback service serving HTTPS with a certificate of a CA
//! made for the test: a round trip in path style and in virtual-hosted style, each request signed
//! as the service checks it; a certificate of a CA the client does not trust, or for another name,
//! refused before any request; and plain HTTP as before.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::error::Error;
use std::sync::Arc;

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::s3::{Addressing, S3Store};
use objstore_s3::{
    ByteRange, Capabilities, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError, PageSize,
};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::{CertificateError, ServerConfig};

/// The host name the virtual-hosted tests reach the service by; the store resolves it to the
/// service itself, so no lookup is made.
const NAME: &str = "s3.fake.test";

/// A CA made for one test.
struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

impl Ca {
    /// A CA named `name`: two CAs of one name would be told apart by their signatures alone.
    fn new(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        Self { issuer }
    }

    /// Its certificate, PEM: what a client is told to trust.
    fn pem(&self) -> Vec<u8> {
        self.issuer.pem().into_bytes()
    }

    /// The TLS of a server whose certificate this CA issued for exactly `sans`.
    fn server(&self, sans: Vec<SanType>) -> Arc<ServerConfig> {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.subject_alt_names = sans;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        let chain = vec![CertificateDer::from(cert.der().to_vec())];
        let key = PrivatePkcs8KeyDer::from(key.serialize_der()).into();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap();
        Arc::new(config)
    }
}

fn loopback() -> Vec<SanType> {
    vec![SanType::IpAddress([127, 0, 0, 1].into())]
}

fn named() -> Vec<SanType> {
    let names = [NAME.to_owned(), format!("*.{NAME}")];
    names
        .into_iter()
        .map(|n| SanType::DnsName(n.try_into().unwrap()))
        .collect()
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

/// Writes, reads a range of, lists and deletes one object through `store`.
async fn round_trip(store: &S3Store) {
    let k = key("dir/obj");
    store
        .put_new(&k, Bytes::from_static(b"over tls"), None)
        .await
        .unwrap();
    let range = ByteRange::new(5, 3).unwrap();
    assert_eq!(store.get_range(&k, range).await.unwrap(), "tls");
    assert_eq!(store.len(&k).await.unwrap(), Some(8));
    let prefix = KeyPrefix::new("dir/").unwrap();
    let page = store
        .list(&prefix, None, PageSize::new(10).unwrap())
        .await
        .unwrap();
    let listed: Vec<String> = (page.objects.iter())
        .map(|o| o.key.as_str().to_owned())
        .collect();
    assert_eq!(listed, ["dir/obj"]);
    store.delete(&k).await.unwrap();
    assert_eq!(store.len(&k).await.unwrap(), None);
}

/// The request lines of the round trip, with `bucket` the start of every path.
fn round_trip_lines(bucket: &str) -> Vec<String> {
    let obj = format!("{bucket}/dir/obj");
    let root = if bucket.is_empty() { "/" } else { bucket };
    vec![
        format!("PUT {obj}?"),
        format!("GET {obj}?"),
        format!("HEAD {obj}?"),
        format!("GET {root}?list-type=2&max-keys=10&prefix=dir%2F"),
        format!("DELETE {obj}?"),
        format!("HEAD {obj}?"),
    ]
}

/// The TLS error in the causes of `e`, a transport failure.
fn tls_error(e: &ObjectStoreError) -> rustls::Error {
    let ObjectStoreError::Transport(cause) = e else {
        panic!("expected a transport failure, got {e:?}");
    };
    let mut next: Option<&(dyn Error + 'static)> = Some(cause.as_ref());
    while let Some(err) = next {
        if let Some(tls) = err.downcast_ref::<rustls::Error>() {
            return tls.clone();
        }
        // An `io::Error` gives the source of the error it wraps, not that error: step into it.
        next = match err.downcast_ref::<std::io::Error>() {
            Some(io) => io.get_ref().map(|inner| inner as &(dyn Error + 'static)),
            None => err.source(),
        };
    }
    panic!("no TLS error in {e:?}");
}

/// Asserts that a store of an `https` endpoint, told to trust the service's CA, writes, reads a
/// range of, measures, lists and deletes an object, path style, every request signed as the
/// service checks it.
#[tokio::test]
async fn an_https_endpoint_round_trips_with_the_ca_it_is_given() {
    let ca = Ca::new("test CA");
    let fake = FakeS3::start_tls(ca.server(loopback())).await;
    assert!(fake.endpoint().starts_with("https://127.0.0.1:"));
    fake.require_credentials("fake-key", "fake-secret");
    let mut config = fake.config(Capabilities::default());
    config.ca_pem = Some(ca.pem());
    let store = S3Store::new(config).unwrap();
    round_trip(&store).await;
    assert_eq!(fake.requests(), round_trip_lines("/tierstore-fake"));
}

/// Asserts that a certificate the client cannot trust is refused in the handshake and no request
/// reaches the service: one of a CA that is not the configured one (the system's CA certificates do
/// not include a CA made for the test either), the same with no configured CA at all, and one of
/// the trusted CA for another name than the endpoint's.
#[tokio::test]
async fn a_certificate_the_client_cannot_trust_is_refused_before_any_request() {
    let ca = Ca::new("test CA");
    let fake = FakeS3::start_tls(ca.server(loopback())).await;
    let stranger = Ca::new("another test CA");
    let mut other_ca = fake.config(Capabilities::default());
    other_ca.ca_pem = Some(stranger.pem());
    let mut no_ca = fake.config(Capabilities::default());
    no_ca.ca_pem = None;
    for config in [other_ca, no_ca] {
        let store = S3Store::new(config).unwrap();
        let e = store
            .put_new(&key("k"), Bytes::from_static(b"x"), None)
            .await
            .unwrap_err();
        assert_eq!(
            tls_error(&e),
            rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer)
        );
    }

    let renamed = FakeS3::start_tls(ca.server(named())).await;
    let mut config = renamed.config(Capabilities::default());
    config.ca_pem = Some(ca.pem());
    let e = S3Store::new(config)
        .unwrap()
        .put_new(&key("k"), Bytes::from_static(b"x"), None)
        .await
        .unwrap_err();
    let refused = tls_error(&e);
    assert!(
        matches!(
            refused,
            rustls::Error::InvalidCertificate(CertificateError::NotValidForNameContext { .. })
        ),
        "{refused:?}"
    );
    assert!(fake.requests().is_empty());
    assert!(renamed.requests().is_empty());
}

/// Asserts that in virtual-hosted style the store reaches the bucket at `<bucket>.<host>` over
/// TLS, whose wildcard certificate covers it, and every path is the key alone (`/` for the
/// listing), each request signed for that host and path as the service checks; and that path style
/// on the same host name puts the bucket back in the path.
#[tokio::test]
async fn virtual_hosted_style_reaches_the_bucket_by_its_host_name() {
    let ca = Ca::new("test CA");
    let fake = FakeS3::start_tls(ca.server(named())).await;
    fake.require_credentials("fake-key", "fake-secret");
    let port = fake.endpoint().rsplit_once(':').unwrap().1.to_owned();
    for (addressing, bucket) in [
        (Addressing::VirtualHosted, ""),
        (Addressing::Path, "/tierstore-fake"),
    ] {
        let mut config = fake.config(Capabilities::default());
        config.endpoint = format!("https://{NAME}:{port}");
        config.addressing = addressing;
        config.ca_pem = Some(ca.pem());
        let store = fake.store_resolving(config, NAME).unwrap();
        round_trip(&store).await;
        let lines = fake.requests();
        assert_eq!(lines[lines.len() - 6..], round_trip_lines(bucket));
    }
    assert_eq!(fake.regions(), ["us-east-1"]);
}

/// Asserts that a plain `http` endpoint round trips as before, signed as the service checks.
#[tokio::test]
async fn an_http_endpoint_still_round_trips() {
    let fake = FakeS3::start().await;
    assert!(fake.endpoint().starts_with("http://127.0.0.1:"));
    fake.require_credentials("fake-key", "fake-secret");
    let store = fake.store(Capabilities::default());
    round_trip(&store).await;
    assert_eq!(fake.requests(), round_trip_lines("/tierstore-fake"));
}
