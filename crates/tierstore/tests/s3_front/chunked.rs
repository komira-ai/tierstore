//! Bodies in `aws-chunked` framing, built and signed by hand apart from the server's decoder:
//! the three streaming payload types of S3 on a put and on a part, over the wire.

use hmac::{Hmac, KeyInit, Mac};
use reqwest::Response;
use sha2::{Digest, Sha256};
use tierstore::Backend;

use crate::raw::{call, send, upload_id};
use crate::signer::{FIXED_STAMP, Request, sign_claiming, signing_key};
use crate::support::{BUCKET, Harness, KEY_ID, SECRET};

const SIGNED: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
const SIGNED_TRAILER: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER";
const UNSIGNED_TRAILER: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";
const SCOPE: &str = "20270115/us-east-1/s3/aws4_request";

fn hmac_hex(key: &[u8], text: &str) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("a key");
    mac.update(text.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The signature of a chunk, from the one before it.
fn chunk_signature(previous: &str, data: &[u8]) -> String {
    let empty = sha256_hex(b"");
    let text = format!(
        "AWS4-HMAC-SHA256-PAYLOAD\n{FIXED_STAMP}\n{SCOPE}\n{previous}\n{empty}\n{}",
        sha256_hex(data)
    );
    hmac_hex(&signing_key(SECRET, &FIXED_STAMP[..8]), &text)
}

/// The signature of the trailer, from the one of the last chunk.
fn trailer_signature(previous: &str, line: &str) -> String {
    let text = format!(
        "AWS4-HMAC-SHA256-TRAILER\n{FIXED_STAMP}\n{SCOPE}\n{previous}\n{}",
        sha256_hex(line.as_bytes())
    );
    hmac_hex(&signing_key(SECRET, &FIXED_STAMP[..8]), &text)
}

/// CRC-64/NVMe, bit by bit.
fn crc64(body: &[u8]) -> u64 {
    let mut crc = u64::MAX;
    for byte in body {
        crc ^= u64::from(*byte);
        for _ in 0..8 {
            let low = crc & 1;
            crc >>= 1;
            if low == 1 {
                crc ^= 0x9a6c_9329_ac4b_c9b5;
            }
        }
    }
    !crc
}

/// The value of the trailing checksum `name` for `body`.
fn trailing(name: &str, body: &[u8]) -> String {
    use base64::Engine as _;
    let bytes = match name {
        "x-amz-checksum-crc32" => crc32fast::hash(body).to_be_bytes().to_vec(),
        "x-amz-checksum-crc64nvme" => crc64(body).to_be_bytes().to_vec(),
        _ => Sha256::digest(body).to_vec(),
    };
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// What the request carries besides the body: the claim, the headers, the decoded length.
struct Put<'a> {
    claim: &'a str,
    trailer: Option<&'a str>,
    decoded: usize,
    omit_length: bool,
}

impl<'a> Put<'a> {
    fn new(claim: &'a str, trailer: Option<&'a str>, decoded: usize) -> Self {
        Self {
            claim,
            trailer,
            decoded,
            omit_length: false,
        }
    }

    /// Signs the head of the request, and gives the seed signature with the headers.
    fn head(
        &self,
        h: &Harness,
        path: &str,
        query: &[(&str, &str)],
    ) -> (Vec<(String, String)>, String) {
        let length = self.decoded.to_string();
        let mut extra = vec![("content-encoding", "aws-chunked")];
        if !self.omit_length {
            extra.push(("x-amz-decoded-content-length", length.as_str()));
        }
        if let Some(name) = self.trailer {
            extra.push(("x-amz-trailer", name));
        }
        let host = h.addr.to_string();
        let request = Request {
            method: "PUT",
            host: &host,
            path,
            query,
            headers: &extra,
            body: b"",
            stamp: FIXED_STAMP,
        };
        let headers = sign_claiming(&request, self.claim, KEY_ID, SECRET);
        let auth = &headers
            .iter()
            .find(|(n, _)| n == "authorization")
            .unwrap()
            .1;
        let seed = auth.rsplit_once("Signature=").unwrap().1.to_owned();
        (headers, seed)
    }
}

/// The body of an unsigned stream: `chunks` and then the trailer line, if there is one.
fn unsigned_body(chunks: &[&[u8]], trailer: Option<(&str, &str)>) -> Vec<u8> {
    let mut wire = Vec::new();
    for chunk in chunks {
        wire.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        wire.extend_from_slice(chunk);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n");
    if let Some((name, value)) = trailer {
        wire.extend_from_slice(format!("{name}:{value}\r\n").as_bytes());
    }
    wire.extend_from_slice(b"\r\n");
    wire
}

/// The body of a signed stream, the chain starting at `seed`; a trailer, if any, is signed too.
fn signed_body(seed: &str, chunks: &[&[u8]], trailer: Option<(&str, &str)>) -> Vec<u8> {
    let mut wire = Vec::new();
    let mut previous = seed.to_owned();
    let last: &[u8] = b"";
    for chunk in chunks.iter().copied().chain([last]) {
        let signature = chunk_signature(&previous, chunk);
        let head = format!("{:x};chunk-signature={signature}\r\n", chunk.len());
        wire.extend_from_slice(head.as_bytes());
        if !chunk.is_empty() {
            wire.extend_from_slice(chunk);
            wire.extend_from_slice(b"\r\n");
        }
        previous = signature;
    }
    if let Some((name, value)) = trailer {
        let line = format!("{name}:{value}\n");
        let signature = trailer_signature(&previous, &line);
        wire.extend_from_slice(format!("{name}:{value}\r\n").as_bytes());
        wire.extend_from_slice(format!("x-amz-trailer-signature:{signature}\r\n").as_bytes());
    }
    wire.extend_from_slice(b"\r\n");
    wire
}

async fn put_key(
    h: &Harness,
    key: &str,
    put: &Put<'_>,
    body: impl FnOnce(&str) -> Vec<u8>,
) -> Response {
    let path = format!("/{BUCKET}/{key}");
    let (headers, seed) = put.head(h, &path, &[]);
    send(h, "PUT", &path, &[], &headers, &body(&seed)).await
}

async fn refusal(response: Response) -> (u16, String) {
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    let code = text
        .split_once("<Code>")
        .map_or("", |(_, r)| r.split_once("</Code>").unwrap().0);
    (status, code.to_owned())
}

async fn stored(h: &Harness, key: &str) -> Option<Vec<u8>> {
    let got = h.hot_mem.get(key, None).await.ok()?;
    Some(got.to_vec())
}

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i * 7 % 251).unwrap())
        .collect()
}

