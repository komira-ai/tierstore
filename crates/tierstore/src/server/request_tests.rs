//! What the server reads of a request: the path, the query, the body, the hashes the body is
//! held to, and the pieces a part is stored from.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use http_body_util::Full;
use hyper::StatusCode;
use hyper::body::{Body, Frame};
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::Error;

use super::chunked::{Decoder, TRAILABLE};
use super::reply::S3Error;
use super::request::{Check, Query, Reader, Refused, Target, header, pieces, query_pairs, resolve};
use super::testing::{CHECKSUMS, Scripted, Step, checksum, crc64nvme};

/// An idle timeout that no test of a body comes near.
const IDLE: Duration = Duration::from_secs(30);

fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let own = |(name, value): &(&str, &str)| (name.to_string(), value.to_string());
    pairs.iter().map(own).collect()
}

/// The headers of a request with these names and values.
fn with(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        let name = name.parse::<HeaderName>().unwrap();
        headers.insert(name, HeaderValue::from_str(value).unwrap());
    }
    headers
}

/// A body that fails when it is read.
struct Broken;

impl Body for Broken {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        Poll::Ready(Some(Err(std::io::Error::other("cut"))))
    }
}

/// The check of a request whose body is not checked at all.
fn unchecked() -> Check {
    Check::new("UNSIGNED-PAYLOAD", &HeaderMap::new(), true).unwrap()
}

/// Feeds `chunks` to the check a request with this claim and these headers makes for a body that
/// is `data` of an object or not, and says whether the whole comes to what the request claims.
fn verdict_of(
    data: bool,
    claim: &str,
    headers: &HeaderMap,
    chunks: &[&[u8]],
) -> Result<(), S3Error> {
    let mut check = Check::new(claim, headers, data)?;
    for chunk in chunks {
        check.update(chunk);
    }
    check.finish()
}

/// The same for the data of an object, which is the body that every checksum header is held to.
fn verdict(claim: &str, headers: &HeaderMap, chunks: &[&[u8]]) -> Result<(), S3Error> {
    verdict_of(true, claim, headers, chunks)
}

/// Asserts that a path names the service, the bucket with or without a closing slash, or an
/// object whose key is the decoded rest of the path.
#[test]
fn a_path_names_the_service_the_bucket_or_an_object() {
    assert_eq!(resolve("bkt", "/").unwrap(), Target::Root);
    assert_eq!(resolve("bkt", "/bkt").unwrap(), Target::Bucket);
    assert_eq!(resolve("bkt", "/bkt/").unwrap(), Target::Bucket);
    let nested = resolve("bkt", "/bkt/a/b").unwrap();
    assert_eq!(nested, Target::Object("a/b".to_owned()));
    let encoded = resolve("bkt", "/bkt/a%2Fb%20c").unwrap();
    assert_eq!(encoded, Target::Object("a/b c".to_owned()));
}

/// Asserts that a path of another bucket is `NoSuchBucket` and a path that is not UTF-8 once
/// decoded is `InvalidURI`.
#[test]
fn a_path_of_another_bucket_or_of_bad_bytes_is_refused() {
    let other = resolve("bkt", "/else/k").unwrap_err();
    assert_eq!(other.kind(), (StatusCode::NOT_FOUND, "NoSuchBucket"));
    assert!(other.message().contains("else"));
    let bad = resolve("bkt", "/bkt/%FF").unwrap_err();
    assert_eq!(bad.kind(), (StatusCode::BAD_REQUEST, "InvalidURI"));
}

/// Asserts that a query is split at `&` and `=`, decoded, and has no pair for an empty piece,
/// and that a name alone has the empty value.
#[test]
fn a_query_is_split_and_decoded() {
    assert_eq!(query_pairs(None), Some(Vec::new()));
    assert_eq!(query_pairs(Some("")), Some(Vec::new()));
    let pairs = query_pairs(Some("a=1&&b=x%2Fy+z&flag&c=%41")).unwrap();
    let want = owned(&[("a", "1"), ("b", "x/y+z"), ("flag", ""), ("c", "A")]);
    assert_eq!(pairs, want);
}

