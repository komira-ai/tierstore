//! The pushes of the OCI front that begin and end a blob in one call, the routes and keys that
//! allow a push, and the streams a push is stored from: over an in-memory store, with no socket.
//! The chunked uploads are in `push_chunked_tests`, the manifests in `push_manifest_tests`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use http_body_util::{BodyExt as _, Full, StreamBody};
use hyper::body::Frame;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, StatusCode};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::body::{self, OciBody};
use super::names::Digest;
use super::push::{Pieces, Refusal, Tally, counted, pieces};
use super::{OciAuth, OciFront, OciMetrics, OciPrefix};
use crate::server::testing::{Scripted, Step};
use crate::server::{AccessKey, ServerConfig};
use crate::{
    Backend, ByteRange, Error, FaultyBackend, Listed, MemoryBackend, Meta, Op, PartBody, TestClock,
};

/// The time a push waits for the next piece of a body.
const IDLE: Duration = Duration::from_secs(30);

pub(super) fn hex_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `sha256:<hex>` of `bytes`.
pub(super) fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_of(bytes))
}

/// The server whose keys the front asks for: the full key and a read-only key.
fn server() -> ServerConfig {
    let clock = Arc::new(TestClock::new());
    let mut server = ServerConfig::new("bucket", "FULL", "full-secret", clock);
    let reader = AccessKey::new("READER", "reader-secret");
    server.read_only_keys.push(reader);
    server
}

/// A front that asks for keys, over `backend`.
pub(super) fn front_over(backend: Arc<dyn Backend>) -> (OciFront, Arc<OciMetrics>) {
    let metrics = Arc::new(OciMetrics::default());
    let prefix = OciPrefix::new("oci/").expect("a prefix");
    let auth = OciAuth::from_server(&server(), &prefix);
    let front = OciFront::new(backend, prefix, Arc::clone(&metrics)).with_auth(Some(auth));
    (front, metrics)
}

pub(super) fn login(id: &str, secret: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{id}:{secret}")))
}

/// A body of known length.
pub(super) fn full(bytes: &[u8]) -> OciBody {
    body::request(Full::new(Bytes::copy_from_slice(bytes)))
}

/// A body that plays `steps`.
pub(super) fn scripted(steps: Vec<Step>) -> OciBody {
    body::request(Scripted::new(steps))
}

/// A body of pieces that does not say how long it is.
pub(super) fn unsized_body(parts: &[&'static [u8]]) -> OciBody {
    let frames: Vec<Result<Frame<Bytes>, std::io::Error>> = parts
        .iter()
        .map(|part| Ok(Frame::data(Bytes::from_static(part))))
        .collect();
    body::request(StreamBody::new(stream::iter(frames)))
}

/// What the front answered.
pub(super) struct Answer {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) body: Vec<u8>,
}

impl Answer {
    #[track_caller]
    pub(super) fn header(&self, name: &str) -> &str {
        let value = self.headers.get(name);
        let value = value.unwrap_or_else(|| panic!("no {name} in {}", self.text()));
        value.to_str().expect("text")
    }

    pub(super) fn text(&self) -> String {
        format!("{} {}", self.status, String::from_utf8_lossy(&self.body))
    }

    /// The `(code, message, detail)` of the one error in the body.
    #[track_caller]
    pub(super) fn error(&self) -> (String, String, Value) {
        let document: Value = serde_json::from_slice(&self.body).expect("an error document");
        let error = &document["errors"][0];
        let code = error["code"].as_str().expect("a code").to_owned();
        let message = error["message"].as_str().expect("a message").to_owned();
        (code, message, error["detail"].clone())
    }

    /// Asserts the answer is the error `code` with `status`, and gives its message and detail.
    #[track_caller]
    pub(super) fn assert_error(&self, status: StatusCode, code: &str) -> (String, Value) {
        assert_eq!(self.status, status, "{}", self.text());
        let (found, message, detail) = self.error();
        assert_eq!(found, code, "{message}");
        (message, detail)
    }
}

/// A front over an in-memory store, which can be told to fail.
pub(super) struct Rig {
    pub(super) faulty: Arc<FaultyBackend>,
    pub(super) memory: Arc<MemoryBackend>,
    pub(super) front: OciFront,
    pub(super) metrics: Arc<OciMetrics>,
}

