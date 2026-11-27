//! CopyObject and UploadPartCopy through `Server::handle`, over a proxy with in-memory tiers: what
//! the source text may be, what is refused, the source conditions, the copy into a part, a source
//! that changes while it is copied, and the permit a copy holds.

use std::time::Duration;

use bytes::Bytes;
use hyper::StatusCode;
use sha2::{Digest, Sha256};

use crate::server::local_cache::pack_testing::Pack;
use crate::server::local_cache::testing::Rig as CacheRig;
use crate::server::local_cache::trees::{CreateRequest, Location, TreeStore, TreesConfig};
use crate::server::ops_tests::{completion, start};
use crate::server::testing::{BUCKET, NOW, Reply, Rig, texts};
use crate::server::time::iso8601;
use crate::{
    Backend, BlobSource, Error, NewEntry, NewKind, Op, TreeDigest, TreeLimits, build_tree,
};

const MEBIBYTE: u64 = 1 << 20;

/// The time the rig stamps an object with, as a header writes it.
const STAMP: &str = "Fri, 15 Jan 2027 08:00:00 GMT";
const HOUR_BEFORE: &str = "Fri, 15 Jan 2027 07:00:00 GMT";
const HOUR_AFTER: &str = "Fri, 15 Jan 2027 09:00:00 GMT";

/// The tag of an object with these bytes, in quotes, as a head and a copy answer give it.
fn quoted_tag(body: &[u8]) -> String {
    format!("\"{}\"", hex::encode(Sha256::digest(body)))
}

/// A `PUT` of `target` (a key, with its query if it has one) that copies `source`, with the
/// headers `extra` besides.
async fn copy_to(rig: &Rig, target: &str, source: &str, extra: &[(&str, &str)]) -> Reply {
    let mut headers = vec![("x-amz-copy-source", source)];
    headers.extend_from_slice(extra);
    let uri = format!("/{BUCKET}/{target}");
    rig.send("PUT", &uri, &headers, b"").await
}

/// The copy of the object `a` into `target`, with the headers `extra`.
async fn copy_a(rig: &Rig, target: &str, extra: &[(&str, &str)]) -> Reply {
    copy_to(rig, target, &format!("/{BUCKET}/a"), extra).await
}

/// The copy of `source` into the part `number` of the upload `id` of `key`.
async fn copy_part(
    rig: &Rig,
    (key, id, number): (&str, &str, u32),
    source: &str,
    extra: &[(&str, &str)],
) -> Reply {
    let target = format!("{key}?partNumber={number}&uploadId={id}");
    copy_to(rig, &target, source, extra).await
}

/// Asserts the status and the code of an answer.
fn refused(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.body);
    assert_eq!(reply.code(), code, "{}", reply.body);
}

/// Asserts that neither tier holds an object `key`.
async fn nothing_at(rig: &Rig, key: &str) {
    assert!(rig.hot_mem.get(key, None).await.is_err(), "{key} in hot");
    assert!(rig.cold_mem.get(key, None).await.is_err(), "{key} in cold");
}

/// A rig with the object `a` (bytes `abc`) in it.
async fn rig_with_a() -> Rig {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.put("a", b"abc").await.status, StatusCode::OK);
    rig
}

/// Asserts that a copy puts the bytes of its source at the destination as a create-only put does:
/// the answer is a `CopyObjectResult` whose ETag is the source's and that has the time of the
/// copy, the destination reads back as the source, a second copy onto it is 412, and the source
/// may be named with or without the leading slash, with percent-encoded characters, with
/// `versionId=null`, and with the metadata and tagging directives `COPY` and `REPLACE`.
#[tokio::test(start_paused = true)]
async fn a_copy_streams_the_source_into_a_create_only_put() {
    let rig = rig_with_a().await;
    let tag = rig.call("HEAD", "a", b"").await.header("etag").to_owned();
    let done = copy_a(&rig, "b", &[]).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(texts(&done.body, "ETag"), [tag.as_str()]);
    assert_eq!(texts(&done.body, "LastModified"), [iso8601(NOW)]);
    assert_eq!(rig.get("b", None).await.body, "abc");
    assert_eq!(rig.call("HEAD", "b", b"").await.header("etag"), tag);
    // A key that is there is not replaced: the proxy, under this format, creates only.
    let again = copy_a(&rig, "b", &[]).await;
    refused(
        &again,
        StatusCode::PRECONDITION_FAILED,
        "PreconditionFailed",
    );
    assert_eq!(rig.get("b", None).await.body, "abc");

    let plain = copy_to(&rig, "c", "tier-bucket/a", &[]).await;
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.body);
    let null = copy_to(&rig, "d", "/tier-bucket/a?versionId=null", &[]).await;
    assert_eq!(null.status, StatusCode::OK, "{}", null.body);
    let copy = [("x-amz-metadata-directive", "COPY")];
    let replace = [("x-amz-tagging-directive", "REPLACE")];
    assert_eq!(copy_a(&rig, "e", &copy).await.status, StatusCode::OK);
    assert_eq!(copy_a(&rig, "f", &replace).await.status, StatusCode::OK);
    for key in ["c", "d", "e", "f"] {
        assert_eq!(rig.get(key, None).await.body, "abc", "{key}");
    }
}

