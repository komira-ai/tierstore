//! The pull-through of the front, over an in-memory store and a registry in the process (see
//! `fake_upstream`): no network.

use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt as _;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, StatusCode};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tracing::Level;
use tracing::subscriber::{DefaultGuard, set_default};

use super::{OciFront, Upstream};
use crate::server::build_info::stderr_log::LineLog;
use crate::server::oci::fake_upstream::{Ending, Fake, Registry, Seen};
use crate::server::oci::{
    DEFAULT_MANIFEST_BYTES, OciMetrics, OciPrefix, OciSizes, UpstreamSettings,
};
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

/// The re-check deadline of the rig's upstream.
const RECHECK_DEADLINE: Duration = Duration::from_secs(5);

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
            recheck_timeout: RECHECK_DEADLINE,
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
    // The second pull of the tag is re-checked with a HEAD; the pull by digest asks nothing.
    assert_eq!(methods(&rig), ["GET", "HEAD"]);
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
    let size = usize::try_from(DEFAULT_MANIFEST_BYTES).expect("a size") + 1;
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

/// Asserts `params.max_manifest_bytes` bounds a manifest pulled from the upstream: with it at
/// 50, a manifest of 51 bytes is refused as the upstream's failure and one of 50 is stored.
#[tokio::test]
async fn a_lowered_manifest_limit_refuses_a_pulled_manifest_just_over_it() {
    let _quiet = quiet();
    let path = "/v2/a/manifests/latest".to_owned();
    let make = |len: usize| {
        let head = r#"{"schemaVersion":2,"config":{},"layers":[],"pad":""#;
        format!("{head}{}\"}}", "a".repeat(len - head.len() - 2)).into_bytes()
    };
    let files = files(vec![(path.clone(), Fake::ok(make(80)))]);
    let mut rig = Rig::new(files).await;
    let sizes = |bytes| OciSizes {
        manifest_bytes: bytes,
        ..OciSizes::default()
    };
    rig.front = rig.front.with_sizes(sizes(79));
    let refused = rig.get(&path).await;
    refused.assert_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE");
    rig.front = rig.front.with_sizes(sizes(80));
    let served = rig.get(&path).await;
    assert_eq!(served.status, StatusCode::OK);
}

const FIRST_VERSION: &str = "oci/tags/library~app/latest/18446744073709551615";
const SECOND_VERSION: &str = "oci/tags/library~app/latest/18446744073709551614";
const TAG_PATH: &str = "/v2/library/app/manifests/latest";

/// What the registry of the re-check tests does now, changed by the test between pulls.
struct Script {
    /// The manifest the tag is at.
    manifest: &'static str,
    /// The status of a `HEAD` of the tag.
    head_status: u16,
    /// The `Docker-Content-Digest` of a `HEAD`: the digest of `manifest` if `None`.
    head_digest: Option<String>,
    /// The status of a `GET` of the tag.
    get_status: u16,
    /// Whether a `HEAD` of the tag is never answered.
    head_hangs: bool,
    /// Whether a `GET` of the tag is never answered.
    get_hangs: bool,
    /// Told when the registry is sent the first request that it never answers (see
    /// [`hold_clock_until_hang`]).
    hung: Option<mpsc::Sender<()>>,
}

type Control = Arc<Mutex<Script>>;

fn script() -> Control {
    Arc::new(Mutex::new(Script {
        manifest: MANIFEST,
        head_status: 200,
        head_digest: None,
        get_status: 200,
        head_hangs: false,
        get_hangs: false,
        hung: None,
    }))
}

fn change(control: &Control, edit: impl FnOnce(&mut Script)) {
    edit(&mut control.lock().unwrap_or_else(PoisonError::into_inner));
}

/// A registry that has the tag `latest` of `library/app` as `control` says.
async fn tag_rig(control: &Control) -> Rig {
    let control = Arc::clone(control);
    Rig::new(move |seen| {
        let mut now = control.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.path != TAG_PATH {
            return Fake::status(404);
        }
        if (seen.method == "HEAD" && now.head_hangs) || (seen.method == "GET" && now.get_hangs) {
            if let Some(hung) = now.hung.take() {
                let _ = hung.send(());
            }
            return Fake::ending(Vec::new(), Ending::Hang);
        }
        if seen.method == "HEAD" {
            let digest =
                (now.head_digest.clone()).unwrap_or_else(|| digest_of(now.manifest.as_bytes()));
            Fake::status(now.head_status).with("docker-content-digest", &digest)
        } else if now.get_status == 200 {
            Fake::ok(now.manifest)
        } else {
            Fake::status(now.get_status)
        }
    })
    .await
}