impl Rig {
    pub(super) fn new() -> Self {
        let memory = Arc::new(MemoryBackend::new());
        let faulty = Arc::new(FaultyBackend::new(memory.clone()));
        let (front, metrics) = front_over(faulty.clone());
        Self {
            faulty,
            memory,
            front,
            metrics,
        }
    }

    /// A request, as the full key unless the headers log in as another.
    pub(super) async fn ask(
        &self,
        method: Method,
        target: &str,
        headers: &[(&str, &str)],
        body: OciBody,
    ) -> Answer {
        let mut map = HeaderMap::new();
        map.insert(
            "authorization",
            login("FULL", "full-secret").parse().expect("a value"),
        );
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).expect("a name");
            map.insert(name, HeaderValue::from_str(value).expect("a value"));
        }
        let response = self
            .front
            .handle_with(&method, target, &map, body, IDLE)
            .await;
        let (parts, mut body) = response.into_parts();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            bytes.extend(frame.expect("a whole body").into_data().expect("data"));
        }
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: bytes,
        }
    }

    /// A request with no body.
    pub(super) async fn send(&self, method: Method, target: &str) -> Answer {
        self.ask(method, target, &[], full(b"")).await
    }

    /// Begins an upload and gives its id.
    pub(super) async fn begin(&self) -> String {
        let answer = self.send(Method::POST, "/v2/app/blobs/uploads/").await;
        assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.text());
        answer.header("docker-upload-uuid").to_owned()
    }

    /// The path of the upload `id`.
    pub(super) fn session(id: &str) -> String {
        format!("/v2/app/blobs/uploads/{id}")
    }

    /// Stores `content` where its digest names it, and gives `sha256:<hex>`.
    pub(super) async fn seed(&self, content: &[u8]) -> String {
        let key = format!("oci/blobs/sha256/{}", hex_of(content));
        let put = self.memory.put(&key, Bytes::copy_from_slice(content)).await;
        put.expect("put");
        digest_of(content)
    }

    /// The bytes stored as the blob `digest`, if there are any.
    pub(super) async fn blob(&self, digest: &str) -> Option<Vec<u8>> {
        let hex = digest.strip_prefix("sha256:").expect("a digest");
        let got = self
            .memory
            .get(&format!("oci/blobs/sha256/{hex}"), None)
            .await;
        got.ok().map(|bytes| bytes.to_vec())
    }

    /// The keys of everything the front keeps for uploads.
    pub(super) async fn staged(&self) -> Vec<String> {
        let listed = self.memory.list("oci/uploads/").await.expect("a listing");
        listed.into_iter().map(|object| object.key).collect()
    }

    /// The keys of every object of the front that is not an upload.
    pub(super) async fn objects(&self) -> Vec<String> {
        let listed = self.memory.list("oci/blobs/").await.expect("a listing");
        listed.into_iter().map(|object| object.key).collect()
    }
}

/// A store that says it needs the length of a body, as an S3 tier does.
pub(super) struct Lengthy(pub(super) MemoryBackend);

#[async_trait]
impl Backend for Lengthy {
    fn needs_length(&self) -> bool {
        true
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.0.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.0.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.0.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.0.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.0.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.0.list(prefix).await
    }
}

/// A front over a store that needs lengths.
pub(super) fn lengthy() -> (Arc<Lengthy>, OciFront) {
    let store = Arc::new(Lengthy(MemoryBackend::new()));
    let (front, _) = front_over(store.clone());
    (store, front)
}