/// Asserts that a query whose name or value is not UTF-8 once decoded is not read.
#[test]
fn a_query_that_is_not_utf8_is_refused() {
    assert_eq!(query_pairs(Some("a=%FF")), None);
    assert_eq!(query_pairs(Some("%FF=1")), None);
    let refused = Query::parse(Some("a=%FF")).unwrap_err();
    assert_eq!(refused.kind(), (StatusCode::BAD_REQUEST, "InvalidURI"));
}

/// Asserts that the first of equal names answers, that a name is found with or without a value,
/// and that the any and only checks say what they are asked.
#[test]
fn a_query_answers_what_it_has() {
    let query = Query::parse(Some("prefix=a%20b&list-type=2&prefix=second")).unwrap();
    assert_eq!(query.get("prefix"), Some("a b"));
    assert_eq!(query.get("absent"), None);
    assert!(query.has("list-type"));
    assert!(!query.has("max-keys"));
    assert!(query.has_any(&["max-keys", "prefix"]));
    assert!(!query.has_any(&["max-keys", "uploads"]));
    assert!(query.only(&["prefix", "list-type"]));
    assert!(!query.only(&["prefix"]));
}

/// Asserts that a header is read as text when it is, and not when it is absent or not text.
#[test]
fn a_header_is_text_or_nothing() {
    let mut headers = HeaderMap::new();
    headers.insert("x-a", HeaderValue::from_static("v"));
    headers.insert("x-b", HeaderValue::from_bytes(b"\xff").unwrap());
    assert_eq!(header(&headers, "x-a"), Some("v"));
    assert_eq!(header(&headers, "x-b"), None);
    assert_eq!(header(&headers, "x-c"), None);
}

/// Asserts that a body is read whole when it has at most the limit, that a longer one is
/// `EntityTooLarge`, and that one that fails is `IncompleteBody`.
#[tokio::test]
async fn a_body_is_read_up_to_its_limit() {
    let four = Full::new(Bytes::from_static(b"abcd"));
    let read = Reader::new(four, 4, IDLE).read_all(unchecked()).await;
    assert_eq!(read.unwrap(), Bytes::from_static(b"abcd"));
    let five = Full::new(Bytes::from_static(b"abcde"));
    let over = Reader::new(five, 4, IDLE).read_all(unchecked()).await;
    let over = over.unwrap_err();
    let too_large = (StatusCode::PAYLOAD_TOO_LARGE, "EntityTooLarge");
    assert_eq!(over.kind(), too_large);
    assert!(over.message().contains("4 bytes"));
    let cut = Reader::new(Broken, 4, IDLE).read_all(unchecked()).await;
    let cut = cut.unwrap_err();
    assert_eq!(cut.kind(), (StatusCode::BAD_REQUEST, "IncompleteBody"));
}

/// Asserts that a body is read a piece at a time, that a frame of trailers carries no piece, and
/// that the end of the body is `None`.
#[tokio::test]
async fn a_body_is_read_piece_by_piece_and_trailers_carry_no_byte() {
    let steps = vec![Step::Data(b"ab"), Step::Trailers, Step::Data(b"cd")];
    let mut reader = Reader::new(Scripted::new(steps), 100, IDLE);
    assert_eq!(
        reader.next().await.unwrap(),
        Some(Bytes::from_static(b"ab"))
    );
    assert_eq!(
        reader.next().await.unwrap(),
        Some(Bytes::from_static(b"cd"))
    );
    assert_eq!(reader.next().await.unwrap(), None);
}

