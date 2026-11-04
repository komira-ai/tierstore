//! The pull-through of the front, over an in-memory store and a registry in the process (see
//! `fake_upstream`): no network.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt as _;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, StatusCode};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tracing::Level;
use tracing::subscriber::{DefaultGuard, set_default};

use super::{MAX_MANIFEST_BYTES, OciFront, Upstream};
use crate::server::build_info::stderr_log::LineLog;
use crate::server::oci::fake_upstream::{Ending, Fake, Registry, Seen};
use crate::server::oci::{OciMetrics, OciPrefix, UpstreamSettings};
use crate::{Backend, Error, FaultyBackend, MemoryBackend, Op};

const MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#;
const OTHER_MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[],"annotations":{}}"#;
const OCI_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

fn hex_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_of(bytes))
}

fn blob_key(content: &[u8]) -> String {
    format!("oci/blobs/sha256/{}", hex_of(content))
}

/// A blob of `len` bytes that is not one repeated byte.
fn blob(len: u32) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i % 251).expect("a byte"))
        .collect()
}

/// A registry that has each of `entries` at its path and answers 404 for any other.
fn files(entries: Vec<(String, Fake)>) -> impl Fn(&Seen) -> Fake + Send + Sync + 'static {
    move |seen| {
        let found = entries.iter().find(|(path, _)| *path == seen.path);
        found.map_or_else(|| Fake::status(404), |(_, fake)| fake.clone())
    }
}

/// A registry that answers every request with `status`.
fn always(status: u16) -> impl Fn(&Seen) -> Fake + Send + Sync + 'static {
    move |_| Fake::status(status)
}

/// A subscriber that drops what it is told, so that the messages of the front are made, as they
/// are when the server runs, and are not shown.
fn quiet() -> DefaultGuard {
    set_default(LineLog::new(std::io::sink(), Level::INFO))
}

struct Rig {
    faulty: Arc<FaultyBackend>,
    memory: Arc<MemoryBackend>,
    front: OciFront,
    metrics: Arc<OciMetrics>,
    registry: Registry,
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> &str {
        let value = self
            .headers
            .get(name)
            .unwrap_or_else(|| panic!("no {name}"));
        value.to_str().expect("text")
    }

    fn document(&self) -> Value {
        serde_json::from_slice(&self.body).expect("an error document")
    }

    /// Asserts the answer is the error `code` with `status`.
    fn assert_error(&self, status: StatusCode, code: &str) {
        assert_eq!(
            self.status,
            status,
            "{}",
            String::from_utf8_lossy(&self.body)
        );
        assert_eq!(self.document()["errors"][0]["code"], code);
    }

    fn message(&self) -> String {
        self.document()["errors"][0]["message"]
            .as_str()
            .expect("a message")
            .to_owned()
    }
}

impl Rig {
    async fn new(handler: impl Fn(&Seen) -> Fake + Send + Sync + 'static) -> Self {
        let registry = Registry::start(handler).await;
        let settings = UpstreamSettings {
            url: registry.url(),
            login: None,
            ca_pem: None,
            timeout: Duration::from_secs(30),
            max_redirects: 4,
        };
        let upstream = Upstream::new(settings).expect("an upstream");
        let memory = Arc::new(MemoryBackend::new());
        let faulty = Arc::new(FaultyBackend::new(memory.clone()));
        let metrics = Arc::new(OciMetrics::default());
        let prefix = OciPrefix::new("oci/").expect("a prefix");
        let front = OciFront::new(faulty.clone(), prefix, Arc::clone(&metrics))
            .with_upstream(Some(upstream));
        Self {
            faulty,
            memory,
            front,
            metrics,
            registry,
        }
    }

    async fn ask(&self, method: Method, path: &str, headers: &[(&str, &str)]) -> Answer {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).expect("a name");
            map.insert(name, HeaderValue::from_str(value).expect("a value"));
        }
        let response = self.front.handle(&method, path, &map).await;
        let (parts, mut body) = response.into_parts();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            bytes.extend(
                frame
                    .expect("a body that is whole")
                    .into_data()
                    .expect("data"),
            );
        }
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: bytes,
        }
    }

    async fn get(&self, path: &str) -> Answer {
        self.ask(Method::GET, path, &[]).await
    }

    /// What the store holds at `key`, if anything.
    async fn stored(&self, key: &str) -> Option<Bytes> {
        self.memory.get(key, None).await.ok()
    }

    /// The keys of the tag versions the store holds.
    async fn tags_stored(&self) -> Vec<String> {
        let listed = self.memory.list("oci/tags/").await.expect("a listing");
        listed.into_iter().map(|object| object.key).collect()
    }

    async fn put_under(&self, key: &str, content: &[u8]) {
        let put = self.memory.put(key, Bytes::copy_from_slice(content)).await;
        put.expect("put");
    }
}