/// Asserts that a `POST` with a `digest` stores its body under that digest in one call, as a
/// client sends the digest percent-encoded: 201, where the blob is and its digest, and the front
/// serves the blob back.
#[tokio::test]
async fn a_monolithic_post_stores_the_blob_under_its_digest() {
    let rig = Rig::new();
    let digest = digest_of(b"layer bytes");
    let target = format!(
        "/v2/app/blobs/uploads/?digest={}",
        digest.replace(':', "%3A")
    );
    let answer = rig
        .ask(Method::POST, &target, &[], full(b"layer bytes"))
        .await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.text());
    assert_eq!(answer.header("location"), format!("/v2/app/blobs/{digest}"));
    assert_eq!(answer.header("docker-content-digest"), digest);
    assert_eq!(answer.header("content-length"), "0");
    assert_eq!(
        answer.header("docker-distribution-api-version"),
        "registry/2.0"
    );
    assert_eq!(
        rig.blob(&digest).await.as_deref(),
        Some(b"layer bytes".as_slice())
    );
    assert!(rig.staged().await.is_empty());
    let pulled = rig
        .send(Method::GET, &format!("/v2/app/blobs/{digest}"))
        .await;
    assert_eq!(pulled.body, b"layer bytes");
    assert_eq!(rig.metrics.requests("upload", "created"), 1);
}

/// Asserts that a body that is not the digest it is sent with is refused 400 `DIGEST_INVALID`,
/// which says the digest that was found, and that nothing is stored: not under the digest
/// claimed, not under the one found.
#[tokio::test]
async fn a_body_that_is_not_its_digest_stores_nothing() {
    let rig = Rig::new();
    let claimed = digest_of(b"what was promised");
    let target = format!("/v2/app/blobs/uploads/?digest={claimed}");
    let answer = rig
        .ask(Method::POST, &target, &[], full(b"what was sent"))
        .await;
    let (_, detail) = answer.assert_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID");
    assert_eq!(detail["digest"], claimed);
    assert_eq!(detail["found"], digest_of(b"what was sent"));
    assert!(rig.objects().await.is_empty());
    assert!(rig.staged().await.is_empty());
    assert_eq!(rig.metrics.requests("upload", "invalid"), 1);
}

/// Asserts that a `digest` that is not `sha256:` and 64 lowercase hex digits is 400
/// `DIGEST_INVALID`, and no body is stored.
#[tokio::test]
async fn a_digest_that_is_not_sha256_is_refused() {
    let rig = Rig::new();
    for bad in ["sha256:abc", "sha512:00", "", "nonsense"] {
        let target = format!("/v2/app/blobs/uploads/?digest={bad}");
        let answer = rig.ask(Method::POST, &target, &[], full(b"x")).await;
        answer.assert_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID");
    }
    assert!(rig.objects().await.is_empty());
}

/// Asserts that a `POST` with no `digest` begins an upload: 202, a `Location` that holds a new id
/// of 32 hex digits, the id in `Docker-Upload-UUID`, a `Range` of `0-0`, and the session kept in
/// the store; two uploads have two ids.
#[tokio::test]
async fn a_post_without_a_digest_begins_an_upload() {
    let rig = Rig::new();
    let first = rig.send(Method::POST, "/v2/app/blobs/uploads/").await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.text());
    let id = first.header("docker-upload-uuid").to_owned();
    assert_eq!(id.len(), 32);
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(first.header("location"), Rig::session(&id));
    assert_eq!(first.header("range"), "0-0");
    assert_eq!(first.header("content-length"), "0");
    let second = rig.begin().await;
    assert_ne!(id, second);
    let mut want = vec![
        format!("oci/uploads/{id}/begin"),
        format!("oci/uploads/{second}/begin"),
    ];
    want.sort();
    let mut kept = rig.staged().await;
    kept.sort();
    assert_eq!(kept, want);
    assert_eq!(rig.metrics.requests("upload", "accepted"), 2);
}

/// Asserts that a name that is not a repository name is 400 `NAME_INVALID` on every push call,
/// and nothing is stored.
#[tokio::test]
async fn a_name_that_is_not_valid_is_refused_on_every_push() {
    let rig = Rig::new();
    let id = rig.begin().await;
    let digest = digest_of(b"x");
    let calls = [
        (Method::POST, "/v2/A/blobs/uploads/".to_owned()),
        (Method::PATCH, format!("/v2/A/blobs/uploads/{id}")),
        (
            Method::PUT,
            format!("/v2/A/blobs/uploads/{id}?digest={digest}"),
        ),
        (Method::DELETE, format!("/v2/A/blobs/uploads/{id}")),
        (Method::PUT, format!("/v2/A/manifests/{digest}")),
    ];
    for (method, target) in calls {
        let answer = rig.ask(method, &target, &[], full(b"x")).await;
        answer.assert_error(StatusCode::BAD_REQUEST, "NAME_INVALID");
    }
    assert_eq!(rig.staged().await.len(), 1);
    assert!(rig.objects().await.is_empty());
}

