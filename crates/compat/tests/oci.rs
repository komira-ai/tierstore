//! One directory shared by this branch's OCI front, which takes pushes, and the front of the
//! release before it (`tierstore_prev`), which only serves: a blob and a manifest this branch's
//! front accepted are pulled through the previous release's front, with an upload that was begun
//! and not finished left in the directory, and a blob and a manifest the previous release stored
//! are pulled through this branch's front and are taken again by a push of the same digest. The
//! release before is the real `tierstore` library of its commit, not a copy of its keys. A tag
//! moved by this branch's front (a new version object each time, `oci/tags/<name>/<tag>/<n>`) is
//! in the same directory: the previous release, which serves no tag, still pulls by digest what
//! the tag pointed at before and after the move, and this branch tags over what it stored.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::header::HeaderMap;
use hyper::{Method, StatusCode};
use tierstore::server::ServerConfig;
use tierstore::server::oci::{OciAuth, OciBody, OciFront, OciMetrics, OciPrefix};
use tierstore::{Backend, DirBackend, SystemClock};
use tierstore_prev::server::oci::{
    OciBody as PrevBody, OciFront as PrevFront, OciMetrics as PrevMetrics, OciPrefix as PrevPrefix,
};
use tierstore_prev::{Backend as PrevBackend, DirBackend as PrevDir};
use tierstore_testkit::dir::TestDir;

/// `Authorization` for the full key `FULL` with the secret `full-secret`.
const LOGIN: &str = "Basic RlVMTDpmdWxsLXNlY3JldA==";
const LAYER: &[u8] = b"a layer pushed in two chunks";
const LAYER_DIGEST: &str =
    "sha256:c5018fb5fbd58e6642d4a2da5f97b1576a02e7ed078e18af5bf6e4ce6940af65";
const CONFIG: &[u8] = b"{}";
const CONFIG_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
const MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","size":2},"layers":[{"digest":"sha256:c5018fb5fbd58e6642d4a2da5f97b1576a02e7ed078e18af5bf6e4ce6940af65","size":28}]}"#;
const MANIFEST_DIGEST: &str =
    "sha256:7fd740d6f0ad4d55c794620d999ffc2f35a3c4e9eb0a081652e788e2dfc2e94d";

/// A second manifest of the same config and layer (the layer named twice), so that a tag moves.
const MOVED: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","size":2},"layers":[{"digest":"sha256:c5018fb5fbd58e6642d4a2da5f97b1576a02e7ed078e18af5bf6e4ce6940af65","size":28},{"digest":"sha256:c5018fb5fbd58e6642d4a2da5f97b1576a02e7ed078e18af5bf6e4ce6940af65","size":28}]}"#;
const MOVED_DIGEST: &str =
    "sha256:06e761d56156fce933ee15af2674de72b222b1c0f8e882a6d6a28a7666b02d9b";

/// How long a push waits for the next piece of a body; the bodies here are whole.
const IDLE: Duration = Duration::from_secs(30);

/// This branch's front, which asks for the full key, over `dir`.
fn now_front(dir: &TestDir) -> OciFront {
    let backend: Arc<dyn Backend> = Arc::new(DirBackend::open(dir.path()).unwrap());
    let prefix = OciPrefix::new("oci/").unwrap();
    let server = ServerConfig::new("bucket", "FULL", "full-secret", Arc::new(SystemClock));
    let auth = OciAuth::from_server(&server);
    OciFront::new(backend, prefix, Arc::new(OciMetrics::default())).with_auth(Some(auth))
}

/// The previous release's front, anonymous as it was, over `dir`.
fn before_front(dir: &TestDir) -> PrevFront {
    let backend: Arc<dyn PrevBackend> = Arc::new(PrevDir::open(dir.path()).unwrap());
    let prefix = PrevPrefix::new("oci/").unwrap();
    PrevFront::new(backend, prefix, Arc::new(PrevMetrics::default()))
}

/// The status, headers and bytes of an answer of this branch's front.
async fn push(
    front: &OciFront,
    method: Method,
    target: &str,
    body: &'static [u8],
) -> (StatusCode, HeaderMap, Bytes) {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", LOGIN.parse().unwrap());
    let body: OciBody = Full::new(Bytes::from_static(body))
        .map_err(|never| match never {})
        .boxed_unsync();
    let answer = front
        .handle_with(&method, target, &headers, body, IDLE)
        .await;
    let (parts, body) = answer.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, parts.headers, bytes)
}

/// The status and bytes the previous release's front gives for a pull of `target`.
async fn pull_before(front: &PrevFront, target: &str) -> (StatusCode, Bytes) {
    let answer = front.handle(&Method::GET, target, &HeaderMap::new()).await;
    let (parts, body): (_, PrevBody) = answer.into_parts();
    (parts.status, body.collect().await.unwrap().to_bytes())
}