/// Asserts a manifest the store lacks is fetched from the upstream by its name and digest, with
/// the manifest media types in `Accept`, stored by its digest, served with its media type, and
/// then served from the store, with no second request upstream.
#[tokio::test]
async fn a_missing_manifest_by_digest_is_pulled_stored_and_then_served_from_the_store() {
    let digest = digest_of(MANIFEST.as_bytes());
    let path = format!("/v2/library/app/manifests/{digest}");
    let rig = Rig::new(files(vec![(path.clone(), Fake::ok(MANIFEST))])).await;
    let answer = rig.get(&path).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, MANIFEST.as_bytes());
    assert_eq!(answer.header("content-type"), OCI_MANIFEST_TYPE);
    assert_eq!(answer.header("docker-content-digest"), digest);
    let stored = rig.stored(&blob_key(MANIFEST.as_bytes())).await;
    assert_eq!(stored.as_deref(), Some(MANIFEST.as_bytes()));
    let again = rig.get(&path).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body, MANIFEST.as_bytes());
    let seen = rig.registry.seen();
    assert_eq!(seen.len(), 1);
    let accept = seen[0].accept.clone().expect("an Accept");
    assert!(accept.contains(OCI_MANIFEST_TYPE), "{accept}");
    assert!(
        accept.contains("application/vnd.oci.image.index.v1+json"),
        "{accept}"
    );
    assert!(accept.contains("application/vnd.docker.distribution.manifest.v2+json"));
}

/// Asserts a tag the store lacks is fetched from the upstream, that its manifest is stored by the
/// digest of its bytes and the tag points at that digest, and that the tag and the digest are
/// then served from the store.
#[tokio::test]
async fn a_missing_tag_is_pulled_and_points_at_the_manifest_it_was_stored_as() {
    let digest = digest_of(MANIFEST.as_bytes());
    let path = "/v2/library/app/manifests/latest".to_owned();
    let rig = Rig::new(files(vec![(path.clone(), Fake::ok(MANIFEST))])).await;
    let answer = rig.get(&path).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, MANIFEST.as_bytes());
    assert_eq!(answer.header("docker-content-digest"), digest);
    let tag = rig
        .stored("oci/tags/library~app/latest/18446744073709551615")
        .await;
    assert_eq!(tag.as_deref(), Some(digest.as_bytes()));
    assert!(rig.stored(&blob_key(MANIFEST.as_bytes())).await.is_some());
    let by_tag = rig.get(&path).await;
    assert_eq!(by_tag.body, MANIFEST.as_bytes());
    let by_digest = rig.get(&format!("/v2/other/name/manifests/{digest}")).await;
    assert_eq!(by_digest.body, MANIFEST.as_bytes());
    assert_eq!(rig.registry.seen().len(), 1);
}

/// Asserts a blob the store lacks is streamed from the upstream into the store and served with its
/// length, then served from the store (a range of it too), with no second request upstream.
#[tokio::test]
async fn a_missing_blob_is_pulled_into_the_store_and_then_served_from_it() {
    let content = blob(100_000);
    let digest = digest_of(&content);
    let path = format!("/v2/library/app/blobs/{digest}");
    let rig = Rig::new(files(vec![(path.clone(), Fake::ok(content.clone()))])).await;
    let answer = rig.get(&path).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.header("content-length"), "100000");
    assert_eq!(answer.body, content);
    let stored = rig.stored(&blob_key(&content)).await;
    assert_eq!(stored.as_deref(), Some(content.as_slice()));
    let again = rig.get(&path).await;
    assert_eq!(again.body, content);
    let ranged = rig
        .ask(Method::GET, &path, &[("range", "bytes=10-19")])
        .await;
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.body, &content[10..20]);
    assert_eq!(rig.registry.seen().len(), 1);
}