/// Asserts that a copy names its source and its destination by decoded keys: a source whose key
/// has an encoded space and an encoded non-ASCII character is found, and the destination key is
/// the decoded one.
#[tokio::test(start_paused = true)]
async fn a_copy_decodes_the_keys_of_its_source_and_destination() {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.put("my%20f%C3%A9", b"odd").await.status, StatusCode::OK);
    let done = copy_to(&rig, "to%20here", "/tier-bucket/my%20f%C3%A9", &[]).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(rig.hot_mem.get("to here", None).await.unwrap(), "odd");
    assert_eq!(rig.get("to%20here", None).await.body, "odd");
}

/// Asserts that a copy with `If-None-Match: *` creates the destination and is refused (412) for
/// one that exists, and that a copy of an object onto itself is 400 `InvalidRequest` with S3's
/// text, with or without `x-amz-metadata-directive: REPLACE`, and writes nothing.
#[tokio::test(start_paused = true)]
async fn a_copy_onto_itself_is_invalid_and_if_none_match_star_creates() {
    let rig = rig_with_a().await;
    let star = [("if-none-match", "*")];
    let made = copy_a(&rig, "b", &star).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert_eq!(rig.get("b", None).await.body, "abc");
    let twice = copy_a(&rig, "b", &star).await;
    refused(
        &twice,
        StatusCode::PRECONDITION_FAILED,
        "PreconditionFailed",
    );

    let text = "This copy request is illegal because it is trying to copy an object to itself \
                without changing the object's metadata, storage class, website redirect location \
                or encryption attributes.";
    let replace = [("x-amz-metadata-directive", "REPLACE")];
    for extra in [&[][..], &replace[..]] {
        let own = copy_a(&rig, "a", extra).await;
        refused(&own, StatusCode::BAD_REQUEST, "InvalidRequest");
        assert_eq!(texts(&own.body, "Message"), [text]);
    }
    assert_eq!(rig.get("a", None).await.body, "abc");
}

/// Asserts that a copy of a source that is not there is 404 `NoSuchKey` and writes nothing.
#[tokio::test(start_paused = true)]
async fn a_copy_of_an_absent_source_is_no_such_key() {
    let rig = Rig::new(MEBIBYTE).await;
    let lost = copy_a(&rig, "b", &[]).await;
    refused(&lost, StatusCode::NOT_FOUND, "NoSuchKey");
    nothing_at(&rig, "b").await;
}

/// The copy of `a` into a new key with the headers `extra`, and the status it answered; a copy
/// that was refused wrote nothing.
async fn conditional(rig: &Rig, n: usize, extra: &[(&str, &str)]) -> StatusCode {
    let key = format!("to{n}");
    let reply = copy_a(rig, &key, extra).await;
    if reply.status == StatusCode::OK {
        assert_eq!(rig.get(&key, None).await.body, "abc", "{extra:?}");
    } else {
        refused(
            &reply,
            StatusCode::PRECONDITION_FAILED,
            "PreconditionFailed",
        );
        nothing_at(rig, &key).await;
    }
    reply.status
}

