//! The `response-*` query parameters of a GetObject and a HeadObject, which set headers of the
//! answer, signed in the header or in a presigned URL, and refused on a request that is not
//! signed, as S3 does.

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};

use super::AccessKey;
use super::presign::tests::request;
use super::presign::{presign_get, presign_url};
use super::testing::{BUCKET, Reply, Rig, texts};

const PARAMS: [(&str, &str); 6] = [
    ("response-cache-control", "cache-control"),
    ("response-content-disposition", "content-disposition"),
    ("response-content-encoding", "content-encoding"),
    ("response-content-language", "content-language"),
    ("response-content-type", "content-type"),
    ("response-expires", "expires"),
];

const VALUES: [&str; 6] = [
    "no-store, max-age=0",
    "attachment; filename=\"r\u{e9}sum\u{e9}.txt\"",
    "gzip",
    "en-GB",
    "text/plain; charset=utf-8",
    "Thu, 01 Dec 2033 16:00:00 GMT",
];

/// The percent-encoded value of a query parameter, as an SDK sends it.
fn encoded(value: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

/// The query of a read that sets the six headers.
fn all_overrides() -> String {
    let pairs = PARAMS.iter().zip(VALUES);
    let pairs: Vec<String> = pairs
        .map(|((param, _), value)| format!("{param}={}", encoded(value)))
        .collect();
    pairs.join("&")
}

async fn rig_with_object() -> Rig {
    let rig = Rig::new(1 << 20).await;
    assert_eq!(rig.put("k", b"0123456789").await.status, StatusCode::OK);
    rig
}

fn url_request(method: &str, url: &str) -> Request<Full<Bytes>> {
    let rest = url.split_once("://").unwrap().1;
    let at = rest.find('/').unwrap();
    Request::builder()
        .method(method)
        .uri(&rest[at..])
        .header("host", &rest[..at])
        .body(Full::new(Bytes::new()))
        .unwrap()
}

fn message(reply: &Reply) -> String {
    texts(&reply.body, "Message").concat()
}

/// Asserts that a signed GET, a signed HEAD and a signed ranged GET carry each of the six
/// overrides in the header it names, in place of the object's own (the content type is
/// `application/octet-stream` without one), with the value exactly as sent, a non-ASCII byte
/// included; and that the body, the length and the tag are the object's.
#[tokio::test(start_paused = true)]
async fn the_six_overrides_set_their_headers_on_a_signed_get_and_head() {
    let rig = rig_with_object().await;
    let plain = rig.get("k", None).await;
    assert_eq!(plain.header("content-type"), "application/octet-stream");
    assert_eq!(plain.header("content-disposition"), "");
    let uri = format!("/{BUCKET}/k?{}", all_overrides());
    for (method, range, status) in [
        ("GET", None, StatusCode::OK),
        ("HEAD", None, StatusCode::OK),
        ("GET", Some("bytes=2-4"), StatusCode::PARTIAL_CONTENT),
    ] {
        let extra: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
        let reply = rig.send(method, &uri, &extra, b"").await;
        assert_eq!(reply.status, status, "{method} {range:?}");
        for ((param, header), value) in PARAMS.iter().zip(VALUES) {
            let sent = reply.headers.get(*header).unwrap();
            assert_eq!(sent.as_bytes(), value.as_bytes(), "{method} {param}");
        }
        assert_eq!(reply.header("etag"), plain.header("etag"), "{method}");
        assert_eq!(reply.header("last-modified"), plain.header("last-modified"));
    }
    let ranged = rig.send("GET", &uri, &[("range", "bytes=2-4")], b"").await;
    assert_eq!(ranged.body, "234");
    assert_eq!(ranged.header("content-range"), "bytes 2-4/10");
    let whole = rig.send("GET", &uri, &[], b"").await;
    assert_eq!(whole.body, "0123456789");
}

/// Asserts that one override leaves the other headers alone, that an override with an empty
/// value is not applied, and that the first of two of the same name wins.
#[tokio::test(start_paused = true)]
async fn one_override_leaves_the_rest_and_an_empty_one_is_not_applied() {
    let rig = rig_with_object().await;
    let one = rig
        .send(
            "GET",
            &format!("/{BUCKET}/k?response-content-type=text%2Fcsv"),
            &[],
            b"",
        )
        .await;
    assert_eq!(one.header("content-type"), "text/csv");
    assert_eq!(one.header("content-disposition"), "");
    assert_eq!(one.header("accept-ranges"), "bytes");
    let empty = rig
        .send(
            "GET",
            &format!("/{BUCKET}/k?response-content-type="),
            &[],
            b"",
        )
        .await;
    assert_eq!(empty.header("content-type"), "application/octet-stream");
    let twice = format!("/{BUCKET}/k?response-content-type=a%2Fb&response-content-type=c%2Fd");
    let first = rig.send("GET", &twice, &[], b"").await;
    assert_eq!(first.header("content-type"), "a/b");
}

/// Asserts that an override is applied only to an answer that carries the object: a 304 from a
/// condition has the object's own headers and none of the overrides, a range that cannot be
/// satisfied is the 416 error, and a key that is not there is the 404 error with its XML body.
#[tokio::test(start_paused = true)]
async fn an_override_is_not_applied_to_a_304_or_to_an_error() {
    let rig = rig_with_object().await;
    let uri = format!("/{BUCKET}/k?response-content-type=text%2Fcsv");
    let tag = rig.get("k", None).await.header("etag").to_owned();
    let same = rig.send("GET", &uri, &[("if-none-match", &tag)], b"").await;
    assert_eq!(same.status, StatusCode::NOT_MODIFIED);
    assert_eq!(same.header("content-type"), "application/octet-stream");
    let range = rig
        .send("GET", &uri, &[("range", "bytes=50-60")], b"")
        .await;
    assert_eq!(range.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_ne!(range.header("content-type"), "text/csv");
    let missing = format!("/{BUCKET}/none?response-content-type=text%2Fcsv");
    let missing = rig.send("GET", &missing, &[], b"").await;
    assert_eq!(missing.code(), "NoSuchKey");
    assert_eq!(missing.header("content-type"), "application/xml");
}

/// Asserts that a value which is no header value (a line feed, a NUL) is refused with 400
/// `InvalidArgument` naming the parameter, before the object is read, so no header is split.
#[tokio::test(start_paused = true)]
async fn an_override_that_is_no_header_value_is_refused() {
    let rig = rig_with_object().await;
    for (param, bad) in [
        ("response-content-disposition", "a%0D%0Ax-injected%3A%201"),
        ("response-content-type", "a%00b"),
    ] {
        let uri = format!("/{BUCKET}/k?{param}={bad}");
        let reply = rig.send("GET", &uri, &[], b"").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{param}");
        assert_eq!(reply.code(), "InvalidArgument", "{param}");
        assert_eq!(
            message(&reply),
            format!("{param} is not a valid header value")
        );
        assert!(reply.headers.get("x-injected").is_none());
    }
}

/// Asserts what a presigned GET and HEAD URL does with the overrides signed into it, for the
/// full key and a read-only key: the answer carries the headers; a URL with an override added
/// after it was signed is `SignatureDoesNotMatch`; and a URL with a parameter that is neither
/// SigV4, `x-id` nor an override is still 403 for a key that reads.
#[tokio::test(start_paused = true)]
async fn a_presigned_get_carries_the_overrides_signed_into_it() {
    let read_only = AccessKey::new("AKIDREADER", "reader-secret");
    let rig = Rig::with(1 << 20, |config| {
        config.read_only_keys = vec![AccessKey::new("AKIDREADER", "reader-secret")];
    })
    .await;
    assert_eq!(rig.put("cas/a", b"0123456789").await.status, StatusCode::OK);
    let full = AccessKey::new(super::testing::KEY_ID, super::testing::SECRET);
    let extra: Vec<(&str, &str)> = PARAMS.iter().map(|(p, _)| *p).zip(VALUES).collect();
    for key in [&full, &read_only] {
        for method in ["GET", "HEAD"] {
            let url = presign_url(method, key, &request("cas/a", 300), &extra).unwrap();
            let reply = rig.send_raw(url_request(method, &url)).await;
            assert_eq!(reply.status, StatusCode::OK, "{} {method}", key.id);
            for ((param, header), value) in PARAMS.iter().zip(VALUES) {
                let sent = reply.headers.get(*header).unwrap();
                assert_eq!(sent.as_bytes(), value.as_bytes(), "{method} {param}");
            }
        }
        let plain = presign_get(key, &request("cas/a", 300)).unwrap();
        let added = format!("{plain}&response-content-type={}", encoded("text/html"));
        let tampered = rig.send_raw(url_request("GET", &added)).await;
        assert_eq!(tampered.status, StatusCode::FORBIDDEN, "{}", key.id);
        assert_eq!(tampered.code(), "SignatureDoesNotMatch", "{}", key.id);
        let unknown = [("versionId", "1")];
        let url = presign_url("GET", key, &request("cas/a", 300), &unknown).unwrap();
        let reply = rig.send_raw(url_request("GET", &url)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", key.id);
        assert_eq!(reply.code(), "AccessDenied", "{}", key.id);
    }
}

/// Asserts that a request with no signature (no `Authorization` header and no SigV4 query) that
/// carries an override is 400 `InvalidRequest` with S3's message, for GET and HEAD, whether or
/// not the object exists; that the same request without the override is 403 `AccessDenied`
/// as before; and that a request with a signature in the query is not taken for anonymous.
#[tokio::test(start_paused = true)]
async fn an_unsigned_request_with_an_override_is_a_bad_request() {
    let rig = rig_with_object().await;
    for method in ["GET", "HEAD"] {
        for key in ["k", "absent"] {
            let uri = format!("/{BUCKET}/{key}?response-content-type=text%2Fplain");
            let bare = Request::builder()
                .method(method)
                .uri(&uri)
                .header("host", "tier.test");
            let reply = rig
                .send_raw(bare.body(Full::new(Bytes::new())).unwrap())
                .await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{method} {key}");
            if method == "GET" {
                assert_eq!(reply.code(), "InvalidRequest", "{key}");
                let wanted = "Request specific response headers cannot be used for anonymous \
                              GET requests.";
                assert_eq!(message(&reply), wanted, "{key}");
            }
        }
        let uri = format!("/{BUCKET}/k");
        let bare = Request::builder()
            .method(method)
            .uri(&uri)
            .header("host", "tier.test");
        let reply = rig
            .send_raw(bare.body(Full::new(Bytes::new())).unwrap())
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method}");
    }
    let bad = Request::builder()
        .method("GET")
        .uri(format!(
            "/{BUCKET}/k?X-Amz-Algorithm=AWS4-HMAC-SHA256&response-content-type=a"
        ))
        .header("host", "tier.test");
    let reply = rig
        .send_raw(bad.body(Full::new(Bytes::new())).unwrap())
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}