/// Asserts a ranged read of a blob the store lacks fills the whole blob (the upstream does not
/// announce its length) and answers the range, and that a HEAD of another blob the store lacks
/// fills it too and answers the headers with no body.
#[tokio::test]
async fn a_range_or_a_head_of_a_missing_blob_fills_the_whole_blob() {
    let first = blob(5000);
    let second = blob(3000);
    let paths = [&first, &second].map(|content| format!("/v2/a/blobs/{}", digest_of(content)));
    let entries = vec![
        (paths[0].clone(), Fake::chunked(first.clone())),
        (paths[1].clone(), Fake::ok(second.clone())),
    ];
    let rig = Rig::new(files(entries)).await;
    let ranged = rig
        .ask(Method::GET, &paths[0], &[("range", "bytes=2-5")])
        .await;
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.header("content-range"), "bytes 2-5/5000");
    assert_eq!(ranged.body, &first[2..6]);
    let stored = rig.stored(&blob_key(&first)).await;
    assert_eq!(stored.as_deref(), Some(first.as_slice()));
    let head = rig.ask(Method::HEAD, &paths[1], &[]).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-length"), "3000");
    assert!(head.body.is_empty());
    let stored = rig.stored(&blob_key(&second)).await;
    assert_eq!(stored.as_deref(), Some(second.as_slice()));
    assert_eq!(rig.registry.seen().len(), 2);
}

/// Asserts what the upstream does not have stays a 404 of the right code, for a manifest by
/// digest, a tag and a blob, and is counted as not found.
#[tokio::test]
async fn what_the_upstream_does_not_have_is_a_404() {
    let rig = Rig::new(always(404)).await;
    let digest = digest_of(b"absent");
    let manifest = rig.get(&format!("/v2/a/manifests/{digest}")).await;
    manifest.assert_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN");
    let tag = rig.get("/v2/a/manifests/latest").await;
    tag.assert_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN");
    let blob = rig.get(&format!("/v2/a/blobs/{digest}")).await;
    blob.assert_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN");
    assert_eq!(rig.metrics.requests("manifest", "not_found"), 2);
    assert_eq!(rig.metrics.requests("blob", "not_found"), 1);
    assert_eq!(rig.registry.seen().len(), 3);
}

/// Asserts an upstream that fails is a 502 `UNAVAILABLE` with a message that does not say what the
/// upstream said, for a manifest by digest, a tag and a blob, and that nothing is stored.
#[tokio::test]
async fn an_upstream_that_fails_is_a_502_and_stores_nothing() {
    let _quiet = quiet();
    let rig = Rig::new(always(500)).await;
    let digest = digest_of(MANIFEST.as_bytes());
    let paths = [
        format!("/v2/a/manifests/{digest}"),
        "/v2/a/manifests/latest".to_owned(),
        format!("/v2/a/blobs/{digest}"),
    ];
    for path in paths {
        let answer = rig.get(&path).await;
        answer.assert_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE");
        assert_eq!(
            answer.message(),
            "the upstream registry could not give the object"
        );
    }
    assert!(rig.stored(&blob_key(MANIFEST.as_bytes())).await.is_none());
    assert!(rig.tags_stored().await.is_empty());
    assert_eq!(rig.metrics.requests("manifest", "failed"), 2);
    assert_eq!(rig.metrics.requests("blob", "failed"), 1);
}

/// Asserts a manifest whose bytes are not the digest it was asked by is not stored and not
/// served, and is counted as a mismatch.
#[tokio::test]
async fn a_manifest_that_is_not_its_digest_is_refused() {
    let _quiet = quiet();
    let digest = digest_of(MANIFEST.as_bytes());
    let path = format!("/v2/a/manifests/{digest}");
    let rig = Rig::new(files(vec![(path.clone(), Fake::ok(OTHER_MANIFEST))])).await;
    let answer = rig.get(&path).await;
    answer.assert_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE");
    assert_eq!(
        answer.message(),
        "the upstream's manifest does not match its digest and is not stored"
    );
    assert!(rig.stored(&blob_key(MANIFEST.as_bytes())).await.is_none());
    assert!(
        rig.stored(&blob_key(OTHER_MANIFEST.as_bytes()))
            .await
            .is_none()
    );
    assert_eq!(rig.metrics.mismatches(), 1);
}