/// Keeps the paused clock of a test where it is until the registry of `control` is sent the
/// request that it never answers, and lets it move on from then.
///
/// The fake registry answers over a loopback socket. With the clock paused, tokio moves it to
/// the next timer whenever the runtime has nothing to run, and a runtime waiting for a reply that
/// is still on its way through the kernel has nothing to run: the clock would jump to the
/// upstream's per-step `timeout` in the middle of a pull that is answered (which then fails, 502)
/// or to the re-check deadline before the request that hangs is made. While a blocking task runs,
/// tokio does not move a paused clock on (`tokio::time::pause`, "Preventing auto-advance"), so
/// this one runs until the registry is sent the request it never answers. From then on nothing
/// is on its way and the next timer is the one the test is about.
fn hold_clock_until_hang(control: &Control) {
    let (hung, hang) = mpsc::channel();
    change(control, |now| now.hung = Some(hung));
    // Ends at the hang, or when the registry's handler is dropped with the rig.
    drop(tokio::task::spawn_blocking(move || hang.recv()));
}

fn methods(rig: &Rig) -> Vec<String> {
    rig.registry.seen().into_iter().map(|s| s.method).collect()
}

/// Asserts the answer is 200 with `manifest` and its digest.
fn assert_served(answer: &Answer, manifest: &str) {
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, manifest.as_bytes());
    let digest = digest_of(manifest.as_bytes());
    assert_eq!(answer.header("docker-content-digest"), digest);
}

/// Asserts a tag in the store whose digest the upstream still announces is served from the store
/// with a `HEAD` only: a second pull by a `GET` and a third by a `HEAD` make no `GET` upstream,
/// write no version, and count no failure.
#[tokio::test]
async fn an_unchanged_digest_serves_the_stored_manifest_without_a_get() {
    let control = script();
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    assert_eq!(methods(&rig), ["GET"]);
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    let head = rig.ask(Method::HEAD, TAG_PATH, &[]).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(
        head.header("docker-content-digest"),
        digest_of(MANIFEST.as_bytes())
    );
    assert_eq!(methods(&rig), ["GET", "HEAD", "HEAD"]);
    assert_eq!(rig.tags_stored().await, [FIRST_VERSION]);
    assert_eq!(rig.metrics.recheck_failures(), 0);
}

/// Asserts a tag the upstream has moved is pulled again: the new manifest is stored by its
/// digest, the next version of the tag points at it and is served, the old version and manifest
/// stay, and the next pull finds the digest unchanged and makes no `GET`.
#[tokio::test]
async fn a_moved_digest_writes_a_new_version_and_serves_the_new_manifest() {
    let control = script();
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    change(&control, |now| now.manifest = OTHER_MANIFEST);
    assert_served(&rig.get(TAG_PATH).await, OTHER_MANIFEST);
    assert_eq!(methods(&rig), ["GET", "HEAD", "GET"]);
    assert_eq!(rig.tags_stored().await, [SECOND_VERSION, FIRST_VERSION]);
    let newest = digest_of(OTHER_MANIFEST.as_bytes());
    assert_eq!(
        rig.stored(SECOND_VERSION).await.as_deref(),
        Some(newest.as_bytes())
    );
    let first = digest_of(MANIFEST.as_bytes());
    assert_eq!(
        rig.stored(FIRST_VERSION).await.as_deref(),
        Some(first.as_bytes())
    );
    assert!(rig.stored(&blob_key(MANIFEST.as_bytes())).await.is_some());
    let stored = rig.stored(&blob_key(OTHER_MANIFEST.as_bytes())).await;
    assert_eq!(stored.as_deref(), Some(OTHER_MANIFEST.as_bytes()));
    assert_served(&rig.get(TAG_PATH).await, OTHER_MANIFEST);
    assert_eq!(methods(&rig), ["GET", "HEAD", "GET", "HEAD"]);
    assert_eq!(rig.metrics.recheck_failures(), 0);
}

