//! What the server answers to a signed POST that names a path of a call this release no longer
//! has: the paths `/_tierstore/v1/hydrate` and `/_tierstore/v1/release`.

use hyper::StatusCode;

use super::local_cache::testing::Rig as CacheRig;
use super::testing::{BUCKET, Rig};

const MEBIBYTE: u64 = 1 << 20;

const BODY: &[u8] = br#"{"hold":"h","expires_ms":1000,"blobs":[]}"#;

/// Asserts that a signed POST of a body to a path under the bucket (the form
/// `/<bucket>/_tierstore/v1/hydrate`) is answered 501 `NotImplemented`, whatever the body. A change
/// that gives the path another answer (404, 403, a handler) fails here and so says that it changed
/// this contract.
#[tokio::test]
async fn an_unknown_path_under_the_bucket_answers_501_not_implemented() {
    let rig = Rig::new(MEBIBYTE).await;
    for call in ["hydrate", "release"] {
        let path = format!("/{BUCKET}/_tierstore/v1/{call}");
        let reply = rig.send("POST", &path, &[], BODY).await;
        assert_eq!(
            (reply.status, reply.code()),
            (StatusCode::NOT_IMPLEMENTED, "NotImplemented".to_owned()),
            "{path}"
        );
    }
}

/// A server in front of a local cache named `name`, which takes writes if `writes`, with the
/// rig of the cache, which keeps its directory for as long as the test holds it.
async fn cached(name: &str, writes: bool) -> (Rig, CacheRig) {
    let cache = CacheRig::new(name);
    let held = cache.cache.clone();
    let rig = Rig::with(MEBIBYTE, move |config| {
        config.cache = Some(held);
        config.cache_writes = writes;
    })
    .await;
    (rig, cache)
}

/// Asserts that on a node whose cache takes writes (a storage node with a cache in front of its
/// proxy), the signed POST of `/_tierstore/v1/hydrate` and of `/_tierstore/v1/release` is read as
/// a request for a bucket named `_tierstore` and answers 404 `NoSuchBucket`, as any other path
/// outside the bucket does: the paths are no longer calls.
#[tokio::test]
async fn the_former_call_paths_outside_the_bucket_answer_404_no_such_bucket() {
    let (rig, _cache) = cached("retired-paths-writes", true).await;
    for path in [
        "/_tierstore/v1/hydrate",
        "/_tierstore/v1/release",
        "/_tierstore/v1/other",
    ] {
        let reply = rig.send("POST", path, &[], BODY).await;
        assert_eq!(
            (reply.status, reply.code()),
            (StatusCode::NOT_FOUND, "NoSuchBucket".to_owned()),
            "POST {path}"
        );
    }
}

/// Asserts that on a build node (a cache that takes no writes), the same signed POSTs are refused
/// as every write is, 403 `AccessDenied`, before the path is read: the paths are no longer calls
/// there either, and a POST of any other path outside the bucket gets the same answer.
#[tokio::test]
async fn the_former_call_paths_on_a_build_node_are_refused_as_any_write() {
    let (rig, _cache) = cached("retired-paths-reads", false).await;
    for path in [
        "/_tierstore/v1/hydrate",
        "/_tierstore/v1/release",
        "/_tierstore/v1/other",
    ] {
        let reply = rig.send("POST", path, &[], BODY).await;
        assert_eq!(
            (reply.status, reply.code()),
            (StatusCode::FORBIDDEN, "AccessDenied".to_owned()),
            "POST {path}"
        );
    }
}