/// Asserts that an unsigned stream with a trailing checksum is stored as the decoded payload,
/// with each of the trailing checksum algorithms the SDKs choose from, and that the object is
/// as long as `x-amz-decoded-content-length` says, not as long as the framed body.
#[tokio::test]
async fn an_unsigned_stream_with_a_trailing_checksum_is_stored_decoded() {
    let h = Harness::start_fixed(|_| {}).await;
    let data = payload(10_000);
    let chunks: Vec<&[u8]> = data.chunks(4096).collect();
    for (i, name) in [
        "x-amz-checksum-crc32",
        "x-amz-checksum-crc64nvme",
        "x-amz-checksum-sha256",
    ]
    .into_iter()
    .enumerate()
    {
        let key = format!("k{i}");
        let put = Put::new(UNSIGNED_TRAILER, Some(name), data.len());
        let value = trailing(name, &data);
        let reply = put_key(&h, &key, &put, |_| {
            unsigned_body(&chunks, Some((name, &value)))
        })
        .await;
        assert_eq!(reply.status(), 200, "{name}");
        assert_eq!(stored(&h, &key).await.unwrap(), data, "{name}");
    }
    let head = call(&h, "HEAD", "k0", &[], &[], b"").await;
    assert_eq!(head.headers()["content-length"], "10000");
}

