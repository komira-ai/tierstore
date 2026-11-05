//! `delimiter` and `encoding-type=url` of ListMultipartUploads, through `Server::handle`, over a
//! proxy whose tiers are in memory. S3 serves both (its reference: keys that share the text
//! between the prefix and the first delimiter are "grouped under a single result element,
//! `CommonPrefixes`"; `Delimiter`, `KeyMarker`, `Prefix`, `NextKeyMarker` and `Key` are encoded).

use hyper::StatusCode;

use crate::Backend;

use super::ops_tests::start;
use super::testing::{BUCKET, Reply, Rig, texts};

async fn rig_with(keys: &[&str]) -> Rig {
    let rig = Rig::new(1 << 20).await;
    for key in keys {
        start(&rig, key).await;
    }
    rig
}

async fn list(rig: &Rig, query: &str) -> Reply {
    let uri = format!("/{BUCKET}?uploads&{query}");
    rig.send("GET", &uri, &[], b"").await
}

/// The `Prefix` of each `CommonPrefixes` of an answer.
fn prefixes(reply: &Reply) -> Vec<String> {
    let doc = roxmltree::Document::parse(&reply.body).expect("well-formed XML");
    doc.descendants()
        .filter(|node| node.tag_name().name() == "CommonPrefixes")
        .flat_map(|node| node.children())
        .filter(|node| node.tag_name().name() == "Prefix")
        .map(|node| node.text().unwrap_or_default().to_owned())
        .collect()
}

fn keys(reply: &Reply) -> Vec<String> {
    texts(&reply.body, "Key")
}

fn truncated(reply: &Reply) -> bool {
    texts(&reply.body, "IsTruncated") == ["true"]
}

/// Asserts that uploads whose keys have the delimiter after the prefix fold into one common
/// prefix each, written after the `Upload`s, with `Delimiter` echoed: over `a/1`, `a/2`, `b/x/1`
/// and `top`, a `/` gives the upload `top` and the prefixes `a/` and `b/`; under `prefix=b/` the
/// key `b/x/1` folds to `b/x/`; under `prefix=a/` nothing folds; an empty delimiter is none.
#[tokio::test(start_paused = true)]
async fn uploads_with_the_delimiter_fold_into_common_prefixes() {
    let rig = rig_with(&["a/1", "a/2", "b/x/1", "top"]).await;
    let reply = list(&rig, "delimiter=%2F").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(keys(&reply), ["top"]);
    assert_eq!(prefixes(&reply), ["a/", "b/"]);
    assert_eq!(texts(&reply.body, "Delimiter"), ["/"]);
    assert!(!truncated(&reply));
    assert!(texts(&reply.body, "NextKeyMarker").is_empty());
    let under_b = list(&rig, "delimiter=%2F&prefix=b%2F").await;
    assert_eq!(prefixes(&under_b), ["b/x/"]);
    assert!(keys(&under_b).is_empty());
    let under_a = list(&rig, "delimiter=%2F&prefix=a%2F").await;
    assert!(prefixes(&under_a).is_empty());
    assert_eq!(keys(&under_a), ["a/1", "a/2"]);
    let none = list(&rig, "delimiter=").await;
    assert_eq!(keys(&none), ["a/1", "a/2", "b/x/1", "top"]);
    assert!(texts(&none.body, "Delimiter").is_empty());
}

/// Asserts that a common prefix counts against `max-uploads` as an upload does, and that a page
/// that ends with one names the prefix as `NextKeyMarker` with an empty `NextUploadIdMarker`:
/// over `a/1`, `a/2`, `b/1` and `c`, `max-uploads=1` gives `a/`, `b/` and the upload `c` on three
/// pages, each following the marker of the one before, and the last is not truncated.
#[tokio::test(start_paused = true)]
async fn max_uploads_pages_across_common_prefixes() {
    let rig = rig_with(&["a/1", "a/2", "b/1", "c"]).await;
    let first = list(&rig, "delimiter=%2F&max-uploads=1").await;
    assert_eq!(prefixes(&first), ["a/"]);
    assert!(truncated(&first));
    assert_eq!(texts(&first.body, "NextKeyMarker"), ["a/"]);
    assert_eq!(texts(&first.body, "NextUploadIdMarker"), [""]);
    let second = list(&rig, "delimiter=%2F&max-uploads=1&key-marker=a%2F").await;
    assert_eq!(prefixes(&second), ["b/"]);
    assert!(truncated(&second));
    assert_eq!(texts(&second.body, "KeyMarker"), ["a/"]);
    assert_eq!(texts(&second.body, "NextKeyMarker"), ["b/"]);
    let third = list(&rig, "delimiter=%2F&max-uploads=1&key-marker=b%2F").await;
    assert!(prefixes(&third).is_empty());
    assert_eq!(keys(&third), ["c"]);
    assert!(!truncated(&third));
    let both = list(&rig, "delimiter=%2F&max-uploads=3").await;
    assert_eq!(prefixes(&both), ["a/", "b/"]);
    assert_eq!(keys(&both), ["c"]);
    assert!(!truncated(&both));
}