/// Asserts the four source conditions as S3 answers them, for the object `a` stamped 08:00:00:
/// a tag list must name the tag (`*` names any, quotes are optional, the list is comma separated)
/// or must not; `if-match` outweighs `if-unmodified-since` and `if-none-match` outweighs
/// `if-modified-since`; a date condition alone is judged against the time of last write, and a
/// date that is not an HTTP date is ignored. A copy that fails one is 412 and writes nothing.
#[tokio::test(start_paused = true)]
async fn the_source_conditions_are_judged_as_s3_judges_them() {
    let rig = rig_with_a().await;
    let tag = quoted_tag(b"abc");
    let bare = tag.trim_matches('"').to_owned();
    let list = format!("\"x\", {tag} ,\"y\"");
    let other = "\"other\"";
    let (ok, no) = (StatusCode::OK, StatusCode::PRECONDITION_FAILED);
    let cases = [
        (vec![("x-amz-copy-source-if-match", tag.as_str())], ok),
        (vec![("x-amz-copy-source-if-match", bare.as_str())], ok),
        (vec![("x-amz-copy-source-if-match", list.as_str())], ok),
        (vec![("x-amz-copy-source-if-match", "*")], ok),
        (vec![("x-amz-copy-source-if-match", other)], no),
        (vec![("x-amz-copy-source-if-none-match", tag.as_str())], no),
        (vec![("x-amz-copy-source-if-none-match", list.as_str())], no),
        (vec![("x-amz-copy-source-if-none-match", "*")], no),
        (vec![("x-amz-copy-source-if-none-match", other)], ok),
        (
            vec![("x-amz-copy-source-if-unmodified-since", HOUR_BEFORE)],
            no,
        ),
        (vec![("x-amz-copy-source-if-unmodified-since", STAMP)], ok),
        (
            vec![("x-amz-copy-source-if-unmodified-since", HOUR_AFTER)],
            ok,
        ),
        (
            vec![("x-amz-copy-source-if-modified-since", HOUR_BEFORE)],
            ok,
        ),
        (vec![("x-amz-copy-source-if-modified-since", STAMP)], no),
        (
            vec![("x-amz-copy-source-if-modified-since", HOUR_AFTER)],
            no,
        ),
        // Not an HTTP date: ignored.
        (
            vec![("x-amz-copy-source-if-unmodified-since", "yesterday")],
            ok,
        ),
        (
            vec![("x-amz-copy-source-if-modified-since", "yesterday")],
            ok,
        ),
        // `if-match` that holds outweighs a failing `if-unmodified-since`, and one that fails
        // outweighs a holding one.
        (
            vec![
                ("x-amz-copy-source-if-match", tag.as_str()),
                ("x-amz-copy-source-if-unmodified-since", HOUR_BEFORE),
            ],
            ok,
        ),
        (
            vec![
                ("x-amz-copy-source-if-match", other),
                ("x-amz-copy-source-if-unmodified-since", HOUR_AFTER),
            ],
            no,
        ),
        // The same for `if-none-match` and `if-modified-since`.
        (
            vec![
                ("x-amz-copy-source-if-none-match", other),
                ("x-amz-copy-source-if-modified-since", HOUR_AFTER),
            ],
            ok,
        ),
        (
            vec![
                ("x-amz-copy-source-if-none-match", tag.as_str()),
                ("x-amz-copy-source-if-modified-since", HOUR_BEFORE),
            ],
            no,
        ),
        // Both pairs together.
        (
            vec![
                ("x-amz-copy-source-if-match", tag.as_str()),
                ("x-amz-copy-source-if-modified-since", HOUR_BEFORE),
            ],
            ok,
        ),
        (
            vec![
                ("x-amz-copy-source-if-match", tag.as_str()),
                ("x-amz-copy-source-if-none-match", tag.as_str()),
            ],
            no,
        ),
    ];
    for (n, (extra, expected)) in cases.iter().enumerate() {
        assert_eq!(conditional(&rig, n, extra).await, *expected, "{extra:?}");
    }
}