/// Asserts that a trailing checksum that is not the one of the decoded payload is 400
/// `BadDigest` and nothing is stored, and that one that is not a hash of the right length is
/// 400 `InvalidDigest`.
#[tokio::test]
async fn a_trailing_checksum_that_does_not_match_stores_nothing() {
    let h = Harness::start_fixed(|_| {}).await;
    let data = payload(100);
    let name = "x-amz-checksum-crc32";
    let put = Put::new(UNSIGNED_TRAILER, Some(name), data.len());
    let wrong = trailing(name, b"another payload");
    let reply = put_key(&h, "k", &put, |_| {
        unsigned_body(&[&data], Some((name, &wrong)))
    })
    .await;
    assert_eq!(refusal(reply).await, (400, "BadDigest".to_owned()));
    let reply = put_key(&h, "k", &put, |_| {
        unsigned_body(&[&data], Some((name, "AAAA==")))
    })
    .await;
    assert_eq!(refusal(reply).await, (400, "InvalidDigest".to_owned()));
    assert_eq!(stored(&h, "k").await, None);
}

/// Asserts that a signed stream whose chunk signatures chain from the signature of the head is
/// stored decoded, in chunks of any size, and that a stream whose signature of one chunk is
/// wrong, or whose data is not the data that chunk signed, is 403 `SignatureDoesNotMatch` and
/// nothing is stored.
#[tokio::test]
async fn a_signed_stream_is_stored_only_if_the_chain_of_signatures_holds() {
    let h = Harness::start_fixed(|_| {}).await;
    let data = payload(10_000);
    let chunks: Vec<&[u8]> = data.chunks(3000).collect();
    let put = Put::new(SIGNED, None, data.len());
    let reply = put_key(&h, "good", &put, |seed| signed_body(seed, &chunks, None)).await;
    assert_eq!(reply.status(), 200);
    assert_eq!(stored(&h, "good").await.unwrap(), data);

    let bad_signature = |seed: &str| {
        let mut wire = signed_body(seed, &chunks, None);
        let at = wire
            .windows(16)
            .position(|w| w == b";chunk-signature")
            .unwrap()
            + 17;
        wire[at] = if wire[at] == b'0' { b'1' } else { b'0' };
        wire
    };
    let reply = put_key(&h, "flip", &put, bad_signature).await;
    assert_eq!(
        refusal(reply).await,
        (403, "SignatureDoesNotMatch".to_owned())
    );
    let changed = |seed: &str| {
        let mut wire = signed_body(seed, &chunks, None);
        let at = wire.windows(2).position(|w| w == b"\r\n").unwrap() + 2;
        wire[at] ^= 1;
        wire
    };
    let reply = put_key(&h, "data", &put, changed).await;
    assert_eq!(
        refusal(reply).await,
        (403, "SignatureDoesNotMatch".to_owned())
    );
    for key in ["flip", "data"] {
        assert_eq!(stored(&h, key).await, None, "{key}");
    }
}

/// Asserts that a signed stream with a signed trailing checksum is stored decoded, that a
/// trailer signed with the wrong signature is 403 `SignatureDoesNotMatch`, and that a trailer
/// whose checksum is not the payload's is 400 `BadDigest`; nothing is stored by the last two.
#[tokio::test]
async fn a_signed_stream_with_a_trailer_signs_the_trailer_too() {
    let h = Harness::start_fixed(|_| {}).await;
    let data = payload(5000);
    let name = "x-amz-checksum-crc64nvme";
    let put = Put::new(SIGNED_TRAILER, Some(name), data.len());
    let value = trailing(name, &data);
    let reply = put_key(&h, "good", &put, |seed| {
        signed_body(seed, &[&data], Some((name, &value)))
    })
    .await;
    assert_eq!(reply.status(), 200);
    assert_eq!(stored(&h, "good").await.unwrap(), data);

    // A trailer signed over a different value than the one sent.
    let reply = put_key(&h, "forged", &put, |seed| {
        let other = trailing(name, b"other");
        let mut wire = signed_body(seed, &[&data], Some((name, &other)));
        let at = wire
            .windows(other.len())
            .position(|window| window == other.as_bytes())
            .unwrap();
        wire[at..at + other.len()].copy_from_slice(value.as_bytes());
        wire
    })
    .await;
    assert_eq!(
        refusal(reply).await,
        (403, "SignatureDoesNotMatch".to_owned())
    );

    let wrong = trailing(name, b"other");
    let reply = put_key(&h, "wrong", &put, |seed| {
        signed_body(seed, &[&data], Some((name, &wrong)))
    })
    .await;
    assert_eq!(refusal(reply).await, (400, "BadDigest".to_owned()));
    assert_eq!(stored(&h, "forged").await, None);
    assert_eq!(stored(&h, "wrong").await, None);
}