/// Asserts what the S3 reference says of the listing of uploads with a delimiter: "`CommonPrefixes`
/// is filtered out from results if it is not lexicographically greater than the key-marker". A
/// `key-marker` inside `a/` (with or without an upload id) leaves `a/` out, and so every upload
/// under it; without a delimiter the same marker is a plain bound on keys.
#[tokio::test(start_paused = true)]
async fn a_key_marker_inside_a_prefix_leaves_the_prefix_out() {
    let rig = rig_with(&["a/1", "a/2", "b/1", "c"]).await;
    for marker in [
        "key-marker=a%2F1",
        "key-marker=a%2F1&upload-id-marker=0",
        "key-marker=a%2F",
    ] {
        let reply = list(&rig, &format!("delimiter=%2F&{marker}")).await;
        assert_eq!(prefixes(&reply), ["b/"], "{marker}");
        assert_eq!(keys(&reply), ["c"], "{marker}");
    }
    let before = list(&rig, "delimiter=%2F&key-marker=a").await;
    assert_eq!(prefixes(&before), ["a/", "b/"]);
    let outside = list(&rig, "delimiter=%2F&prefix=b%2F&key-marker=a%2F1").await;
    assert_eq!(keys(&outside), ["b/1"]);
    let plain = list(&rig, "key-marker=a%2F1").await;
    assert_eq!(keys(&plain), ["a/2", "b/1", "c"]);
}

/// Asserts that a prefix of more uploads than one read of the backend holds is crossed: 1100
/// uploads of keys under `a/` and one of `b`, with `max-uploads=1`, give `a/` on the first page
/// and, after its marker, the upload `b`. It catches a walk that stops reading after the first
/// batch and so takes the end of the batch for the end of the listing.
#[tokio::test(start_paused = true)]
async fn a_prefix_of_many_uploads_is_one_entry() {
    let rig = Rig::new(1 << 20).await;
    for number in 0..1100 {
        rig.cold_mem
            .create_upload(&format!("a/{number:04}"))
            .await
            .unwrap();
    }
    rig.cold_mem.create_upload("b").await.unwrap();
    let first = list(&rig, "delimiter=%2F&max-uploads=1").await;
    assert_eq!(prefixes(&first), ["a/"]);
    assert!(truncated(&first));
    let second = list(&rig, "delimiter=%2F&max-uploads=1&key-marker=a%2F").await;
    assert_eq!(keys(&second), ["b"]);
    assert!(!truncated(&second));
}

/// Asserts what `encoding-type=url` does to a listing of uploads: `Key`, `Prefix`, `Delimiter`,
/// `KeyMarker`, `NextKeyMarker` and the `Prefix` of a common prefix are percent-encoded (a space
/// is `+`), `EncodingType` is `url`, and the upload id is not touched. Without it, or with it
/// empty, nothing is encoded, and any other value is 400 `InvalidArgument`.
#[tokio::test(start_paused = true)]
async fn encoding_type_url_encodes_the_names_of_an_uploads_listing() {
    let rig = rig_with(&["d%20e/f", "d%20e/g", "h%2Bi"]).await;
    let query = "encoding-type=url&delimiter=%2F&max-uploads=1";
    let first = list(&rig, query).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(texts(&first.body, "EncodingType"), ["url"]);
    assert_eq!(prefixes(&first), ["d+e/"]);
    assert_eq!(texts(&first.body, "NextKeyMarker"), ["d+e/"]);
    let second = list(&rig, &format!("{query}&key-marker=d%20e%2F")).await;
    assert_eq!(texts(&second.body, "KeyMarker"), ["d+e/"]);
    assert_eq!(keys(&second), ["h%2Bi"]);
    let all = list(&rig, "encoding-type=url&prefix=d%20e").await;
    assert_eq!(texts(&all.body, "Prefix"), ["d+e"]);
    assert_eq!(keys(&all), ["d+e/f", "d+e/g"]);
    let ids = texts(&all.body, "UploadId");
    assert!(
        ids.iter().all(|id| !id.contains('%') && !id.contains('+')),
        "{ids:?}"
    );
    let plain = list(&rig, "prefix=d%20e").await;
    assert_eq!(keys(&plain), ["d e/f", "d e/g"]);
    assert!(texts(&plain.body, "EncodingType").is_empty());
    let empty = list(&rig, "encoding-type=&prefix=d%20e").await;
    assert_eq!(keys(&empty), keys(&plain));
    for value in ["base64", "URL"] {
        let reply = list(&rig, &format!("encoding-type={value}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(reply.code(), "InvalidArgument", "{value}");
        let message = "Invalid Encoding Method specified in Request";
        assert_eq!(texts(&reply.body, "Message"), [message], "{value}");
    }
}