/// Asserts that a body that sends no piece for the idle time is cut at that time with
/// `RequestTimeout`, and that the pieces it sent before are not lost.
#[tokio::test(start_paused = true)]
async fn a_body_with_no_piece_for_the_idle_time_is_cut() {
    let hour = Duration::from_secs(3600);
    let steps = vec![Step::Data(b"a"), Step::Wait(hour), Step::Data(b"b")];
    let mut reader = Reader::new(Scripted::new(steps), 100, Duration::from_secs(10));
    assert_eq!(reader.next().await.unwrap(), Some(Bytes::from_static(b"a")));
    let started = Instant::now();
    let cut = reader.next().await.unwrap_err();
    assert_eq!(cut.kind(), (StatusCode::BAD_REQUEST, "RequestTimeout"));
    assert!(cut.is_timeout());
    let waited = started.elapsed();
    assert!(waited >= Duration::from_secs(10), "{waited:?}");
    assert!(waited < Duration::from_secs(11), "{waited:?}");
}

/// Asserts that a body that is slow but steady is read whole: every wait is shorter than the
/// idle time and the whole takes longer than it.
#[tokio::test(start_paused = true)]
async fn a_body_that_is_slow_but_steady_is_read_whole() {
    let wait = Step::Wait(Duration::from_secs(9));
    let steps = vec![
        wait,
        Step::Data(b"a"),
        Step::Wait(Duration::from_secs(9)),
        Step::Data(b"b"),
        Step::Wait(Duration::from_secs(9)),
        Step::Data(b"c"),
    ];
    let reader = Reader::new(Scripted::new(steps), 100, Duration::from_secs(10));
    let started = Instant::now();
    let read = reader.read_all(unchecked()).await.unwrap();
    assert_eq!(read, Bytes::from_static(b"abc"));
    assert!(started.elapsed() >= Duration::from_secs(27));
}

/// Asserts that a body is checked against the hash the signature covers, in either case and
/// however it is cut into pieces, that an unsigned payload is not checked, that another hash and
/// a claim that is not a hash are `XAmzContentSHA256Mismatch`, and that a signed stream is
/// `NotImplemented` for a body that is not the data of an object or of a part.
#[test]
fn a_body_is_checked_against_the_hash_the_signature_covers() {
    let none = HeaderMap::new();
    let digest = hex::encode(Sha256::digest(b"abc"));
    assert!(verdict("UNSIGNED-PAYLOAD", &none, &[b"anything"]).is_ok());
    assert!(verdict(&digest, &none, &[b"a", b"bc"]).is_ok());
    assert!(verdict(&digest.to_uppercase(), &none, &[b"abc"]).is_ok());
    let mismatch = (StatusCode::BAD_REQUEST, "XAmzContentSHA256Mismatch");
    let other = hex::encode(Sha256::digest(b"abd"));
    let differs = verdict(&other, &none, &[b"abc"]).unwrap_err();
    assert_eq!(differs.kind(), mismatch);
    let no_hash = verdict("not hex", &none, &[b"abc"]).unwrap_err();
    assert_eq!(no_hash.kind(), mismatch);
    let claim = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
    let streaming = verdict_of(false, claim, &none, &[b"abc"]).unwrap_err();
    let not_done = (StatusCode::NOT_IMPLEMENTED, "NotImplemented");
    assert_eq!(streaming.kind(), not_done);
}

/// Asserts that a body that comes to the checksum a header claims is accepted, for each header
/// (`Content-MD5`, CRC-32, CRC-32C, SHA-1, SHA-256, CRC-64/NVMe) and for all of them at once, in whatever
/// pieces the body comes, and that a body that does not is `BadDigest` naming the header.
#[test]
fn each_checksum_header_is_held_to() {
    let body: &[u8] = b"hello world";
    let chunks: [&[u8]; 3] = [b"hello", b" wor", b"ld"];
    let bad = (StatusCode::BAD_REQUEST, "BadDigest");
    for name in CHECKSUMS {
        let good = with(&[(name, &checksum(name, body))]);
        assert!(
            verdict("UNSIGNED-PAYLOAD", &good, &chunks).is_ok(),
            "{name}"
        );
        let wrong = with(&[(name, &checksum(name, b"hello worle"))]);
        let refused = verdict("UNSIGNED-PAYLOAD", &wrong, &chunks).unwrap_err();
        assert_eq!(refused.kind(), bad, "{name}");
        assert!(refused.message().contains(name), "{name}");
    }
    let all: Vec<(String, String)> = CHECKSUMS
        .iter()
        .map(|name| ((*name).to_owned(), checksum(name, body)))
        .collect();
    let pairs: Vec<(&str, &str)> = all.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    assert!(verdict("UNSIGNED-PAYLOAD", &with(&pairs), &chunks).is_ok());
    let claim = hex::encode(Sha256::digest(body));
    assert!(verdict(&claim, &with(&pairs), &chunks).is_ok());
    assert!(verdict(&claim, &with(&pairs), &[b"hello"]).is_err());
}