/// Asserts that a stream with no `x-amz-decoded-content-length` is 411, one whose payload is
/// shorter or longer than it says is 400 `IncompleteBody`, one cut before its last chunk is
/// 400 `IncompleteBody`, and a trailer that `x-amz-trailer` did not announce is 400
/// `InvalidRequest`; nothing is stored by any.
#[tokio::test]
async fn a_stream_that_is_not_what_its_head_says_stores_nothing() {
    let h = Harness::start_fixed(|_| {}).await;
    let data = payload(300);
    let name = "x-amz-checksum-crc32";
    let value = trailing(name, &data);
    let whole = unsigned_body(&[&data], Some((name, &value)));

    let mut put = Put::new(UNSIGNED_TRAILER, Some(name), data.len());
    put.omit_length = true;
    let reply = put_key(&h, "none", &put, |_| whole.clone()).await;
    assert_eq!(
        refusal(reply).await,
        (411, "MissingContentLength".to_owned())
    );

    for (key, declared) in [("short", data.len() - 1), ("long", data.len() + 1)] {
        let put = Put::new(UNSIGNED_TRAILER, Some(name), declared);
        let reply = put_key(&h, key, &put, |_| whole.clone()).await;
        assert_eq!(
            refusal(reply).await,
            (400, "IncompleteBody".to_owned()),
            "{key}"
        );
    }
    let put = Put::new(UNSIGNED_TRAILER, Some(name), data.len());
    let reply = put_key(&h, "cut", &put, |_| whole[..whole.len() - 30].to_vec()).await;
    assert_eq!(refusal(reply).await, (400, "IncompleteBody".to_owned()));

    let other = "x-amz-checksum-sha256";
    let reply = put_key(&h, "other", &put, |_| {
        unsigned_body(&[&data], Some((other, &trailing(other, &data))))
    })
    .await;
    assert_eq!(refusal(reply).await, (400, "InvalidRequest".to_owned()));
    for key in ["none", "short", "long", "cut", "other"] {
        assert_eq!(stored(&h, key).await, None, "{key}");
    }
}