/// Asserts a `HEAD` that fails (5xx, 429, a status that is not a digest answer) serves the stored
/// tag, with no `GET`, no new version, and one failure counted for each pull.
#[tokio::test]
async fn an_upstream_that_fails_the_head_serves_the_stored_tag_and_counts_it() {
    let _quiet = quiet();
    let control = script();
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    change(&control, |now| now.manifest = OTHER_MANIFEST);
    for (status, count) in [500_u16, 429, 503, 401].into_iter().zip(1_u64..) {
        change(&control, |now| now.head_status = status);
        assert_served(&rig.get(TAG_PATH).await, MANIFEST);
        assert_eq!(rig.metrics.recheck_failures(), count);
    }
    assert!(!methods(&rig)[1..].contains(&"GET".to_owned()));
    assert_eq!(rig.tags_stored().await, [FIRST_VERSION]);
    let text = rig.metrics.render();
    assert!(
        text.contains("tierstore_oci_tag_recheck_failures_total 4\n"),
        "{text}"
    );
}

/// Asserts a `HEAD` answered 200 without a usable `Docker-Content-Digest` (none, another
/// algorithm) is a failure to check, and the stored tag is served.
#[tokio::test]
async fn a_head_without_a_usable_digest_serves_the_stored_tag_and_counts_it() {
    let _quiet = quiet();
    let control = script();
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    change(&control, |now| now.manifest = OTHER_MANIFEST);
    for (bad, count) in ["", "sha512:abc"].into_iter().zip(1_u64..) {
        change(&control, |now| now.head_digest = Some(bad.to_owned()));
        assert_served(&rig.get(TAG_PATH).await, MANIFEST);
        assert_eq!(rig.metrics.recheck_failures(), count);
    }
    assert_eq!(rig.tags_stored().await, [FIRST_VERSION]);
}

/// Asserts a tag the upstream has moved whose manifest then cannot be pulled (404, 500) keeps the
/// stored tag, counts a failure, and writes no version.
#[tokio::test]
async fn a_moved_tag_that_cannot_be_pulled_serves_the_stored_tag_and_counts_it() {
    let _quiet = quiet();
    let control = script();
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    change(&control, |now| now.manifest = OTHER_MANIFEST);
    for (status, count) in [404_u16, 500].into_iter().zip(1_u64..) {
        change(&control, |now| now.get_status = status);
        assert_served(&rig.get(TAG_PATH).await, MANIFEST);
        assert_eq!(rig.metrics.recheck_failures(), count);
    }
    assert_eq!(rig.tags_stored().await, [FIRST_VERSION]);
}

/// Asserts a tag that is back at the stored digest by the time it is pulled (the `HEAD` said it
/// had moved, the `GET` sends the stored manifest) writes no version and counts no failure.
#[tokio::test]
async fn a_tag_back_at_the_stored_digest_when_pulled_writes_no_version() {
    let control = script();
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    let moved = digest_of(OTHER_MANIFEST.as_bytes());
    change(&control, |now| now.head_digest = Some(moved.clone()));
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    assert_eq!(methods(&rig), ["GET", "HEAD", "GET"]);
    assert_eq!(rig.tags_stored().await, [FIRST_VERSION]);
    assert_eq!(rig.metrics.recheck_failures(), 0);
}

/// Asserts a moved tag whose manifest or new version the store cannot write keeps the stored tag
/// and counts a failure: a failed manifest put writes no version, and a failed version write
/// leaves the tag as it was.
#[tokio::test]
async fn a_store_that_cannot_keep_the_moved_tag_serves_the_stored_tag_and_counts_it() {
    let _quiet = quiet();
    let control = script();
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    change(&control, |now| now.manifest = OTHER_MANIFEST);
    rig.faulty.fail_next(Op::Put, 1);
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    assert_eq!(rig.metrics.recheck_failures(), 1);
    assert_eq!(rig.tags_stored().await, [FIRST_VERSION]);
    rig.faulty
        .fail_next_on(Op::Put, "oci/tags/", Error::Other("full".to_owned()));
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    assert_eq!(rig.metrics.recheck_failures(), 2);
    assert_eq!(rig.tags_stored().await, [FIRST_VERSION]);
}

