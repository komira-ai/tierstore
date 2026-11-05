//! The options of a listing that S3 clients send whether or not they mean anything by them,
//! through `Server::handle`, over a proxy whose tiers are in memory.

use hyper::StatusCode;

use super::testing::{BUCKET, Rig, texts};

async fn stocked() -> Rig {
    let rig = Rig::new(1 << 20).await;
    for key in ["a/1", "a/2", "b"] {
        assert_eq!(rig.put(key, b"x").await.status, StatusCode::OK, "{key}");
    }
    rig
}

/// Asserts that an empty `delimiter` is no delimiter, as it is for S3: a listing with
/// `delimiter=` answers the same keys, count and truncation as a listing without it, and
/// ListMultipartUploads accepts it too.
#[tokio::test(start_paused = true)]
async fn an_empty_delimiter_lists_as_if_there_were_none() {
    let rig = stocked().await;
    let plain = rig.list("").await;
    let empty = rig.list("delimiter=").await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_eq!(texts(&empty.body, "Key"), ["a/1", "a/2", "b"]);
    assert_eq!(texts(&empty.body, "Key"), texts(&plain.body, "Key"));
    assert_eq!(texts(&empty.body, "KeyCount"), ["3"]);
    assert_eq!(texts(&empty.body, "IsTruncated"), ["false"]);
    let path = format!("/{BUCKET}?uploads&delimiter=");
    let uploads = rig.send("GET", &path, &[], b"").await;
    assert_eq!(uploads.status, StatusCode::OK);
}

/// Asserts that `fetch-owner` is accepted and ignored with either value: the listing is the
/// same as without it and has no `Owner`.
#[tokio::test(start_paused = true)]
async fn fetch_owner_is_accepted_and_changes_nothing() {
    let rig = stocked().await;
    for query in ["fetch-owner=true", "fetch-owner=false"] {
        let reply = rig.list(query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}");
        assert_eq!(texts(&reply.body, "Key"), ["a/1", "a/2", "b"], "{query}");
        assert!(texts(&reply.body, "Owner").is_empty(), "{query}");
    }
}