/// Asserts that a checksum header whose value is not the base64 of a hash of its length is
/// `InvalidDigest`: not base64, empty, the wrong length, and not text.
#[test]
fn a_checksum_header_that_is_not_a_hash_is_invalid() {
    let invalid = (StatusCode::BAD_REQUEST, "InvalidDigest");
    for name in CHECKSUMS {
        for value in ["!!!", "", "AAAA"] {
            let headers = with(&[(name, value)]);
            let refused = verdict("UNSIGNED-PAYLOAD", &headers, &[b"x"]).unwrap_err();
            assert_eq!(refused.kind(), invalid, "{name} {value:?}");
            assert!(refused.message().contains(name), "{name}");
        }
        let mut headers = HeaderMap::new();
        let name = name.parse::<HeaderName>().unwrap();
        headers.insert(name, HeaderValue::from_bytes(b"\xff").unwrap());
        let refused = verdict("UNSIGNED-PAYLOAD", &headers, &[b"x"]).unwrap_err();
        assert_eq!(refused.kind(), invalid);
    }
}

/// Asserts that the CRC-64/NVMe checksum is the standard one: the check value of the algorithm
/// for `123456789` is `0xae8b14860a799888`, and the reference of the tests agrees with it; the
/// base64 of its eight big-endian bytes is accepted for that body in any pieces and refused,
/// as `BadDigest`, for another. It catches a polynomial, an initial value or a byte order that
/// is not the one S3 uses.
#[test]
fn the_crc64nvme_checksum_is_the_standard_one() {
    assert_eq!(crc64nvme(b"123456789"), 0xae8b_1486_0a79_9888);
    let name = "x-amz-checksum-crc64nvme";
    let headers = with(&[(name, "rosUhgp5mIg=")]);
    assert!(verdict("UNSIGNED-PAYLOAD", &headers, &[b"123456789"]).is_ok());
    assert!(verdict("UNSIGNED-PAYLOAD", &headers, &[b"1234", b"", b"56789"]).is_ok());
    let refused = verdict("UNSIGNED-PAYLOAD", &headers, &[b"123456780"]).unwrap_err();
    assert_eq!(refused.kind(), (StatusCode::BAD_REQUEST, "BadDigest"));
    assert!(refused.message().contains(name));
}

/// Asserts that the SHA-1 checksum is the standard one: the base64 of the SHA-1 of `abc`, which
/// is `a9993e36...d89d`, is accepted for a body of `abc` in any pieces and refused for another.
#[test]
fn the_sha1_checksum_is_the_standard_one() {
    let name = "x-amz-checksum-sha1";
    let headers = with(&[(name, "qZk+NkcGgWq6PiVxeFDCbJzQ2J0=")]);
    assert!(verdict("UNSIGNED-PAYLOAD", &headers, &[b"abc"]).is_ok());
    assert!(verdict("UNSIGNED-PAYLOAD", &headers, &[b"a", b"b", b"c"]).is_ok());
    let refused = verdict("UNSIGNED-PAYLOAD", &headers, &[b"abd"]).unwrap_err();
    assert_eq!(refused.kind(), (StatusCode::BAD_REQUEST, "BadDigest"));
}