/// Asserts that the date conditions compare to the second: a source written half a second after
/// 08:00:00 is not modified since 08:00:00 and is unmodified since 08:00:00, and is modified since
/// the second before.
#[tokio::test(start_paused = true)]
async fn the_source_dates_are_compared_to_the_second() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.hot_mem.clock().advance(500);
    assert_eq!(rig.put("a", b"abc").await.status, StatusCode::OK);
    let before = "Fri, 15 Jan 2027 07:59:59 GMT";
    let ok = StatusCode::OK;
    let no = StatusCode::PRECONDITION_FAILED;
    let unmodified = "x-amz-copy-source-if-unmodified-since";
    let modified = "x-amz-copy-source-if-modified-since";
    assert_eq!(conditional(&rig, 0, &[(unmodified, STAMP)]).await, ok);
    assert_eq!(conditional(&rig, 1, &[(unmodified, before)]).await, no);
    assert_eq!(conditional(&rig, 2, &[(modified, STAMP)]).await, no);
    assert_eq!(conditional(&rig, 3, &[(modified, before)]).await, ok);
}

/// Asserts what a source text may be: another bucket is 404 `NoSuchBucket`; a text with no key
/// (with or without the slash, or with an empty bucket) is 400 `InvalidArgument`; `versionId=null`
/// is the one version; another version id is 404 `NoSuchVersion`; another parameter is 400
/// `InvalidArgument`; a text that is not percent-encoded UTF-8 is 400 `InvalidURI`.
#[tokio::test(start_paused = true)]
async fn a_source_text_is_parsed_and_refused_as_s3_does() {
    let rig = rig_with_a().await;
    let cases = [
        ("/other/a", StatusCode::NOT_FOUND, "NoSuchBucket"),
        ("/tier-bucket/", StatusCode::BAD_REQUEST, "InvalidArgument"),
        ("tier-bucket", StatusCode::BAD_REQUEST, "InvalidArgument"),
        ("/tier-bucket", StatusCode::BAD_REQUEST, "InvalidArgument"),
        ("//a", StatusCode::BAD_REQUEST, "InvalidArgument"),
        (
            "/tier-bucket/a?versionId=abc",
            StatusCode::NOT_FOUND,
            "NoSuchVersion",
        ),
        (
            "/tier-bucket/a?other=1",
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
        ),
        ("/tier-bucket/%FF", StatusCode::BAD_REQUEST, "InvalidURI"),
    ];
    for (source, status, code) in cases {
        let reply = copy_to(&rig, "b", source, &[]).await;
        refused(&reply, status, code);
        nothing_at(&rig, "b").await;
    }
}

/// Asserts that a key the proxy keeps for itself is refused on either side of a copy with 403
/// `AccessDenied`, and nothing is written.
#[tokio::test(start_paused = true)]
async fn a_reserved_key_is_refused_as_source_and_as_destination() {
    let rig = rig_with_a().await;
    for key in ["_journal/x", "_tiering/x", "_entries/x"] {
        let source = format!("/{BUCKET}/{key}");
        let from = copy_to(&rig, "b", &source, &[]).await;
        refused(&from, StatusCode::FORBIDDEN, "AccessDenied");
        let into = copy_a(&rig, key, &[]).await;
        refused(&into, StatusCode::FORBIDDEN, "AccessDenied");
    }
    nothing_at(&rig, "b").await;
}

/// Asserts that a destination key over 1024 bytes is 400 `KeyTooLongError` and nothing is
/// written, and that a key of exactly 1024 bytes is copied.
#[tokio::test(start_paused = true)]
async fn a_destination_key_over_1024_bytes_is_refused() {
    let rig = rig_with_a().await;
    let long = "k".repeat(1025);
    let over = copy_a(&rig, &long, &[]).await;
    refused(&over, StatusCode::BAD_REQUEST, "KeyTooLongError");
    nothing_at(&rig, &long).await;
    let exact = "k".repeat(1024);
    let done = copy_a(&rig, &exact, &[]).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
}

/// Asserts that a directive other than `COPY` and `REPLACE` is 400 `InvalidArgument` with S3's
/// text, for the metadata directive and for the tagging directive, and nothing is written.
#[tokio::test(start_paused = true)]
async fn an_unknown_directive_is_invalid_argument() {
    let rig = rig_with_a().await;
    let cases = [
        ("x-amz-metadata-directive", "Unknown metadata directive."),
        ("x-amz-tagging-directive", "Unknown tagging directive."),
    ];
    for (name, text) in cases {
        for value in ["MOVE", "copy"] {
            let reply = copy_a(&rig, "b", &[(name, value)]).await;
            refused(&reply, StatusCode::BAD_REQUEST, "InvalidArgument");
            assert_eq!(texts(&reply.body, "Message"), [text], "{name} {value}");
        }
    }
    nothing_at(&rig, "b").await;
}