/// Asserts that the 405 of a front that asks for keys lists what the route serves (a manifest
/// `GET, HEAD, PUT`, the start of an upload `POST`, an upload `PATCH, PUT, DELETE`, a blob and
/// the rest `GET, HEAD`), and that a `GET` of an upload path is 404 `UNSUPPORTED`.
#[tokio::test]
async fn the_405_of_a_front_with_keys_lists_what_the_route_serves() {
    let rig = Rig::new();
    let digest = rig.seed(b"x").await;
    let id = rig.begin().await;
    let cases = [
        (
            Method::DELETE,
            format!("/v2/app/manifests/{digest}"),
            "GET, HEAD, PUT",
        ),
        (Method::PUT, "/v2/app/blobs/uploads/".to_owned(), "POST"),
        (Method::POST, Rig::session(&id), "PATCH, PUT, DELETE"),
        (Method::PUT, format!("/v2/app/blobs/{digest}"), "GET, HEAD"),
        (Method::POST, "/v2/".to_owned(), "GET, HEAD"),
        (Method::OPTIONS, "/v2/app/blobs/uploads/".to_owned(), "POST"),
    ];
    for (method, target, allow) in cases {
        let answer = rig.send(method.clone(), &target).await;
        let (message, _) = answer.assert_error(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED");
        assert_eq!(answer.header("allow"), allow, "{method} {target}");
        assert!(message.contains("not served on this path"), "{message}");
    }
    for target in ["/v2/app/blobs/uploads/".to_owned(), Rig::session(&id)] {
        let answer = rig.send(Method::GET, &target).await;
        answer.assert_error(StatusCode::NOT_FOUND, "UNSUPPORTED");
    }
    assert_eq!(rig.metrics.requests("upload", "not_allowed"), 3);
    assert_eq!(rig.metrics.requests("upload", "not_found"), 2);
}

/// Asserts that a front without keys serves no push: `POST`, `PATCH`, `PUT` and `DELETE` of the
/// upload and manifest paths are 405 `GET, HEAD`, whatever they carry, and nothing is stored.
#[tokio::test]
async fn a_front_without_keys_is_read_only() {
    let memory = Arc::new(MemoryBackend::new());
    let metrics = Arc::new(OciMetrics::default());
    let prefix = OciPrefix::new("oci/").expect("a prefix");
    let front = OciFront::new(memory.clone(), prefix, metrics);
    let digest = digest_of(b"x");
    let id = "0".repeat(32);
    let calls = [
        (
            Method::POST,
            format!("/v2/app/blobs/uploads/?digest={digest}"),
        ),
        (Method::PATCH, format!("/v2/app/blobs/uploads/{id}")),
        (
            Method::PUT,
            format!("/v2/app/blobs/uploads/{id}?digest={digest}"),
        ),
        (Method::DELETE, format!("/v2/app/blobs/uploads/{id}")),
        (Method::PUT, format!("/v2/app/manifests/{digest}")),
    ];
    for (method, target) in calls {
        let headers = HeaderMap::new();
        let response = front
            .handle_with(&method, &target, &headers, full(b"x"), IDLE)
            .await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {target}"
        );
        assert_eq!(response.headers()["allow"], "GET, HEAD");
    }
    assert!(memory.list("").await.expect("a listing").is_empty());
}

/// Asserts that a key that may not push is refused 403 `DENIED` before the body is read or
/// anything is stored.
#[tokio::test]
async fn a_key_that_may_not_push_stores_nothing() {
    let rig = Rig::new();
    let reader = login("READER", "reader-secret");
    let digest = digest_of(b"x");
    let target = format!("/v2/app/blobs/uploads/?digest={digest}");
    let answer = rig
        .ask(
            Method::POST,
            &target,
            &[("authorization", reader.as_str())],
            full(b"x"),
        )
        .await;
    answer.assert_error(StatusCode::FORBIDDEN, "DENIED");
    assert!(rig.memory.list("").await.expect("a listing").is_empty());
    assert_eq!(rig.metrics.requests("upload", "denied"), 1);
}