/// The status and bytes this branch's front gives for a pull of `target`.
async fn pull_now(front: &OciFront, target: &str) -> (StatusCode, Bytes) {
    let (status, _, bytes) = push(front, Method::GET, target, b"").await;
    (status, bytes)
}

/// Asserts that the previous release's front pulls, byte for byte, the config, the layer (made
/// from two chunks) and the manifest that this branch's front accepted, while the records of an
/// upload that was begun and left open are in the same directory.
#[tokio::test]
async fn the_previous_release_pulls_what_this_branch_pushed() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-oci-push-new");
    let now = now_front(&dir);

    let (status, _, _) = push(&now, Method::POST, "/v2/app/blobs/uploads/", b"").await;
    assert_eq!(status, StatusCode::ACCEPTED, "an upload left open");
    let (status, headers, _) = push(&now, Method::POST, "/v2/app/blobs/uploads/", b"").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers["location"].to_str().unwrap().to_owned();
    let (first, second) = LAYER.split_at(10);
    let (status, _, _) = push(&now, Method::PATCH, &location, first).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _, _) = push(&now, Method::PATCH, &location, second).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let end = format!("{location}?digest={LAYER_DIGEST}");
    let (status, _, _) = push(&now, Method::PUT, &end, b"").await;
    assert_eq!(status, StatusCode::CREATED);
    let target = format!("/v2/app/blobs/uploads/?digest={CONFIG_DIGEST}");
    let (status, _, _) = push(&now, Method::POST, &target, CONFIG).await;
    assert_eq!(status, StatusCode::CREATED);
    let target = "/v2/app/manifests/latest";
    let (status, headers, _) = push(&now, Method::PUT, target, MANIFEST.as_bytes()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], MANIFEST_DIGEST);

    let before = before_front(&dir);
    let layer = pull_before(&before, &format!("/v2/app/blobs/{LAYER_DIGEST}")).await;
    assert_eq!(layer, (StatusCode::OK, Bytes::from_static(LAYER)));
    let config = pull_before(&before, &format!("/v2/app/blobs/{CONFIG_DIGEST}")).await;
    assert_eq!(config, (StatusCode::OK, Bytes::from_static(CONFIG)));
    let manifest = pull_before(&before, &format!("/v2/app/manifests/{MANIFEST_DIGEST}")).await;
    assert_eq!(manifest, (StatusCode::OK, Bytes::from(MANIFEST)));
}

/// Asserts that this branch's front pulls, byte for byte, a layer, a config and a manifest that
/// the previous release stored (in the keys its front reads), and that a push of the same layer
/// through an upload, with the record of another upload in the directory, is created again and
/// leaves the one object.
#[tokio::test]
async fn this_branch_pulls_what_the_previous_release_stored() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-oci-push-prev");
    let before = PrevDir::open(dir.path()).unwrap();
    for (digest, bytes) in [
        (LAYER_DIGEST, LAYER),
        (CONFIG_DIGEST, CONFIG),
        (MANIFEST_DIGEST, MANIFEST.as_bytes()),
    ] {
        let hex = digest.strip_prefix("sha256:").unwrap();
        let key = format!("oci/blobs/sha256/{hex}");
        before
            .put(&key, Bytes::copy_from_slice(bytes))
            .await
            .unwrap();
    }

    let now = now_front(&dir);
    let layer = pull_now(&now, &format!("/v2/app/blobs/{LAYER_DIGEST}")).await;
    assert_eq!(layer, (StatusCode::OK, Bytes::from_static(LAYER)));
    let config = pull_now(&now, &format!("/v2/app/blobs/{CONFIG_DIGEST}")).await;
    assert_eq!(config, (StatusCode::OK, Bytes::from_static(CONFIG)));
    let manifest = pull_now(&now, &format!("/v2/app/manifests/{MANIFEST_DIGEST}")).await;
    assert_eq!(manifest, (StatusCode::OK, Bytes::from(MANIFEST)));

    let (status, headers, _) = push(&now, Method::POST, "/v2/app/blobs/uploads/", b"").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers["location"].to_str().unwrap().to_owned();
    let (status, _, _) = push(&now, Method::PATCH, &location, LAYER).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let end = format!("{location}?digest={LAYER_DIGEST}");
    let (status, _, _) = push(&now, Method::PUT, &end, b"").await;
    assert_eq!(status, StatusCode::CREATED);
    let blobs = before.list("oci/blobs/").await.unwrap();
    assert_eq!(blobs.len(), 3);
    let left = before.list("oci/uploads/").await.unwrap();
    assert!(left.is_empty(), "{left:?}");
}