/// Asserts that a body that is not the data of an object (the XML of a completion) is held to
/// the hash the signature covers and to `Content-MD5` only: a checksum header that is wrong,
/// malformed or of an algorithm that is not supported is not read, a `Content-MD5` that does not
/// match is `BadDigest` and one that does is accepted, and a signed hash that does not match is
/// `XAmzContentSHA256Mismatch`.
#[test]
fn a_body_that_is_not_data_is_held_only_to_its_signature_and_content_md5() {
    let body: &[u8] = b"<xml/>";
    let chunks: [&[u8]; 2] = [b"<xml", b"/>"];
    for name in CHECKSUMS.into_iter().skip(1) {
        let wrong = with(&[(name, &checksum(name, b"another body"))]);
        assert!(verdict_of(false, "UNSIGNED-PAYLOAD", &wrong, &chunks).is_ok());
        let malformed = with(&[(name, "!!!")]);
        assert!(verdict_of(false, "UNSIGNED-PAYLOAD", &malformed, &chunks).is_ok());
    }
    let name = "content-md5";
    let wrong = with(&[(name, &checksum(name, b"another body"))]);
    let refused = verdict_of(false, "UNSIGNED-PAYLOAD", &wrong, &chunks).unwrap_err();
    assert_eq!(refused.kind(), (StatusCode::BAD_REQUEST, "BadDigest"));
    let right = with(&[(name, &checksum(name, body))]);
    assert!(verdict_of(false, "UNSIGNED-PAYLOAD", &right, &chunks).is_ok());
    let other = hex::encode(Sha256::digest(b"another body"));
    let differs = verdict_of(false, &other, &HeaderMap::new(), &chunks).unwrap_err();
    let mismatch = (StatusCode::BAD_REQUEST, "XAmzContentSHA256Mismatch");
    assert_eq!(differs.kind(), mismatch);
}

/// Collects the items of a stream of pieces until it ends.
async fn collected(stream: crate::PartBody<'_>) -> Vec<Result<Bytes, Error>> {
    stream.collect().await
}

/// Asserts that the pieces of a body whose checksum matches are a stream of those pieces that
/// ends without an error, and that nothing is left for the answer.
#[tokio::test]
async fn the_pieces_of_a_good_body_are_a_stream_that_ends() {
    let name = "x-amz-checksum-crc32";
    let headers = with(&[(name, &checksum(name, b"abcd"))]);
    let check = Check::new("UNSIGNED-PAYLOAD", &headers, true).unwrap();
    let body = Scripted::new(vec![Step::Data(b"ab"), Step::Data(b"cd")]);
    let refused = Refused::default();
    let stream = pieces(Reader::new(body, 100, IDLE), check, refused.clone());
    let items = collected(stream).await;
    let want = vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))];
    assert_eq!(items, want);
    assert!(refused.take().is_none());
}

/// Asserts that a body whose checksum does not match ends its stream with an error after the
/// pieces, once, and leaves the answer to send (`BadDigest`) to be taken, once.
#[tokio::test]
async fn a_body_that_does_not_match_ends_its_stream_with_an_error() {
    let name = "content-md5";
    let headers = with(&[(name, &checksum(name, b"other"))]);
    let check = Check::new("UNSIGNED-PAYLOAD", &headers, true).unwrap();
    let body = Scripted::new(vec![Step::Data(b"abc")]);
    let refused = Refused::default();
    let stream = pieces(Reader::new(body, 100, IDLE), check, refused.clone());
    let items = collected(stream).await;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], Ok(Bytes::from_static(b"abc")));
    assert!(matches!(items[1], Err(Error::Other(_))));
    let answer = refused.take().unwrap();
    assert_eq!(answer.kind(), (StatusCode::BAD_REQUEST, "BadDigest"));
    assert!(refused.take().is_none());
}