/// Asserts that a copy with `If-Match` is 501 `NotImplemented`, and so is one with an
/// `If-None-Match` other than `*`, and nothing is written.
#[tokio::test(start_paused = true)]
async fn a_copy_with_a_condition_on_the_destination_other_than_star_is_not_implemented() {
    let rig = rig_with_a().await;
    for extra in [("if-match", "\"x\""), ("if-none-match", "\"x\"")] {
        let reply = copy_a(&rig, "b", &[extra]).await;
        refused(&reply, StatusCode::NOT_IMPLEMENTED, "NotImplemented");
    }
    nothing_at(&rig, "b").await;
}

/// Asserts that `x-amz-copy-source` on a call that is not a `PUT` of an object is 501
/// `NotImplemented`: a get, a head and a delete of an object, a `PUT` and a `GET` of the bucket.
#[tokio::test(start_paused = true)]
async fn a_copy_source_on_a_call_that_is_not_a_put_of_an_object_is_not_implemented() {
    let rig = rig_with_a().await;
    let source = [("x-amz-copy-source", "/tier-bucket/a")];
    let object = format!("/{BUCKET}/a");
    let bucket = format!("/{BUCKET}");
    for (method, uri) in [
        ("GET", &object),
        ("HEAD", &object),
        ("DELETE", &object),
        ("PUT", &bucket),
        ("GET", &bucket),
    ] {
        let reply = rig.send(method, uri, &source, b"").await;
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{method} {uri}");
        if method != "HEAD" {
            assert_eq!(reply.code(), "NotImplemented", "{method} {uri}");
        }
    }
    assert_eq!(rig.get("a", None).await.body, "abc");
}

/// Asserts that a zero-byte source copies to a zero-byte object, and that a range copied into a
/// part of it is refused with the size 0 in S3's text.
#[tokio::test(start_paused = true)]
async fn a_zero_byte_source_copies_to_a_zero_byte_object() {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.put("a", b"").await.status, StatusCode::OK);
    let done = copy_a(&rig, "b", &[]).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(texts(&done.body, "ETag"), [quoted_tag(b"")]);
    let head = rig.call("HEAD", "b", b"").await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-length"), "0");
    assert_eq!(rig.get("b", None).await.body, "");

    let id = start(&rig, "d").await;
    let range = [("x-amz-copy-source-range", "bytes=0-0")];
    let part = copy_part(&rig, ("d", id.as_str(), 1), "/tier-bucket/a", &range).await;
    refused(&part, StatusCode::BAD_REQUEST, "InvalidArgument");
    let text = "Range specified is not valid for source object of size: 0";
    assert_eq!(texts(&part.body, "Message"), [text]);
}

/// Asserts UploadPartCopy: a part copied from a range of the source and another from the whole
/// source answer a `CopyPartResult` with the tag of the bytes copied, a completion that names
/// those tags makes the object of the parts in order, and a range that ends at the last byte of
/// the source is allowed.
#[tokio::test(start_paused = true)]
async fn parts_are_copied_from_a_range_and_from_the_whole_source() {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.put("a", b"0123456789").await.status, StatusCode::OK);
    let id = start(&rig, "d").await;
    let source = "/tier-bucket/a";
    let range = [("x-amz-copy-source-range", "bytes=1-3")];
    let one = copy_part(&rig, ("d", id.as_str(), 1), source, &range).await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.body);
    assert_eq!(texts(&one.body, "ETag"), [quoted_tag(b"123")]);
    assert_eq!(texts(&one.body, "LastModified"), [iso8601(NOW)]);
    let two = copy_part(&rig, ("d", id.as_str(), 2), source, &[]).await;
    assert_eq!(two.status, StatusCode::OK, "{}", two.body);
    assert_eq!(texts(&two.body, "ETag"), [quoted_tag(b"0123456789")]);
    let last = [("x-amz-copy-source-range", "bytes=9-9")];
    let three = copy_part(&rig, ("d", id.as_str(), 3), source, &last).await;
    assert_eq!(texts(&three.body, "ETag"), [quoted_tag(b"9")]);

    let (e1, e2, e3) = (
        texts(&one.body, "ETag").concat(),
        texts(&two.body, "ETag").concat(),
        texts(&three.body, "ETag").concat(),
    );
    let body = completion(&[(1, e1.as_str()), (2, e2.as_str()), (3, e3.as_str())]);
    let uri = format!("d?uploadId={id}");
    let done = rig.call("POST", &uri, body.as_bytes()).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(rig.get("d", None).await.body, "12301234567899");
}

