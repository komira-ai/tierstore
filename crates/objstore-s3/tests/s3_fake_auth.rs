//! The loopback S3 service of `s3::fake` when it requires credentials: a request that is
//! not signed as the store signs is refused `403 SignatureDoesNotMatch`, whatever way it
//! falls short, and does nothing.

use objstore_s3::s3::fake::FakeS3;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DATE: &str = "20260901T000000Z";
const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Sends one request with `headers` (and `body`) and returns the status line and the
/// response body.
async fn send(s3: &FakeS3, method: &str, headers: &[(&str, &str)], body: &str) -> (String, String) {
    let addr = s3.endpoint().trim_start_matches("http://").to_owned();
    let mut sock = TcpStream::connect(&addr).await.unwrap();
    let mut request = format!("{method} /kbf-fake/obj HTTP/1.1\r\nhost: {addr}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("content-length: {}\r\n\r\n{body}", body.len()));
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_end, length) = loop {
        let n = sock.read(&mut chunk).await.unwrap();
        assert!(n > 0, "closed before the answer");
        got.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&got).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            let length = text[..end]
                .lines()
                .find_map(|l| l.strip_prefix("content-length: "))
                .map_or(0, |v| v.parse::<usize>().unwrap());
            break (end + 4, length);
        }
    };
    while got.len() < head_end + length {
        let n = sock.read(&mut chunk).await.unwrap();
        assert!(n > 0, "closed before the end of the body");
        got.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&got).into_owned();
    let status = text.lines().next().unwrap().to_owned();
    (status, text[head_end..].to_owned())
}

async fn refused(s3: &FakeS3, headers: &[(&str, &str)]) {
    let (status, body) = send(s3, "PUT", headers, "payload").await;
    assert_eq!(status, "HTTP/1.1 403 Forbidden");
    assert!(
        body.contains("<Code>SignatureDoesNotMatch</Code>"),
        "{body}"
    );
    assert!(s3.keys().is_empty(), "the refused write stored something");
}

/// Asserts that a request with no `Authorization` header is refused 403
/// `SignatureDoesNotMatch` and stores nothing.
#[tokio::test]
async fn a_request_with_no_authorization_is_refused() {
    let s3 = FakeS3::start().await;
    s3.require_credentials("fake-key", "fake-secret");
    refused(
        &s3,
        &[("x-amz-date", DATE), ("x-amz-content-sha256", EMPTY_SHA)],
    )
    .await;
}

/// Asserts that an `Authorization` header that names no credential scope (so no region)
/// is refused, as is one with a scope but no `SignedHeaders` list.
#[tokio::test]
async fn an_authorization_it_cannot_parse_is_refused() {
    let s3 = FakeS3::start().await;
    s3.require_credentials("fake-key", "fake-secret");
    let dated = [("x-amz-date", DATE), ("x-amz-content-sha256", EMPTY_SHA)];
    let with = |authorization: &'static str| {
        let mut headers = vec![("authorization", authorization)];
        headers.extend(dated);
        headers
    };
    refused(&s3, &with("garbage")).await;
    refused(
        &s3,
        &with("AWS4-HMAC-SHA256 Credential=fake-key/20260901/us-east-1/s3/aws4_request"),
    )
    .await;
}

/// Asserts that a well-formed `Authorization` is refused when the request lacks a header
/// the signature needs: `x-amz-date`, or `x-amz-content-sha256`.
#[tokio::test]
async fn a_request_missing_a_signing_header_is_refused() {
    let s3 = FakeS3::start().await;
    s3.require_credentials("fake-key", "fake-secret");
    let authorization = "AWS4-HMAC-SHA256 \
        Credential=fake-key/20260901/us-east-1/s3/aws4_request, \
        SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=00";
    refused(
        &s3,
        &[
            ("authorization", authorization),
            ("x-amz-content-sha256", EMPTY_SHA),
        ],
    )
    .await;
    refused(
        &s3,
        &[("authorization", authorization), ("x-amz-date", DATE)],
    )
    .await;
}

/// Asserts that a `SignedHeaders` name the request does not carry is left out of the
/// signed headers rather than failing the parse, and the request is then refused for its
/// signature, which cannot match.
#[tokio::test]
async fn a_signed_header_that_is_absent_leaves_a_signature_that_fails() {
    let s3 = FakeS3::start().await;
    s3.require_credentials("fake-key", "fake-secret");
    let authorization = "AWS4-HMAC-SHA256 \
        Credential=fake-key/20260901/us-east-1/s3/aws4_request, \
        SignedHeaders=host;x-amz-absent;x-amz-content-sha256;x-amz-date, Signature=00";
    refused(
        &s3,
        &[
            ("authorization", authorization),
            ("x-amz-date", DATE),
            ("x-amz-content-sha256", EMPTY_SHA),
        ],
    )
    .await;
}