/// Asserts that a body that stalls ends its stream with an error after the pieces that came, and
/// that a body over the limit ends its stream with an error: the answers are `RequestTimeout`
/// and `EntityTooLarge`.
#[tokio::test(start_paused = true)]
async fn a_body_that_stalls_or_is_too_large_ends_its_stream_with_an_error() {
    let hour = Duration::from_secs(3600);
    let steps = vec![Step::Data(b"a"), Step::Wait(hour)];
    let refused = Refused::default();
    let reader = Reader::new(Scripted::new(steps), 100, Duration::from_secs(5));
    let items = collected(pieces(reader, unchecked(), refused.clone())).await;
    assert_eq!(items.len(), 2);
    assert!(matches!(items[1], Err(Error::Other(_))));
    let answer = refused.take().unwrap();
    assert_eq!(answer.kind(), (StatusCode::BAD_REQUEST, "RequestTimeout"));
    let big = Full::new(Bytes::from_static(b"abcde"));
    let reader = Reader::new(big, 4, IDLE);
    let items = collected(pieces(reader, unchecked(), refused.clone())).await;
    assert_eq!(items.len(), 1);
    let answer = refused.take().unwrap();
    let too_large = (StatusCode::PAYLOAD_TOO_LARGE, "EntityTooLarge");
    assert_eq!(answer.kind(), too_large);
}

/// The head of an unsigned stream with a trailing checksum `name` of `length` bytes of data.
fn trailer_head(name: &str, length: usize) -> HeaderMap {
    let length = length.to_string();
    with(&[
        ("x-amz-decoded-content-length", &length),
        ("x-amz-trailer", name),
    ])
}