/// Asserts that a part may be copied from the object of the key of its own upload.
#[tokio::test(start_paused = true)]
async fn a_part_may_be_copied_from_the_key_of_its_own_upload() {
    let rig = Rig::new(MEBIBYTE).await;
    let id = start(&rig, "own").await;
    assert_eq!(rig.put("own", b"xyz").await.status, StatusCode::OK);
    let part = copy_part(&rig, ("own", id.as_str(), 1), "/tier-bucket/own", &[]).await;
    assert_eq!(part.status, StatusCode::OK, "{}", part.body);
    assert_eq!(texts(&part.body, "ETag"), [quoted_tag(b"xyz")]);
}

/// Asserts that a copy into a part refuses a range of another form, with `InvalidArgument` and
/// S3's text, whether the source is there or not.
#[tokio::test(start_paused = true)]
async fn a_range_of_another_form_is_invalid_argument() {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.put("a", b"0123456789").await.status, StatusCode::OK);
    let id = start(&rig, "d").await;
    let text = "The x-amz-copy-source-range value must be of the form bytes=first-last where first \
                and last are the zero-based offsets of the first and last bytes to copy";
    let forms = [
        "bytes=3-1",
        "items=0-1",
        "bytes=0-",
        "bytes=-5",
        "bytes=a-b",
        "0-1",
        "",
    ];
    for form in forms {
        let range = [("x-amz-copy-source-range", form)];
        let part = copy_part(&rig, ("d", id.as_str(), 1), "/tier-bucket/a", &range).await;
        refused(&part, StatusCode::BAD_REQUEST, "InvalidArgument");
        assert_eq!(texts(&part.body, "Message"), [text], "{form}");
    }
}

/// Asserts that a range that ends past the source is 400 `InvalidArgument` with S3's text, which
/// names the size of the source.
#[tokio::test(start_paused = true)]
async fn a_range_past_the_end_of_the_source_names_the_size() {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.put("a", b"0123456789").await.status, StatusCode::OK);
    let id = start(&rig, "d").await;
    let text = "Range specified is not valid for source object of size: 10";
    for form in ["bytes=0-10", "bytes=5-500"] {
        let range = [("x-amz-copy-source-range", form)];
        let part = copy_part(&rig, ("d", id.as_str(), 1), "/tier-bucket/a", &range).await;
        refused(&part, StatusCode::BAD_REQUEST, "InvalidArgument");
        assert_eq!(texts(&part.body, "Message"), [text], "{form}");
    }
}

/// Asserts what a copy into a part refuses: an upload that is not there is 404 `NoSuchUpload`, a
/// source that is not there is 404 `NoSuchKey`, a source condition that fails is 412, and a part
/// number of 0, over 10000, not a number, or missing, and a missing upload id, are 400
/// `InvalidArgument`. A source condition that holds is served.
#[tokio::test(start_paused = true)]
async fn a_copy_into_a_part_is_refused_for_what_is_missing_or_fails() {
    let rig = rig_with_a().await;
    let id = start(&rig, "d").await;
    let source = "/tier-bucket/a";
    let unknown = copy_part(&rig, ("d", "nope", 1), source, &[]).await;
    refused(&unknown, StatusCode::NOT_FOUND, "NoSuchUpload");
    let absent = copy_part(&rig, ("d", id.as_str(), 1), "/tier-bucket/missing", &[]).await;
    refused(&absent, StatusCode::NOT_FOUND, "NoSuchKey");
    let failing = [("x-amz-copy-source-if-match", "\"other\"")];
    let failed = copy_part(&rig, ("d", id.as_str(), 1), source, &failing).await;
    refused(
        &failed,
        StatusCode::PRECONDITION_FAILED,
        "PreconditionFailed",
    );
    let holding = [("x-amz-copy-source-if-match", "*")];
    let held = copy_part(&rig, ("d", id.as_str(), 1), source, &holding).await;
    assert_eq!(held.status, StatusCode::OK, "{}", held.body);

    let queries = [
        format!("d?partNumber=0&uploadId={id}"),
        format!("d?partNumber=10001&uploadId={id}"),
        format!("d?partNumber=one&uploadId={id}"),
        format!("d?uploadId={id}"),
        "d?partNumber=1".to_owned(),
    ];
    for query in queries {
        let reply = copy_to(&rig, &query, source, &[]).await;
        refused(&reply, StatusCode::BAD_REQUEST, "InvalidArgument");
    }
}