/// Asserts that the parts of a multipart upload sent as signed and as unsigned streams are
/// stored decoded, and that the completed object is their concatenation, byte for byte.
#[tokio::test]
async fn parts_sent_as_streams_make_the_object_they_decode_to() {
    let h = Harness::start_fixed(|_| {}).await;
    let started = call(&h, "POST", "big", &[("uploads", "")], &[], b"").await;
    let id = started.text().await.unwrap();
    let id = upload_id(&id).to_owned();
    let (one, two) = (payload(6000), payload(2500));
    let mut etags = Vec::new();
    for (number, data, claim) in [(1, &one, SIGNED), (2, &two, UNSIGNED_TRAILER)] {
        let number = number.to_string();
        let query = [("partNumber", number.as_str()), ("uploadId", id.as_str())];
        let path = format!("/{BUCKET}/big");
        let name = "x-amz-checksum-crc32";
        let trailer = (claim == UNSIGNED_TRAILER).then_some(name);
        let put = Put::new(claim, trailer, data.len());
        let (headers, seed) = put.head(&h, &path, &query);
        let value = trailing(name, data);
        let body = if trailer.is_some() {
            unsigned_body(&[data], Some((name, &value)))
        } else {
            signed_body(&seed, &[data], None)
        };
        let reply = send(&h, "PUT", &path, &query, &headers, &body).await;
        assert_eq!(reply.status(), 200, "part {number}");
        etags.push(reply.headers()["etag"].to_str().unwrap().to_owned());
    }
    let parts: String = etags
        .iter()
        .enumerate()
        .map(|(i, tag)| {
            format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{tag}</ETag></Part>",
                i + 1
            )
        })
        .collect();
    let xml = format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>");
    let query = [("uploadId", id.as_str())];
    let done = call(&h, "POST", "big", &query, &[], xml.as_bytes()).await;
    assert_eq!(done.status(), 200);
    let got = call(&h, "GET", "big", &[], &[], b"").await;
    let whole = [one, two].concat();
    assert_eq!(got.bytes().await.unwrap().to_vec(), whole);
}

/// Asserts that a stream is decoded whatever the pieces it arrives in: the same signed stream
/// written to the socket a few bytes at a time, so that chunk headers, signatures, data and line
/// ends are split across the reads of the server, is stored decoded.
#[tokio::test]
async fn a_stream_split_into_tiny_pieces_is_decoded_the_same() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h = Harness::start_fixed(|_| {}).await;
    let data = payload(1000);
    let name = "x-amz-checksum-crc32";
    let put = Put::new(SIGNED_TRAILER, Some(name), data.len());
    let path = format!("/{BUCKET}/tiny");
    let (headers, seed) = put.head(&h, &path, &[]);
    let value = trailing(name, &data);
    let wire = signed_body(&seed, &[&data[..400], &data[400..]], Some((name, &value)));
    let mut head = format!(
        "PUT {path} HTTP/1.1\r\ncontent-length: {}\r\nconnection: close\r\n",
        wire.len()
    );
    for (name, value) in &headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut socket = tokio::net::TcpStream::connect(h.addr).await.unwrap();
    socket.set_nodelay(true).unwrap();
    socket.write_all(head.as_bytes()).await.unwrap();
    for piece in wire.chunks(7) {
        socket.write_all(piece).await.unwrap();
        socket.flush().await.unwrap();
        tokio::task::yield_now().await;
    }
    let mut answer = String::new();
    socket.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert_eq!(stored(&h, "tiny").await.unwrap(), data);
}

/// Asserts that `server.aws_chunked_max_line_bytes` bounds what the server holds of a body: a
/// signed stream whose chunk header (a size and a 64 digit signature, about 80 bytes) is longer
/// than a limit of 64 is 400 `InvalidRequest` and nothing is stored, while the data of a chunk
/// far longer than the limit is taken, since the data is not held.
#[tokio::test]
async fn a_line_over_the_limit_is_refused_and_data_over_it_is_not() {
    let h = Harness::start_fixed(|config| config.aws_chunked_max_line_bytes = 64).await;
    let data = payload(1000);
    let put = Put::new(SIGNED, None, data.len());
    let reply = put_key(&h, "long-line", &put, |seed| {
        signed_body(seed, &[&data], None)
    })
    .await;
    assert_eq!(refusal(reply).await, (400, "InvalidRequest".to_owned()));
    assert_eq!(stored(&h, "long-line").await, None);

    let put = Put::new(UNSIGNED_TRAILER, Some("x-amz-checksum-crc32"), data.len());
    let value = trailing("x-amz-checksum-crc32", &data);
    let reply = put_key(&h, "long-data", &put, |_| {
        unsigned_body(&[&data], Some(("x-amz-checksum-crc32", &value)))
    })
    .await;
    assert_eq!(reply.status(), 200);
    assert_eq!(stored(&h, "long-data").await.unwrap(), data);
}