/// Asserts that a store that fails to take the object of a new upload is 503 `UNAVAILABLE`, and
/// that a monolithic push whose store fails is too, with nothing stored.
#[tokio::test]
async fn a_store_that_fails_is_503_on_a_new_upload_and_a_monolithic_push() {
    let rig = Rig::new();
    rig.faulty.fail_next(Op::Put, 1);
    let answer = rig.send(Method::POST, "/v2/app/blobs/uploads/").await;
    answer.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    rig.faulty.fail_next(Op::Put, 1);
    let target = format!("/v2/app/blobs/uploads/?digest={}", digest_of(b"x"));
    let answer = rig.ask(Method::POST, &target, &[], full(b"x")).await;
    answer.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    assert!(rig.memory.list("").await.expect("a listing").is_empty());
}

/// Asserts that a blob that is already stored is created again without error (201), and stays
/// the one object.
#[tokio::test]
async fn a_blob_that_is_already_stored_is_created_again() {
    let rig = Rig::new();
    let target = format!("/v2/app/blobs/uploads/?digest={}", digest_of(b"twice"));
    for _ in 0..2 {
        let answer = rig.ask(Method::POST, &target, &[], full(b"twice")).await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.text());
    }
    assert_eq!(rig.objects().await.len(), 1);
}

/// Asserts that a store that needs the length of a body refuses a monolithic push that has none
/// (411 `BLOB_UPLOAD_INVALID`, nothing stored) and takes one that has.
#[tokio::test]
async fn a_store_that_needs_a_length_refuses_a_body_without_one() {
    let (store, front) = lengthy();
    let digest = digest_of(b"abc");
    let target = format!("/v2/app/blobs/uploads/?digest={digest}");
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        login("FULL", "full-secret").parse().expect("a value"),
    );
    let body = unsized_body(&[b"a", b"bc"]);
    let response = front
        .handle_with(&Method::POST, &target, &headers, body, IDLE)
        .await;
    assert_eq!(response.status(), StatusCode::LENGTH_REQUIRED);
    assert!(store.0.list("").await.expect("a listing").is_empty());
    let response = front
        .handle_with(&Method::POST, &target, &headers, full(b"abc"), IDLE)
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

/// Asserts that a body that is cut is refused 400 `BLOB_UPLOAD_INVALID` and nothing is stored, and
/// that the bytes that arrived before the cut are not an object.
#[tokio::test]
async fn a_body_that_is_cut_stores_nothing() {
    let rig = Rig::new();
    let target = format!("/v2/app/blobs/uploads/?digest={}", digest_of(b"ab"));
    let steps = vec![Step::Data(b"a"), Step::Data(b"b"), Step::Fail];
    let answer = rig.ask(Method::POST, &target, &[], scripted(steps)).await;
    let (message, _) = answer.assert_error(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID");
    assert!(message.contains("could not be read in full"), "{message}");
    assert!(rig.objects().await.is_empty());
}

/// Asserts that a frame of trailers carries no byte and does not end the body: the blob is the
/// bytes before and after it.
#[tokio::test]
async fn a_frame_of_trailers_carries_no_byte() {
    let rig = Rig::new();
    let digest = digest_of(b"ab");
    let target = format!("/v2/app/blobs/uploads/?digest={digest}");
    let steps = vec![Step::Data(b"a"), Step::Trailers, Step::Data(b"b")];
    let answer = rig.ask(Method::POST, &target, &[], scripted(steps)).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.text());
    assert_eq!(rig.blob(&digest).await.as_deref(), Some(b"ab".as_slice()));
}