/// Asserts a blob whose bytes are not the digest it was asked by is not stored and not served,
/// and is counted as a mismatch.
#[tokio::test]
async fn a_blob_that_is_not_its_digest_stores_nothing() {
    let _quiet = quiet();
    let wanted = blob(2000);
    let sent = blob(2001);
    let path = format!("/v2/a/blobs/{}", digest_of(&wanted));
    let rig = Rig::new(files(vec![(path.clone(), Fake::ok(sent.clone()))])).await;
    let answer = rig.get(&path).await;
    answer.assert_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE");
    assert_eq!(
        answer.message(),
        "the upstream's blob does not match its digest and is not stored"
    );
    assert!(rig.stored(&blob_key(&wanted)).await.is_none());
    assert!(rig.stored(&blob_key(&sent)).await.is_none());
    assert_eq!(rig.metrics.mismatches(), 1);
}

/// Asserts a blob whose upstream cuts the body stores nothing and is a 502 that says the blob was
/// not read in full.
#[tokio::test]
async fn a_blob_cut_midway_stores_nothing() {
    let _quiet = quiet();
    let content = blob(2000);
    let path = format!("/v2/a/blobs/{}", digest_of(&content));
    let cut = Fake::ending(content[..500].to_vec(), Ending::Cut(2000));
    let rig = Rig::new(files(vec![(path.clone(), cut)])).await;
    let answer = rig.get(&path).await;
    answer.assert_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE");
    assert_eq!(
        answer.message(),
        "the upstream's blob could not be read in full"
    );
    assert!(rig.stored(&blob_key(&content)).await.is_none());
    assert_eq!(rig.metrics.mismatches(), 0);
}

/// Asserts what the upstream sends for a tag that is no manifest is refused, and neither a
/// manifest nor the tag is stored.
#[tokio::test]
async fn an_answer_that_is_not_a_manifest_is_refused() {
    let _quiet = quiet();
    let path = "/v2/a/manifests/latest".to_owned();
    let rig = Rig::new(files(vec![(path.clone(), Fake::ok("not json"))])).await;
    let answer = rig.get(&path).await;
    answer.assert_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE");
    assert_eq!(answer.message(), "the upstream's answer is not a manifest");
    assert!(rig.stored(&blob_key(b"not json")).await.is_none());
    assert!(rig.tags_stored().await.is_empty());
}

/// Asserts a manifest longer than the limit is refused as the upstream's failure.
#[tokio::test]
async fn a_manifest_over_the_limit_is_refused() {
    let _quiet = quiet();
    let size = usize::try_from(MAX_MANIFEST_BYTES).expect("a size") + 1;
    let path = "/v2/a/manifests/latest".to_owned();
    let rig = Rig::new(files(vec![(path.clone(), Fake::ok(vec![b' '; size]))])).await;
    let answer = rig.get(&path).await;
    answer.assert_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE");
    assert_eq!(
        answer.message(),
        "the upstream registry could not give the object"
    );
}

/// Asserts a store that cannot take what was pulled is the store's failure and not the
/// upstream's: for a manifest, for the tag that points at it, and for a blob.
#[tokio::test]
async fn a_store_that_cannot_keep_what_was_pulled_is_503() {
    let _quiet = quiet();
    let down = || Error::Unavailable("down".to_owned());
    let content = blob(1000);
    let blob_digest = digest_of(&content);
    let manifest_digest = digest_of(MANIFEST.as_bytes());
    let entries = vec![
        (
            format!("/v2/a/manifests/{manifest_digest}"),
            Fake::ok(MANIFEST),
        ),
        ("/v2/a/manifests/latest".to_owned(), Fake::ok(MANIFEST)),
        (format!("/v2/a/blobs/{blob_digest}"), Fake::ok(content)),
    ];
    let rig = Rig::new(files(entries)).await;
    rig.faulty.fail_next_on(Op::Put, "oci/blobs/", down());
    let by_digest = rig.get(&format!("/v2/a/manifests/{manifest_digest}")).await;
    by_digest.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    rig.faulty.fail_next_on(Op::Put, "oci/tags/", down());
    let by_tag = rig.get("/v2/a/manifests/latest").await;
    by_tag.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    rig.faulty.fail_next_on(Op::Put, "oci/blobs/", down());
    let blob = rig.get(&format!("/v2/a/blobs/{blob_digest}")).await;
    blob.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
}