/// Asserts that a copy whose source read is not the size its head gave (the source was replaced in
/// between: the hot tier lost it and the cold tier holds a longer one) is 503 `SlowDown` with
/// `Retry-After`, nothing is written, and the copy made after it is served.
#[tokio::test(start_paused = true)]
async fn a_source_replaced_before_its_read_is_slow_down_and_writes_nothing() {
    let rig = rig_with_a().await;
    let longer = Bytes::from_static(b"abcdef");
    rig.cold_mem.put("a", longer).await.unwrap();
    rig.hot
        .fail_next_on(Op::Get, "a", Error::NotFound("a".to_owned()));
    let cut = copy_a(&rig, "b", &[]).await;
    refused(&cut, StatusCode::SERVICE_UNAVAILABLE, "SlowDown");
    assert_eq!(cut.header("retry-after"), "1");
    nothing_at(&rig, "b").await;
    let next = copy_a(&rig, "c", &[]).await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.body);
}

/// Asserts that a part copied from a source replaced before its read (see the copy of an object)
/// is 503 `SlowDown`, and that a range of it is judged against the size the head gave.
#[tokio::test(start_paused = true)]
async fn a_part_copy_of_a_replaced_source_is_slow_down() {
    let rig = rig_with_a().await;
    let id = start(&rig, "d").await;
    rig.cold_mem
        .put("a", Bytes::from_static(b"abcdef"))
        .await
        .unwrap();
    rig.hot
        .fail_next_on(Op::Get, "a", Error::NotFound("a".to_owned()));
    let cut = copy_part(&rig, ("d", id.as_str(), 1), "/tier-bucket/a", &[]).await;
    refused(&cut, StatusCode::SERVICE_UNAVAILABLE, "SlowDown");
}

/// Asserts that the source conditions are tested again against the object the read opens: a source
/// replaced by one of the same size and another tag between the head and the read fails
/// `x-amz-copy-source-if-match` with 412, and nothing is written.
#[tokio::test(start_paused = true)]
async fn the_source_conditions_are_tested_again_on_the_object_read() {
    let rig = rig_with_a().await;
    rig.cold_mem
        .put("a", Bytes::from_static(b"xyz"))
        .await
        .unwrap();
    rig.hot
        .fail_next_on(Op::Get, "a", Error::NotFound("a".to_owned()));
    let tag = quoted_tag(b"abc");
    let extra = [("x-amz-copy-source-if-match", tag.as_str())];
    let lost = copy_a(&rig, "b", &extra).await;
    refused(&lost, StatusCode::PRECONDITION_FAILED, "PreconditionFailed");
    nothing_at(&rig, "b").await;
}

/// Asserts that a source that the head found and the read does not find is 404 `NoSuchKey`, for a
/// copy of an object (which writes nothing) and for a copy into a part (the upload is there).
#[tokio::test(start_paused = true)]
async fn a_source_that_vanishes_before_its_read_is_no_such_key() {
    let rig = rig_with_a().await;
    rig.hot
        .fail_next_on(Op::Get, "a", Error::NotFound("a".to_owned()));
    let object = copy_a(&rig, "b", &[]).await;
    refused(&object, StatusCode::NOT_FOUND, "NoSuchKey");
    nothing_at(&rig, "b").await;

    let id = start(&rig, "d").await;
    rig.hot
        .fail_next_on(Op::Get, "a", Error::NotFound("a".to_owned()));
    let part = copy_part(&rig, ("d", id.as_str(), 1), "/tier-bucket/a", &[]).await;
    refused(&part, StatusCode::NOT_FOUND, "NoSuchKey");
}

