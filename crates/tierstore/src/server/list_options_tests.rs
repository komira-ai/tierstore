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

/// Asserts that `fetch-owner=true` gives each object an `Owner` with an `ID` and a
/// `DisplayName`, as S3 does, and that `fetch-owner=false` and no `fetch-owner` list the same
/// keys without one.
#[tokio::test(start_paused = true)]
async fn fetch_owner_adds_an_owner_to_each_object() {
    let rig = stocked().await;
    let asked = rig.list("fetch-owner=true").await;
    assert_eq!(asked.status, StatusCode::OK);
    assert_eq!(texts(&asked.body, "Key"), ["a/1", "a/2", "b"]);
    assert_eq!(texts(&asked.body, "ID"), ["tierstore"; 3]);
    assert_eq!(texts(&asked.body, "DisplayName"), ["tierstore"; 3]);
    for query in ["fetch-owner=false", ""] {
        let reply = rig.list(query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}");
        assert_eq!(texts(&reply.body, "Key"), ["a/1", "a/2", "b"], "{query}");
        assert!(texts(&reply.body, "Owner").is_empty(), "{query}");
    }
}

/// Asserts that `max-keys=0` is an empty, untruncated page with `KeyCount` 0 and `MaxKeys` 0
/// (as S3 answers it, measured on MinIO), though keys are there, and that `max-uploads=0` is the
/// same for the uploads. A size that is negative, not a number or above 2147483647 is 400
/// `InvalidArgument` with the words S3 uses.
#[tokio::test(start_paused = true)]
async fn a_page_size_of_zero_is_an_empty_page() {
    let rig = stocked().await;
    let zero = rig.list("max-keys=0").await;
    assert_eq!(zero.status, StatusCode::OK);
    assert!(texts(&zero.body, "Key").is_empty());
    assert_eq!(texts(&zero.body, "KeyCount"), ["0"]);
    assert_eq!(texts(&zero.body, "MaxKeys"), ["0"]);
    assert_eq!(texts(&zero.body, "IsTruncated"), ["false"]);
    assert!(texts(&zero.body, "NextContinuationToken").is_empty());
    let path = format!("/{BUCKET}?uploads&max-uploads=0");
    let uploads = rig.send("GET", &path, &[], b"").await;
    assert_eq!(uploads.status, StatusCode::OK);
    assert_eq!(texts(&uploads.body, "MaxUploads"), ["0"]);
    assert_eq!(texts(&uploads.body, "IsTruncated"), ["false"]);
    for bad in ["-1", "many", "2147483648"] {
        let reply = rig.list(&format!("max-keys={bad}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(reply.code(), "InvalidArgument", "{bad}");
        assert_eq!(
            texts(&reply.body, "Message"),
            ["Argument maxKeys must be an integer between 0 and 2147483647"],
            "{bad}"
        );
        let path = format!("/{BUCKET}?uploads&max-uploads={bad}");
        let reply = rig.send("GET", &path, &[], b"").await;
        let message = "Argument max-uploads must be an integer between 0 and 2147483647";
        assert_eq!(texts(&reply.body, "Message"), [message], "{bad}");
    }
}