/// Asserts an object that appears in the store while it is being pulled is taken as it is: the
/// create-only write finds it, and the request is answered from the store. The store's first
/// look is made to miss, as it would if the other pull had not yet stored it.
#[tokio::test]
async fn an_object_stored_meanwhile_is_taken_as_pulled() {
    let _quiet = quiet();
    let content = blob(1000);
    let blob_digest = digest_of(&content);
    let manifest_digest = digest_of(MANIFEST.as_bytes());
    let entries = vec![
        (
            format!("/v2/a/manifests/{manifest_digest}"),
            Fake::ok(MANIFEST),
        ),
        ("/v2/a/manifests/latest".to_owned(), Fake::ok(MANIFEST)),
        (
            format!("/v2/a/blobs/{blob_digest}"),
            Fake::ok(content.clone()),
        ),
    ];
    let rig = Rig::new(files(entries)).await;
    let missed = || Error::NotFound("raced".to_owned());
    rig.put_under(&blob_key(MANIFEST.as_bytes()), MANIFEST.as_bytes())
        .await;
    rig.faulty.fail_next_with(Op::Head, missed());
    let manifest = rig.get(&format!("/v2/a/manifests/{manifest_digest}")).await;
    assert_eq!(manifest.status, StatusCode::OK);
    assert_eq!(manifest.body, MANIFEST.as_bytes());
    rig.put_under(&blob_key(&content), &content).await;
    rig.faulty.fail_next_with(Op::Head, missed());
    let answer = rig.get(&format!("/v2/a/blobs/{blob_digest}")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, content);
    assert_eq!(rig.registry.seen().len(), 2);
}

/// Asserts a tag created between the read of it and the write of its first version is kept: the
/// create-only write finds it, nothing is overwritten, and the request is answered with the
/// manifest that tag points at, not the one the upstream sent. The first two listings of the tag
/// are made to miss (the read, and the read once more that a miss gets), as they would if the other
/// pull had not yet written it.
#[tokio::test]
async fn a_tag_created_meanwhile_with_another_digest_is_kept_and_answered() {
    let _quiet = quiet();
    let entries = vec![("/v2/a/manifests/latest".to_owned(), Fake::ok(MANIFEST))];
    let rig = Rig::new(files(entries)).await;
    let other = digest_of(OTHER_MANIFEST.as_bytes());
    let version = "oci/tags/a/latest/18446744073709551615";
    rig.put_under(
        &blob_key(OTHER_MANIFEST.as_bytes()),
        OTHER_MANIFEST.as_bytes(),
    )
    .await;
    rig.put_under(version, other.as_bytes()).await;
    for _ in 0..2 {
        let missed = Error::NotFound("raced".to_owned());
        rig.faulty.fail_next_on(Op::List, "oci/tags/", missed);
    }
    let tagged = rig.get("/v2/a/manifests/latest").await;
    assert_eq!(tagged.status, StatusCode::OK);
    assert_eq!(tagged.body, OTHER_MANIFEST.as_bytes());
    assert_eq!(tagged.header("docker-content-digest"), other);
    assert_eq!(rig.tags_stored().await, [version]);
    assert_eq!(rig.stored(version).await.as_deref(), Some(other.as_bytes()));
    assert_eq!(rig.registry.seen().len(), 1);
}

/// Asserts a store that fails is not answered by going to the upstream: a tag that cannot be read,
/// a manifest or a blob that cannot be measured is the store's failure, and the upstream is not
/// asked.
#[tokio::test]
async fn a_store_that_fails_is_not_answered_from_the_upstream() {
    let _quiet = quiet();
    let rig = Rig::new(always(200)).await;
    let digest = digest_of(MANIFEST.as_bytes());
    // A tag is read by listing its versions.
    rig.faulty.fail_next(Op::List, 1);
    let tag = rig.get("/v2/a/manifests/latest").await;
    tag.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    rig.faulty.fail_next(Op::Head, 1);
    let manifest = rig.get(&format!("/v2/a/manifests/{digest}")).await;
    manifest.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    rig.faulty.fail_next(Op::Head, 1);
    let blob = rig.get(&format!("/v2/a/blobs/{digest}")).await;
    blob.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    assert!(rig.registry.seen().is_empty());
}