/// Asserts that a copy holds a permit of the server's puts: with none free and no wait it is 503
/// `SlowDown` with `Retry-After`, and so is one that waits for a permit that is never given; a
/// copy into a part is the same; nothing is written.
#[tokio::test(start_paused = true)]
async fn a_copy_with_no_free_put_permit_is_slow_down() {
    for wait in [Duration::ZERO, Duration::from_secs(15)] {
        let rig = Rig::with(MEBIBYTE, |config| {
            config.max_concurrent_puts = 0;
            config.put_permit_wait = wait;
        })
        .await;
        // The source is put straight in the tier, since the rig's server takes no put.
        rig.hot_mem
            .put("a", Bytes::from_static(b"abc"))
            .await
            .unwrap();
        let busy = copy_a(&rig, "b", &[]).await;
        refused(&busy, StatusCode::SERVICE_UNAVAILABLE, "SlowDown");
        assert_eq!(busy.header("retry-after"), "1");
        nothing_at(&rig, "b").await;

        let id = start(&rig, "d").await;
        let part = copy_part(&rig, ("d", id.as_str(), 1), "/tier-bucket/a", &[]).await;
        refused(&part, StatusCode::SERVICE_UNAVAILABLE, "SlowDown");
    }
}

/// A server with the tree of one file placed at `jobs/1/root/`, and the cache that holds it.
async fn placed_rig() -> (Rig, CacheRig) {
    let cache = CacheRig::new("copy-placed");
    let pack = Pack::put(&cache, "cas/pack/one", &[1000]).await;
    let store = TreeStore::open(cache.cache.clone(), TreesConfig::default()).expect("a store");
    let held = store.clone();
    let rig = Rig::with(MEBIBYTE, move |config| config.trees = Some(held)).await;
    let (sha256, offset, size) = &pack.blobs[0];
    let file = NewEntry {
        path: "file".to_owned(),
        kind: NewKind::File {
            mode: None,
            sha256: TreeDigest::from_hex(sha256).expect("a digest"),
            size: *size,
            from: Some(BlobSource {
                key: "cas/pack/one".into(),
                offset: *offset,
            }),
        },
    };
    let built = build_tree(None, vec![file], &TreeLimits::default()).expect("a tree");
    let at = vec![Location::s3(BUCKET, "jobs/1/root/").expect("a location")];
    let request = CreateRequest {
        built,
        at,
        ttl: None,
        ready: false,
    };
    store.create(request).expect("a placement");
    (rig, cache)
}

/// Asserts that a copy of a file of a placed tree is 501 `NotImplemented`, for an object and for a
/// part, and that a copy of an object into the placement is 409 `TreeLocationReadOnly`; nothing
/// is written.
#[tokio::test]
async fn a_copy_from_a_placed_tree_is_not_implemented_and_into_one_is_read_only() {
    let (rig, _cache) = placed_rig().await;
    assert_eq!(rig.put("a", b"abc").await.status, StatusCode::OK);
    let source = "/tier-bucket/jobs/1/root/file";
    let object = copy_to(&rig, "b", source, &[]).await;
    refused(&object, StatusCode::NOT_IMPLEMENTED, "NotImplemented");
    nothing_at(&rig, "b").await;
    let id = start(&rig, "d").await;
    let part = copy_part(&rig, ("d", id.as_str(), 1), source, &[]).await;
    refused(&part, StatusCode::NOT_IMPLEMENTED, "NotImplemented");

    let into = copy_a(&rig, "jobs/1/root/new", &[]).await;
    refused(&into, StatusCode::CONFLICT, "TreeLocationReadOnly");
    nothing_at(&rig, "jobs/1/root/new").await;
}

/// Asserts that a copy into a part of an upload that was aborted is 404 `NoSuchUpload`.
#[tokio::test(start_paused = true)]
async fn a_part_copy_into_an_aborted_upload_is_no_such_upload() {
    let rig = rig_with_a().await;
    let id = start(&rig, "d").await;
    let uri = format!("d?uploadId={id}");
    assert_eq!(
        rig.call("DELETE", &uri, b"").await.status,
        StatusCode::NO_CONTENT
    );
    let part = copy_part(&rig, ("d", id.as_str(), 1), "/tier-bucket/a", &[]).await;
    refused(&part, StatusCode::NOT_FOUND, "NoSuchUpload");
}
