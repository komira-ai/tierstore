//! What the server answers today to a signed POST that names a hydrate or release path, which
//! this release does not know.

use hyper::StatusCode;

use super::testing::{BUCKET, Rig};

const MEBIBYTE: u64 = 1 << 20;

const BODY: &[u8] = br#"{"hold":"h","expires_ms":1000,"blobs":[]}"#;

/// Asserts that a signed POST of a hydrate or release body to a path under the bucket (the form
/// `/<bucket>/_tierstore/v1/hydrate`) is answered 501 `NotImplemented`, whatever the body: the
/// answer a client of a later release reads as "unsupported now". A change that gives the path
/// another answer (404, 403, a handler) fails here and so says that it changed this contract.
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

/// Asserts that the same POST to `/_tierstore/v1/hydrate` and `/_tierstore/v1/release`, outside
/// any bucket (the form the design names), is answered 404 `NoSuchBucket` today, not 501: the
/// path is read as a bucket named `_tierstore`. A release that adds the call changes this answer
/// on purpose and changes this test with it.
#[tokio::test]
async fn an_unknown_path_outside_the_bucket_answers_404_no_such_bucket() {
    let rig = Rig::new(MEBIBYTE).await;
    for call in ["hydrate", "release"] {
        let path = format!("/_tierstore/v1/{call}");
        let reply = rig.send("POST", &path, &[], BODY).await;
        assert_eq!(
            (reply.status, reply.code()),
            (StatusCode::NOT_FOUND, "NoSuchBucket".to_owned()),
            "{path}"
        );
    }
}
