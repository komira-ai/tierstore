//! ListParts (`GET /<bucket>/<key>?uploadId=`) through `Server::handle`, over a proxy whose tiers
//! are in memory. An upload is the cold tier's, so the parts are listed from there.

use hyper::StatusCode;

use crate::Op;

use super::ops_tests::{start, upload};
use super::testing::{BUCKET, Reply, Rig, texts};

/// Lists the parts of the upload `id` of `key` (`query` is what follows `uploadId=<id>&`).
async fn list(rig: &Rig, key: &str, id: &str, query: &str) -> Reply {
    let uri = format!("{key}?uploadId={id}&{query}");
    rig.call("GET", &uri, b"").await
}

/// The part numbers of a listing, in the order it gives them.
fn numbers(reply: &Reply) -> Vec<String> {
    texts(&reply.body, "PartNumber")
}

/// An upload of `key` with the parts 3, 1 and 2 stored in that order (`ccc`, `a`, `bb`), and the
/// ETag each answered, by number.
async fn stocked(rig: &Rig, key: &str) -> (String, [String; 3]) {
    let id = start(rig, key).await;
    let mut tags = Vec::new();
    for (number, body) in [(3, "ccc"), (1, "a"), (2, "bb")] {
        let stored = upload(rig, key, &id, number, body.as_bytes()).await;
        assert_eq!(stored.status, StatusCode::OK);
        tags.push((number, stored.header("etag").to_owned()));
    }
    tags.sort();
    let [one, two, three]: [(u32, String); 3] = tags.try_into().unwrap();
    (id, [one.1, two.1, three.1])
}