/// The body of an unsigned stream of `data` in two chunks with the trailer `name:value`.
fn trailer_body(data: &[u8], name: &str, value: &str) -> Scripted {
    let (first, second) = data.split_at(data.len() / 2);
    let mut wire = Vec::new();
    for part in [first, second] {
        wire.extend(format!("{:x}\r\n", part.len()).bytes());
        wire.extend_from_slice(part);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend(format!("0\r\n{name}:{value}\r\n\r\n").bytes());
    let wire: &'static [u8] = Box::leak(wire.into_boxed_slice());
    let (head, tail) = wire.split_at(wire.len() / 3);
    Scripted::new(vec![Step::Data(head), Step::Data(tail)])
}

/// A reader of `body` that decodes it as the framing of `check` says.
fn decoding(body: Scripted, check: &Check) -> Reader<Scripted> {
    let decoder = Decoder::new(check.framing().unwrap(), None, 1024);
    Reader::new(body, 1000, IDLE).with_decoder(Some(decoder))
}

/// Asserts that the trailing checksum of an `aws-chunked` body is held to the decoded data, for
/// each algorithm S3 takes in a trailer: a body whose trailer is the data's checksum is read as
/// the data and nothing else, one whose trailer is another's is `BadDigest` naming the header,
/// and one whose trailer is not a hash of the right length is `InvalidDigest`.
#[tokio::test]
async fn a_trailing_checksum_is_held_to_the_data_of_the_body() {
    let data: &[u8] = b"hello world, hello world";
    let claim = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";
    for name in TRAILABLE {
        let head = trailer_head(name, data.len());
        let check = Check::new(claim, &head, true).unwrap();
        assert_eq!(
            check.framing().map(|f| f.decoded()),
            Some(data.len() as u64)
        );
        let good = trailer_body(data, name, &checksum(name, data));
        let read = decoding(good, &check).read_all(check).await;
        assert_eq!(read.unwrap(), data, "{name}");

        let check = Check::new(claim, &head, true).unwrap();
        let wrong = trailer_body(data, name, &checksum(name, b"another body"));
        let refused = decoding(wrong, &check).read_all(check).await.unwrap_err();
        assert_eq!(
            refused.kind(),
            (StatusCode::BAD_REQUEST, "BadDigest"),
            "{name}"
        );
        assert!(refused.message().contains(name), "{name}");

        let check = Check::new(claim, &head, true).unwrap();
        let short = trailer_body(data, name, "AAAA");
        let refused = decoding(short, &check).read_all(check).await.unwrap_err();
        assert_eq!(
            refused.kind(),
            (StatusCode::BAD_REQUEST, "InvalidDigest"),
            "{name}"
        );
    }
}

/// Asserts that the pieces of an `aws-chunked` body are the stream of its data, without the
/// framing and the trailer, which ends cleanly when the trailer is the data's checksum, and ends
/// with an error and the answer `BadDigest` when it is not.
#[tokio::test]
async fn the_pieces_of_an_aws_chunked_body_are_its_data() {
    let data: &[u8] = b"hello world, hello world";
    let name = "x-amz-checksum-crc64nvme";
    let claim = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";
    let head = trailer_head(name, data.len());
    let check = Check::new(claim, &head, true).unwrap();
    let body = trailer_body(data, name, &checksum(name, data));
    let refused = Refused::default();
    let items = collected(pieces(decoding(body, &check), check, refused.clone())).await;
    let got: Vec<u8> = items
        .iter()
        .flat_map(|piece| piece.clone().unwrap())
        .collect();
    assert_eq!(got, data);
    assert!(refused.take().is_none());

    let check = Check::new(claim, &head, true).unwrap();
    let body = trailer_body(data, name, &checksum(name, b"another body"));
    let items = collected(pieces(decoding(body, &check), check, refused.clone())).await;
    assert!(matches!(items.last(), Some(Err(Error::Other(_)))));
    let answer = refused.take().unwrap();
    assert_eq!(answer.kind(), (StatusCode::BAD_REQUEST, "BadDigest"));
}

/// Asserts that a reader that has given the end of an `aws-chunked` body goes on giving the end,
/// and keeps the trailer it read.
#[tokio::test]
async fn a_reader_at_the_end_of_an_aws_chunked_body_stays_there() {
    let name = "x-amz-checksum-crc32";
    let claim = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";
    let check = Check::new(claim, &trailer_head(name, 4), true).unwrap();
    let value = checksum(name, b"abcd");
    let mut reader = decoding(trailer_body(b"abcd", name, &value), &check);
    assert_eq!(reader.trailer(), None);
    while reader.next().await.unwrap().is_some() {}
    assert_eq!(reader.trailer(), Some(value.as_str()));
    assert_eq!(reader.next().await.unwrap(), None);
    assert_eq!(reader.trailer(), Some(value.as_str()));
}

/// Asserts that a streaming payload on a call that takes no data is `NotImplemented`, that one
/// without the length of its data is `MissingContentLength`, and that a payload that is not
/// streamed has no framing.
#[test]
fn a_streamed_body_needs_a_length_and_a_call_that_takes_data() {
    let claim = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
    let head = trailer_head("x-amz-checksum-crc32", 1);
    let refused = Check::new(claim, &head, false).err().unwrap();
    let not_done = (StatusCode::NOT_IMPLEMENTED, "NotImplemented");
    assert_eq!(refused.kind(), not_done);
    let refused = Check::new(claim, &HeaderMap::new(), true).err().unwrap();
    let required = (StatusCode::LENGTH_REQUIRED, "MissingContentLength");
    assert_eq!(refused.kind(), required);
    assert!(unchecked().framing().is_none());
}

/// Asserts that a request names the content tags its body would have from `Content-MD5` (the hex
/// of the MD5) and `x-amz-checksum-sha256` (the hex of the SHA-256) and from no other checksum
/// header, and that a request with none names none.
#[test]
fn the_claim_of_a_request_is_the_hex_of_its_md5_and_sha256() {
    let md5 = checksum("content-md5", b"abcd");
    let sha = checksum("x-amz-checksum-sha256", b"abcd");
    let crc = checksum("x-amz-checksum-crc32", b"abcd");
    let headers = with(&[
        ("content-md5", &md5),
        ("x-amz-checksum-sha256", &sha),
        ("x-amz-checksum-crc32", &crc),
    ]);
    let claim = Check::new("UNSIGNED-PAYLOAD", &headers, true)
        .unwrap()
        .claim();
    let tags = vec![
        "e2fc714c4727ee9395f324cd2e7f331f".to_owned(),
        "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589".to_owned(),
    ];
    assert_eq!(claim.tags, tags);
    assert!(unchecked().claim().tags.is_empty());
}