/// Asserts that after this branch's front moved a tag from one manifest to another (two version
/// objects, the first byte for byte as it was written) the previous release's front pulls both
/// manifests and the blobs by digest, byte for byte, and answers a pull by the tag 404 as it did
/// (it serves no tags); this branch's front serves the tag as the second manifest.
#[tokio::test]
async fn the_previous_release_pulls_by_digest_what_a_moved_tag_pointed_at() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-oci-tag-moved");
    let now = now_front(&dir);
    for (digest, bytes) in [(CONFIG_DIGEST, CONFIG), (LAYER_DIGEST, LAYER)] {
        let target = format!("/v2/app/blobs/uploads/?digest={digest}");
        let (status, _, _) = push(&now, Method::POST, &target, bytes).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let target = "/v2/app/manifests/latest";
    for (manifest, digest) in [(MANIFEST, MANIFEST_DIGEST), (MOVED, MOVED_DIGEST)] {
        let (status, headers, _) = push(&now, Method::PUT, target, manifest.as_bytes()).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(headers["docker-content-digest"], digest);
    }
    let tagged = pull_now(&now, target).await;
    assert_eq!(tagged, (StatusCode::OK, Bytes::from(MOVED)));

    let before = before_front(&dir);
    let first = pull_before(&before, &format!("/v2/app/manifests/{MANIFEST_DIGEST}")).await;
    assert_eq!(first, (StatusCode::OK, Bytes::from(MANIFEST)));
    let second = pull_before(&before, &format!("/v2/app/manifests/{MOVED_DIGEST}")).await;
    assert_eq!(second, (StatusCode::OK, Bytes::from(MOVED)));
    let layer = pull_before(&before, &format!("/v2/app/blobs/{LAYER_DIGEST}")).await;
    assert_eq!(layer, (StatusCode::OK, Bytes::from_static(LAYER)));
    let by_tag = pull_before(&before, target).await;
    assert_eq!(by_tag.0, StatusCode::NOT_FOUND);

    let store = PrevDir::open(dir.path()).unwrap();
    let versions = store.list("oci/tags/app/latest/").await.unwrap();
    let keys: Vec<&str> = versions.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "oci/tags/app/latest/18446744073709551614",
            "oci/tags/app/latest/18446744073709551615"
        ]
    );
    let first_version = store.get(keys[1], None).await.unwrap();
    assert_eq!(first_version, Bytes::from(MANIFEST_DIGEST));
}

/// Asserts that this branch's front, over a directory the previous release wrote (a manifest and
/// its blobs, and no tag), lists no tags and answers a pull by tag 404, then tags the manifest
/// (201, listed and pulled), and that the previous release's front still pulls the manifest by
/// digest and the store holds the three objects it had.
#[tokio::test]
async fn this_branch_tags_what_the_previous_release_stored() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-oci-tag-prev");
    let before = PrevDir::open(dir.path()).unwrap();
    for (digest, bytes) in [
        (LAYER_DIGEST, LAYER),
        (CONFIG_DIGEST, CONFIG),
        (MANIFEST_DIGEST, MANIFEST.as_bytes()),
    ] {
        let hex = digest.strip_prefix("sha256:").unwrap();
        let key = format!("oci/blobs/sha256/{hex}");
        before
            .put(&key, Bytes::copy_from_slice(bytes))
            .await
            .unwrap();
    }

    let now = now_front(&dir);
    let empty = pull_now(&now, "/v2/app/tags/list").await;
    assert_eq!(
        empty,
        (StatusCode::OK, Bytes::from(r#"{"name":"app","tags":[]}"#))
    );
    let missing = pull_now(&now, "/v2/app/manifests/latest").await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    let target = "/v2/app/manifests/latest";
    let (status, headers, _) = push(&now, Method::PUT, target, MANIFEST.as_bytes()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], MANIFEST_DIGEST);
    let tagged = pull_now(&now, target).await;
    assert_eq!(tagged, (StatusCode::OK, Bytes::from(MANIFEST)));
    let listed = pull_now(&now, "/v2/app/tags/list").await;
    let want = Bytes::from(r#"{"name":"app","tags":["latest"]}"#);
    assert_eq!(listed, (StatusCode::OK, want));

    let front = before_front(&dir);
    let manifest = pull_before(&front, &format!("/v2/app/manifests/{MANIFEST_DIGEST}")).await;
    assert_eq!(manifest, (StatusCode::OK, Bytes::from(MANIFEST)));
    assert_eq!(before.list("oci/blobs/").await.unwrap().len(), 3);
}