/// Asserts a pull by digest never contacts the upstream once its object is stored, and a tag
/// pull's manifest is served by digest for any name without a request upstream.
#[tokio::test]
async fn a_pull_by_digest_never_contacts_the_upstream() {
    let control = script();
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    let digest = digest_of(MANIFEST.as_bytes());
    let by_digest = format!("/v2/library/app/manifests/{digest}");
    assert_served(&rig.get(&by_digest).await, MANIFEST);
    let head = rig.ask(Method::HEAD, &by_digest, &[]).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(methods(&rig), ["GET"]);
}

/// Asserts a tag the store holds that the upstream does not have (a tag only pushed here) is
/// served as it is, with no new version and no failure counted, and that a tag moved by a push
/// to a digest the upstream does not hold is moved back to the upstream's by the next pull, as
/// the next version.
#[tokio::test]
async fn a_pushed_tag_stands_unless_the_upstream_has_it_at_another_digest() {
    let control = script();
    let rig = tag_rig(&control).await;
    let pushed = digest_of(OTHER_MANIFEST.as_bytes());
    rig.put_under(
        &blob_key(OTHER_MANIFEST.as_bytes()),
        OTHER_MANIFEST.as_bytes(),
    )
    .await;
    let only_here = "oci/tags/library~app/local/18446744073709551615";
    rig.put_under(only_here, pushed.as_bytes()).await;
    let local = rig.get("/v2/library/app/manifests/local").await;
    assert_served(&local, OTHER_MANIFEST);
    assert_eq!(rig.metrics.recheck_failures(), 0);
    assert_eq!(methods(&rig), ["HEAD"]);
    rig.put_under(FIRST_VERSION, pushed.as_bytes()).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    let upstream = digest_of(MANIFEST.as_bytes());
    assert_eq!(
        rig.stored(SECOND_VERSION).await.as_deref(),
        Some(upstream.as_bytes())
    );
    assert_eq!(methods(&rig), ["HEAD", "HEAD", "GET"]);
}

/// Asserts a re-check whose `HEAD` is never answered serves the stored tag once the re-check
/// deadline (`recheck_timeout`, 5s, shorter than the upstream's per-step `timeout`) has passed on
/// the injected clock, counts one failure, and writes no version. The clock stands still until
/// the `HEAD` is made ([`hold_clock_until_hang`]), so the deadline is the only timer that can end
/// the re-check.
#[tokio::test(start_paused = true)]
async fn a_head_that_is_never_answered_serves_the_stored_tag_at_the_recheck_deadline() {
    let _quiet = quiet();
    let control = script();
    hold_clock_until_hang(&control);
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    change(&control, |now| now.head_hangs = true);
    let before = tokio::time::Instant::now();
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    assert_eq!(tokio::time::Instant::now() - before, RECHECK_DEADLINE);
    assert_eq!(methods(&rig), ["GET", "HEAD"]);
    assert_eq!(rig.metrics.recheck_failures(), 1);
    assert_eq!(rig.tags_stored().await, [FIRST_VERSION]);
}

/// Asserts the deadline bounds the re-check as a whole: a `HEAD` that says the tag moved followed
/// by a `GET` that is never answered serves the stored tag at the same deadline, not after a
/// further per-step timeout, counts one failure, and writes no version. The clock stands still
/// until the `GET` is made ([`hold_clock_until_hang`]): the first pull and the `HEAD` are
/// answered over the loopback socket at the instant the test starts, whatever the load.
#[tokio::test(start_paused = true)]
async fn a_get_of_a_moved_tag_that_is_never_answered_is_cut_at_the_recheck_deadline() {
    let _quiet = quiet();
    let control = script();
    hold_clock_until_hang(&control);
    let rig = tag_rig(&control).await;
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    change(&control, |now| {
        now.manifest = OTHER_MANIFEST;
        now.get_hangs = true;
    });
    let before = tokio::time::Instant::now();
    assert_served(&rig.get(TAG_PATH).await, MANIFEST);
    assert_eq!(tokio::time::Instant::now() - before, RECHECK_DEADLINE);
    assert_eq!(methods(&rig), ["GET", "HEAD", "GET"]);
    assert_eq!(rig.metrics.recheck_failures(), 1);
    assert_eq!(rig.tags_stored().await, [FIRST_VERSION]);
}