/// Asserts the answer for an upload whose parts were stored out of order: `ListPartsResult` with
/// the `Bucket`, `Key` and `UploadId`; the parts in ascending order of number, each with its
/// `ETag` (equal to the `ETag` its UploadPart answered, quotes included), `Size` and
/// `LastModified` (the time of the clock of the tier that holds the upload); the `Initiator`, the `Owner` and the
/// `StorageClass`; and `PartNumberMarker` 0, `MaxParts` 1000, `IsTruncated` false and no
/// `NextPartNumberMarker`.
#[tokio::test(start_paused = true)]
async fn the_parts_of_an_upload_are_listed_in_ascending_order_with_their_etags() {
    let rig = Rig::new(1 << 20).await;
    let (id, tags) = stocked(&rig, "obj/key").await;
    let reply = list(&rig, "obj/key", &id, "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.header("content-type"), "application/xml");
    assert!(reply.body.contains("<ListPartsResult"), "{}", reply.body);
    assert_eq!(texts(&reply.body, "Bucket"), [BUCKET]);
    assert_eq!(texts(&reply.body, "Key"), ["obj/key"]);
    assert_eq!(texts(&reply.body, "UploadId"), [id.as_str()]);
    assert_eq!(numbers(&reply), ["1", "2", "3"]);
    assert_eq!(texts(&reply.body, "ETag"), tags);
    assert_eq!(texts(&reply.body, "Size"), ["1", "2", "3"]);
    let stored = vec!["2027-01-15T08:00:00.000Z"; 3];
    assert_eq!(texts(&reply.body, "LastModified"), stored);
    assert_eq!(texts(&reply.body, "PartNumberMarker"), ["0"]);
    assert_eq!(texts(&reply.body, "MaxParts"), ["1000"]);
    assert_eq!(texts(&reply.body, "IsTruncated"), ["false"]);
    assert!(texts(&reply.body, "NextPartNumberMarker").is_empty());
    assert_eq!(texts(&reply.body, "StorageClass"), ["STANDARD"]);
    assert_eq!(texts(&reply.body, "ID"), ["tierstore"; 2]);
    assert_eq!(texts(&reply.body, "DisplayName"), ["tierstore"; 2]);
    // The upload was made with room in the hot tier, so its id names that tier and it is asked.
    assert_eq!(rig.hot.calls(Op::ListParts), 1);
    assert_eq!(rig.cold.calls(Op::ListParts), 0);
}

/// Asserts that a part sent again with a number replaces the earlier one: the listing has the
/// number once, with the size and the `ETag` of the second.
#[tokio::test(start_paused = true)]
async fn a_part_stored_again_is_listed_once_with_its_new_etag() {
    let rig = Rig::new(1 << 20).await;
    let (id, tags) = stocked(&rig, "k").await;
    let again = upload(&rig, "k", &id, 2, b"zzzz").await;
    let reply = list(&rig, "k", &id, "").await;
    assert_eq!(numbers(&reply), ["1", "2", "3"]);
    assert_eq!(texts(&reply.body, "Size"), ["1", "4", "3"]);
    let etags = texts(&reply.body, "ETag");
    assert_eq!(etags[1], again.header("etag"));
    assert_ne!(etags[1], tags[1]);
}

/// Asserts the paging by `max-parts` and `part-number-marker`: with `max-parts=1` the first page
/// is part 1, truncated, with `NextPartNumberMarker` 1 and `MaxParts` 1; the marker given back
/// gives part 2 and then part 3, the last not truncated and with no next marker; the marker is
/// echoed as `PartNumberMarker`; following the markers lists every part exactly once; a marker
/// past the last part gives an empty page.
#[tokio::test(start_paused = true)]
async fn the_parts_are_paged_by_max_parts_and_the_part_number_marker() {
    let rig = Rig::new(1 << 20).await;
    let (id, _) = stocked(&rig, "k").await;
    let first = list(&rig, "k", &id, "max-parts=1").await;
    assert_eq!(numbers(&first), ["1"]);
    assert_eq!(texts(&first.body, "IsTruncated"), ["true"]);
    assert_eq!(texts(&first.body, "NextPartNumberMarker"), ["1"]);
    assert_eq!(texts(&first.body, "MaxParts"), ["1"]);

    let mut seen = numbers(&first);
    let mut marker = "1".to_owned();
    loop {
        let page = list(
            &rig,
            "k",
            &id,
            &format!("max-parts=1&part-number-marker={marker}"),
        )
        .await;
        assert_eq!(texts(&page.body, "PartNumberMarker"), [marker.as_str()]);
        seen.extend(numbers(&page));
        match texts(&page.body, "NextPartNumberMarker").first() {
            Some(next) => marker = next.clone(),
            None => {
                assert_eq!(texts(&page.body, "IsTruncated"), ["false"]);
                break;
            }
        }
    }
    assert_eq!(seen, ["1", "2", "3"]);

    let two = list(&rig, "k", &id, "max-parts=2&part-number-marker=1").await;
    assert_eq!(numbers(&two), ["2", "3"]);
    assert_eq!(texts(&two.body, "IsTruncated"), ["false"]);
    let past = list(&rig, "k", &id, "part-number-marker=3").await;
    assert_eq!(past.status, StatusCode::OK);
    assert!(numbers(&past).is_empty());
    assert_eq!(texts(&past.body, "IsTruncated"), ["false"]);
}

/// Asserts that `max-parts=0` is an empty, untruncated page (as `max-keys=0` is), that a size or a
/// marker that is not a whole number from 0 to 2147483647 is 400 `InvalidArgument`, and that a
/// size above 1000 is 1000.
#[tokio::test(start_paused = true)]
async fn max_parts_and_the_marker_are_held_to_the_rules_of_a_listing() {
    let rig = Rig::new(1 << 20).await;
    let (id, _) = stocked(&rig, "k").await;
    let zero = list(&rig, "k", &id, "max-parts=0").await;
    assert_eq!(zero.status, StatusCode::OK);
    assert!(numbers(&zero).is_empty());
    assert_eq!(texts(&zero.body, "MaxParts"), ["0"]);
    assert_eq!(texts(&zero.body, "IsTruncated"), ["false"]);
    let large = list(&rig, "k", &id, "max-parts=5000").await;
    assert_eq!(texts(&large.body, "MaxParts"), ["1000"]);
    assert_eq!(numbers(&large).len(), 3);
    for query in [
        "max-parts=-1",
        "max-parts=many",
        "part-number-marker=-1",
        "part-number-marker=one",
        "part-number-marker=2147483648",
    ] {
        let bad = list(&rig, "k", &id, query).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(bad.code(), "InvalidArgument", "{query}");
    }
}

/// Asserts that an upload that is not open is 404 `NoSuchUpload`: an id never given, the id of an
/// upload of another key, an upload that was aborted, and one that was completed; also with
/// `max-parts=0`.
#[tokio::test(start_paused = true)]
async fn an_upload_that_is_not_open_is_no_such_upload() {
    let rig = Rig::new(1 << 20).await;
    let (id, tags) = stocked(&rig, "k").await;
    let other = start(&rig, "other").await;
    for (key, id, query) in [
        ("k", "no-such-id", ""),
        ("k", other.as_str(), ""),
        ("other", id.as_str(), ""),
        ("k", "no-such-id", "max-parts=0"),
    ] {
        let reply = list(&rig, key, id, query).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{key} {id}");
        assert_eq!(reply.code(), "NoSuchUpload", "{key} {id}");
    }
    let aborted = rig.call("DELETE", &format!("k?uploadId={id}"), b"").await;
    assert_eq!(aborted.status, StatusCode::NO_CONTENT);
    let after_abort = list(&rig, "k", &id, "").await;
    assert_eq!(after_abort.code(), "NoSuchUpload");

    let made = start(&rig, "made").await;
    let stored = upload(&rig, "made", &made, 1, b"x").await;
    let parts = [(1, stored.header("etag"))];
    let body = super::ops_tests::completion(&parts);
    let done = rig
        .call("POST", &format!("made?uploadId={made}"), body.as_bytes())
        .await;
    assert_eq!(done.status, StatusCode::OK, "{} {tags:?}", done.body);
    let after_complete = list(&rig, "made", &made, "").await;
    assert_eq!(after_complete.code(), "NoSuchUpload");
}

/// Asserts that a key of the proxy's own is refused with `AccessDenied` before the backend is
/// asked, and that a key with `&` is escaped in the answer.
#[tokio::test(start_paused = true)]
async fn the_keys_of_the_proxy_are_refused_and_a_key_is_escaped() {
    let rig = Rig::new(1 << 20).await;
    for key in ["_journal/x", "_tiering/x", "_entries/x"] {
        let reply = list(&rig, key, "u", "").await;
        assert_eq!(reply.code(), "AccessDenied", "{key}");
    }
    assert_eq!(rig.hot.calls(Op::ListParts) + rig.cold.calls(Op::ListParts), 0);
    let id = start(&rig, "a&b").await;
    upload(&rig, "a&b", &id, 1, b"x").await;
    let reply = list(&rig, "a%26b", &id, "").await;
    assert!(reply.body.contains("<Key>a&amp;b</Key>"), "{}", reply.body);
    assert_eq!(numbers(&reply), ["1"]);
}

/// Asserts that a failure of the tier that holds the upload (the hot tier, which has room here) is
/// the answer of the listing (503 for an unavailable tier), and that the listing is a read: it
/// changes no part.
#[tokio::test(start_paused = true)]
async fn a_failure_of_the_tier_that_holds_the_upload_is_the_answer() {
    let rig = Rig::new(1 << 20).await;
    let (id, _) = stocked(&rig, "k").await;
    rig.hot.fail_next(Op::ListParts, 1);
    let down = list(&rig, "k", &id, "").await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    let up = list(&rig, "k", &id, "").await;
    assert_eq!(numbers(&up), ["1", "2", "3"]);
    let again = list(&rig, "k", &id, "").await;
    assert_eq!(again.body, up.body);
}

/// Asserts that a read-only key may list the parts of an upload, as it may list the uploads,
/// and that a listing needs no more than a signed GET: the same call without the upload id is
/// the GetObject it always was.
#[tokio::test(start_paused = true)]
async fn a_read_only_key_lists_the_parts() {
    let rig = Rig::with(1 << 20, |config| {
        config.read_only_keys = vec![super::keys::AccessKey::new("ro", "ro-secret-0123456789")];
    })
    .await;
    let (id, _) = stocked(&rig, "k").await;
    let uri = format!("/{BUCKET}/k?uploadId={id}");
    let reply = rig
        .send_as(("ro", "ro-secret-0123456789"), "GET", &uri, &[], b"")
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(numbers(&reply), ["1", "2", "3"]);
}