/// Asserts that a body whose next piece does not come within the idle time is refused 400
/// `BLOB_UPLOAD_INVALID` and nothing is stored. The clock is the paused one of the test.
#[tokio::test(start_paused = true)]
async fn a_body_that_stalls_stores_nothing() {
    let rig = Rig::new();
    let target = format!("/v2/app/blobs/uploads/?digest={}", digest_of(b"ab"));
    let steps = vec![
        Step::Data(b"a"),
        Step::Wait(Duration::from_secs(3600)),
        Step::Data(b"b"),
    ];
    let answer = rig.ask(Method::POST, &target, &[], scripted(steps)).await;
    let (message, _) = answer.assert_error(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID");
    assert_eq!(message, "No byte of the request body arrived within 30s.");
    assert!(rig.objects().await.is_empty());
}

/// Collects a stream to its end, as a reader that does not stop at the first error does.
async fn drained(source: PartBody<'static>) -> Vec<Result<Bytes, Error>> {
    source.collect().await
}

fn pieces_of(parts: &[&'static [u8]]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = parts
        .iter()
        .map(|part| Ok(Bytes::from_static(part)))
        .collect();
    stream::iter(items).boxed()
}

/// Asserts the stream of a body that is counted and not hashed: every piece passes and is counted,
/// an error ends it (nothing is read after it, though it is polled again), and no refusal is kept.
#[tokio::test]
async fn a_counted_stream_passes_pieces_and_ends_at_its_first_error() {
    let tally = Arc::new(Tally::default());
    let items: Vec<Result<Bytes, Error>> = vec![
        Ok(Bytes::from_static(b"abc")),
        Err(Error::Other("cut".to_owned())),
        Ok(Bytes::from_static(b"never")),
    ];
    let source = stream::iter(items).boxed();
    let out = drained(counted(source, None, Arc::clone(&tally))).await;
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], Ok(Bytes::from_static(b"abc")));
    assert_eq!(out[1], Err(Error::Other("cut".to_owned())));
    assert_eq!(tally.bytes(), 3);
    assert!(tally.take().is_none());
}

/// Asserts the stream of a body that is hashed: bytes that hash to the digest pass whole, and
/// bytes that do not end the stream with `BadDigest` after every piece, with the hash found kept.
#[tokio::test]
async fn a_hashed_stream_ends_with_an_error_unless_the_digest_is_right() {
    let right = Digest::parse(&digest_of(b"abcdef")).expect("a digest");
    let tally = Arc::new(Tally::default());
    let out = drained(counted(
        pieces_of(&[b"abc", b"def"]),
        Some(right),
        Arc::clone(&tally),
    ))
    .await;
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(Result::is_ok));
    assert!(tally.take().is_none());

    let wrong = Digest::parse(&digest_of(b"other")).expect("a digest");
    let tally = Arc::new(Tally::default());
    let out = drained(counted(
        pieces_of(&[b"abc", b"def"]),
        Some(wrong.clone()),
        Arc::clone(&tally),
    ))
    .await;
    assert_eq!(out.len(), 3);
    assert_eq!(out[2], Err(Error::BadDigest(wrong.text())));
    assert_eq!(tally.bytes(), 6);
    match tally.take() {
        Some(Refusal::Digest { found }) => assert_eq!(found, hex_of(b"abcdef")),
        _ => panic!("the hash found is kept"),
    }
}

/// Asserts the stream of a request body: its pieces pass, a body that fails ends it with an
/// error after the pieces that were fine, the reason is kept, and the stream stays ended.
#[tokio::test]
async fn the_pieces_of_a_request_body_end_at_its_failure() {
    let tally = Arc::new(Tally::default());
    let steps = vec![Step::Data(b"ab"), Step::Fail, Step::Data(b"never")];
    let reader = Pieces::new(scripted(steps), IDLE);
    let out = drained(pieces(reader, Arc::clone(&tally))).await;
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], Ok(Bytes::from_static(b"ab")));
    assert!(out[1].is_err());
    match tally.take() {
        Some(Refusal::Body(message)) => assert!(message.contains("could not be read"), "{message}"),
        _ => panic!("the reason is kept"),
    }
}

/// Asserts that a tally keeps the first refusal and not a later one.
#[test]
fn a_tally_keeps_the_first_refusal() {
    let tally = Tally::default();
    tally.refuse(Refusal::Body("first".to_owned()));
    tally.refuse(Refusal::Body("second".to_owned()));
    match tally.take() {
        Some(Refusal::Body(message)) => assert_eq!(message, "first"),
        _ => panic!("a refusal"),
    }
    assert!(tally.take().is_none());
}
