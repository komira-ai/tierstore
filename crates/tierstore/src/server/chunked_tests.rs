//! The decoder of `aws-chunked` bodies: framing, pieces of any size, the chain of signatures, and
//! every way a body can be wrong. The signatures here are made with `hmac` and `sha2` apart from
//! the decoder's own code.

use std::collections::VecDeque;

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use hyper::StatusCode;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};

use super::auth::StreamAuth;
use super::chunked::{Decoder, Framing, TRAILABLE};
use super::reply::S3Error;

const SIGNED: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
const SIGNED_TRAILER: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER";
const UNSIGNED_TRAILER: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";
const CRC32: &str = "x-amz-checksum-crc32";

const KEY: [u8; 32] = [7; 32];
const STAMP: &str = "20270115T080000Z";
const SCOPE: &str = "20270115/us-east-1/s3/aws4_request";

fn auth() -> StreamAuth {
    StreamAuth {
        key: KEY.to_vec(),
        stamp: STAMP.to_owned(),
        scope: SCOPE.to_owned(),
        seed: "5".repeat(64),
    }
}

fn sign(text: &str) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&KEY).unwrap();
    mac.update(text.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn sha(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn chunk_signature(previous: &str, data: &[u8]) -> String {
    let empty = sha(b"");
    let hash = sha(data);
    sign(&format!(
        "AWS4-HMAC-SHA256-PAYLOAD\n{STAMP}\n{SCOPE}\n{previous}\n{empty}\n{hash}"
    ))
}

fn trailer_signature(previous: &str, canonical: &str) -> String {
    let hash = sha(canonical.as_bytes());
    sign(&format!(
        "AWS4-HMAC-SHA256-TRAILER\n{STAMP}\n{SCOPE}\n{previous}\n{hash}"
    ))
}

/// A signed stream of `chunks`, with a signed trailer if there is one.
fn signed_wire(chunks: &[&[u8]], trailer: Option<(&str, &str)>) -> Vec<u8> {
    let mut wire = Vec::new();
    let mut previous = auth().seed;
    let end: &[u8] = b"";
    for chunk in chunks.iter().copied().chain([end]) {
        let signature = chunk_signature(&previous, chunk);
        wire.extend(format!("{:x};chunk-signature={signature}\r\n", chunk.len()).bytes());
        if !chunk.is_empty() {
            wire.extend_from_slice(chunk);
            wire.extend_from_slice(b"\r\n");
        }
        previous = signature;
    }
    if let Some((name, value)) = trailer {
        let signature = trailer_signature(&previous, &format!("{name}:{value}\n"));
        wire.extend(format!("{name}:{value}\r\n").bytes());
        wire.extend(format!("x-amz-trailer-signature:{signature}\r\n").bytes());
    }
    wire.extend_from_slice(b"\r\n");
    wire
}

/// An unsigned stream of `chunks` and a trailer if there is one.
fn plain_wire(chunks: &[&[u8]], trailer: Option<(&str, &str)>) -> Vec<u8> {
    let mut wire = Vec::new();
    for chunk in chunks {
        wire.extend(format!("{:x}\r\n", chunk.len()).bytes());
        wire.extend_from_slice(chunk);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n");
    if let Some((name, value)) = trailer {
        wire.extend(format!("{name}:{value}\r\n").bytes());
    }
    wire.extend_from_slice(b"\r\n");
    wire
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        let name = name.parse::<HeaderName>().unwrap();
        map.insert(name, HeaderValue::from_str(value).unwrap());
    }
    map
}

/// The framing of a request with this claim, announcing the trailer `CRC32` and `length` bytes.
fn framing(claim: &str, length: usize) -> Framing {
    let length = length.to_string();
    let head = headers(&[
        ("x-amz-decoded-content-length", &length),
        ("x-amz-trailer", CRC32),
    ]);
    Framing::parse(claim, &head).unwrap().unwrap()
}

/// What a decoder gives for `wire` fed in pieces of `size` bytes: the data, and the trailer.
fn decode(
    framing: &Framing,
    wire: &[u8],
    size: usize,
    max_line: usize,
) -> Result<(Vec<u8>, Option<String>), S3Error> {
    let stream = framing.signed().then(auth);
    let mut decoder = Decoder::new(framing, stream, max_line);
    let mut out = VecDeque::new();
    for piece in wire.chunks(size) {
        decoder.feed(Bytes::copy_from_slice(piece), &mut out)?;
    }
    let data: Vec<u8> = out.into_iter().flat_map(|piece| piece.to_vec()).collect();
    Ok((data, decoder.finish()?))
}

fn kind(error: S3Error) -> (u16, &'static str) {
    let (status, code) = error.kind();
    (status.as_u16(), code)
}

const DATA: &[u8] = b"The quick brown fox jumps over the lazy dog, again and again.";

/// Asserts the framing a request's head gives: none for a payload that is not streamed, the
/// three streaming types with their signing and trailer, a trailer name in any case and with
/// spaces, and every checksum S3 takes as a trailer.
#[test]
fn the_head_of_a_request_says_how_its_body_is_framed() {
    let length = [("x-amz-decoded-content-length", "61")];
    assert!(
        Framing::parse("UNSIGNED-PAYLOAD", &headers(&length))
            .unwrap()
            .is_none()
    );
    let signed = Framing::parse(SIGNED, &headers(&length)).unwrap().unwrap();
    assert!(signed.signed());
    assert_eq!((signed.decoded(), signed.trailer()), (61, None));
    for (claim, is_signed) in [(SIGNED_TRAILER, true), (UNSIGNED_TRAILER, false)] {
        for name in TRAILABLE {
            let spaced = format!(" {} ", name.to_uppercase());
            let head = headers(&[length[0], ("x-amz-trailer", &spaced)]);
            let got = Framing::parse(claim, &head).unwrap().unwrap();
            assert_eq!((got.signed(), got.trailer()), (is_signed, Some(name)));
        }
    }
}

/// Asserts the refusals of a head: a streaming type that is not one of the three is 501, a
/// missing `x-amz-decoded-content-length` is 411, one that is not a number is 400
/// `InvalidArgument`, and a trailer that is missing, unknown, or more than one checksum is 400
/// `InvalidRequest`.
#[test]
fn a_head_that_does_not_say_what_its_body_is_is_refused() {
    let ecdsa = "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD";
    let head = headers(&[("x-amz-decoded-content-length", "1")]);
    let refused = Framing::parse(ecdsa, &head).unwrap_err();
    assert_eq!(kind(refused), (501, "NotImplemented"));
    let refused = Framing::parse(SIGNED, &HeaderMap::new()).unwrap_err();
    assert_eq!(kind(refused), (411, "MissingContentLength"));
    let head = headers(&[("x-amz-decoded-content-length", "ten")]);
    let refused = Framing::parse(SIGNED, &head).unwrap_err();
    assert_eq!(kind(refused), (400, "InvalidArgument"));
    let two = "x-amz-checksum-crc32,x-amz-checksum-sha1";
    for trailer in [None, Some("x-amz-checksum-md5"), Some(two)] {
        let mut pairs = vec![("x-amz-decoded-content-length", "1")];
        pairs.extend(trailer.map(|value| ("x-amz-trailer", value)));
        let refused = Framing::parse(UNSIGNED_TRAILER, &headers(&pairs)).unwrap_err();
        assert_eq!(kind(refused), (400, "InvalidRequest"), "{trailer:?}");
    }
}

/// Asserts that an unsigned stream, with chunks of several sizes (one a single byte) and a trailer, decodes to the data and gives the trailer's text, whatever the
/// size of the pieces the wire is cut into, down to a byte.
#[test]
fn an_unsigned_stream_decodes_in_pieces_of_any_size() {
    let chunks: [&[u8]; 3] = [&DATA[..10], &DATA[10..11], &DATA[11..]];
    let wire = plain_wire(&chunks, Some((CRC32, "AAAAAA==")));
    let got = framing(UNSIGNED_TRAILER, DATA.len());
    for size in [1, 2, 3, 7, 16, wire.len() - 1, wire.len(), wire.len() + 5] {
        let (data, trailer) = decode(&got, &wire, size, 1024).unwrap();
        assert_eq!(data, DATA, "pieces of {size}");
        assert_eq!(trailer.as_deref(), Some("AAAAAA=="), "pieces of {size}");
    }
}

/// Asserts that a signed stream decodes, the chain of signatures holding, in pieces of any size,
/// with a signed trailer and without, and with no data at all.
#[test]
fn a_signed_stream_decodes_in_pieces_of_any_size() {
    let chunks: [&[u8]; 2] = [&DATA[..33], &DATA[33..]];
    let with = signed_wire(&chunks, Some((CRC32, "AAAAAA==")));
    let without = signed_wire(&chunks, None);
    let empty = signed_wire(&[], None);
    for size in [1, 5, 64, 1000] {
        let trailing = framing(SIGNED_TRAILER, DATA.len());
        let (data, trailer) = decode(&trailing, &with, size, 1024).unwrap();
        assert_eq!(
            (data, trailer.as_deref()),
            (DATA.to_vec(), Some("AAAAAA=="))
        );
        let plain = framing(SIGNED, DATA.len());
        let (data, trailer) = decode(&plain, &without, size, 1024).unwrap();
        assert_eq!((data, trailer), (DATA.to_vec(), None));
        let nothing = framing(SIGNED, 0);
        assert_eq!(decode(&nothing, &empty, size, 1024).unwrap().0, b"");
    }
}

/// Asserts that a signed stream whose signature is wrong anywhere is 403 `SignatureDoesNotMatch`:
/// a changed byte of data, a changed signature of a chunk, a signature that is not hex, a chunk
/// moved to another place, a trailer signed over another value, a signature of the trailer that
/// is missing or is not hex, a final chunk signed as if it had data, and a stream signed by
/// another key.
#[test]
fn a_signed_stream_whose_chain_is_broken_is_refused() {
    let chunks: [&[u8]; 2] = [&DATA[..33], &DATA[33..]];
    let good = signed_wire(&chunks, Some((CRC32, "AAAAAA==")));
    let got = framing(SIGNED_TRAILER, DATA.len());
    let text = |wire: &[u8]| String::from_utf8(wire.to_vec()).unwrap();
    let denied = (403, "SignatureDoesNotMatch");
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    cases.push((
        "data",
        text(&good).replacen("quick", "quack", 1).into_bytes(),
    ));
    let first = text(&good).split_once(";chunk-signature=").unwrap().1[..64].to_owned();
    let flipped = format!(
        "{}{}",
        if first.starts_with('0') { '1' } else { '0' },
        &first[1..]
    );
    cases.push((
        "signature",
        text(&good).replacen(&first, &flipped, 1).into_bytes(),
    ));
    cases.push((
        "not hex",
        text(&good)
            .replacen(&first, &"z".repeat(64), 1)
            .into_bytes(),
    ));
    let first_chunk = text(&good).find("\r\n").unwrap() + 2 + chunks[0].len() + 2;
    cases.push(("dropped chunk", good[first_chunk..].to_vec()));
    cases.push((
        "value",
        text(&good).replacen("AAAAAA==", "BBBBBB==", 1).into_bytes(),
    ));
    let trailer_sig = text(&good).split_once("signature:").unwrap().1[..64].to_owned();
    let dropped =
        text(&good).replacen(&format!("x-amz-trailer-signature:{trailer_sig}\r\n"), "", 1);
    cases.push(("no trailer signature", dropped.into_bytes()));
    let broken = text(&good).replacen(&trailer_sig, &"q".repeat(64), 1);
    cases.push(("trailer signature not hex", broken.into_bytes()));
    for (name, wire) in cases {
        let refused = decode(&got, &wire, 9, 1024).unwrap_err();
        assert_eq!(kind(refused), denied, "{name}");
    }
    let mut other = auth();
    other.key = vec![8; 32];
    let mut decoder = Decoder::new(&got, Some(other), 1024);
    let refused = decoder
        .feed(Bytes::from(good), &mut VecDeque::new())
        .unwrap_err();
    assert_eq!(kind(refused), denied);
}

/// Asserts that a stream that is not framed as S3 frames it is 400 `InvalidRequest`: a size that
/// is not hex digits, empty, or signed with a plus; a chunk not ended by CR LF; a line with a bare
/// line feed or bytes that are not text; a signed chunk with no signature; a trailer line that is
/// not the announced checksum, or twice; a signature line in an unsigned stream; a line over the
/// limit; and bytes after the end.
#[test]
fn a_body_that_is_not_framed_as_aws_chunked_is_refused() {
    let unsigned = framing(UNSIGNED_TRAILER, 5);
    let invalid = (400, "InvalidRequest");
    let good = plain_wire(&[b"hello"], Some((CRC32, "AAAAAA==")));
    let text = String::from_utf8(good.clone()).unwrap();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "size not hex",
            text.replacen("5\r\n", "zz\r\n", 1).into_bytes(),
        ),
        ("size empty", text.replacen("5\r\n", "\r\n", 1).into_bytes()),
        (
            "size signed",
            text.replacen("5\r\n", "+5\r\n", 1).into_bytes(),
        ),
        (
            "size too long",
            text.replacen("5\r\n", "10000000000000000\r\n", 1)
                .into_bytes(),
        ),
        (
            "chunk end",
            text.replacen("hello\r\n", "helloXX", 1).into_bytes(),
        ),
        (
            "bare line feed",
            text.replacen("5\r\n", "5\n", 1).into_bytes(),
        ),
        ("not text", b"\xff\xfe\r\n".to_vec()),
        (
            "other trailer",
            text.replacen(CRC32, "x-amz-checksum-sha1", 1).into_bytes(),
        ),
        (
            "trailer twice",
            text.replacen("\r\n\r\n", "\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n", 1)
                .into_bytes(),
        ),
        (
            "no colon",
            text.replacen("x-amz-checksum-crc32:AAAAAA==", "nothing", 1)
                .into_bytes(),
        ),
        (
            "signature line",
            text.replacen("\r\n\r\n", "\r\nx-amz-trailer-signature:00\r\n\r\n", 1)
                .into_bytes(),
        ),
        ("after the end", [good.clone(), b"x".to_vec()].concat()),
    ];
    for (name, wire) in cases {
        let refused = decode(&unsigned, &wire, 4, 1024).unwrap_err();
        assert_eq!(kind(refused), invalid, "{name}");
    }
    let refused = decode(&unsigned, &good, 4, 10).unwrap_err();
    assert_eq!(kind(refused), invalid);
    assert!(refused_text(&unsigned, &good, 10).contains("server.aws_chunked_max_line_bytes"));
    let signed = framing(SIGNED, 5);
    let no_signature = b"5\r\nhello\r\n0\r\n\r\n";
    let refused = decode(&signed, no_signature, 4, 1024).unwrap_err();
    assert_eq!(kind(refused), invalid);
    let announced = framing(SIGNED, 0);
    let wire = signed_wire(&[], Some((CRC32, "AAAAAA==")));
    let refused = decode(&announced, &wire, 4, 1024).unwrap_err();
    assert_eq!(kind(refused), invalid);
}

/// Asserts that a signature of the trailer is taken only where the head announced a trailer and
/// the stream is signed, and only once: one on a signed stream that announced no trailer, and a
/// second one after the first, are 400 `InvalidRequest`.
#[test]
fn a_signature_of_a_trailer_that_was_not_announced_or_comes_twice_is_refused() {
    let invalid = (400, "InvalidRequest");
    let chunks: [&[u8]; 1] = [b"hello"];
    let wire = signed_wire(&chunks, Some((CRC32, "AAAAAA==")));
    let text = String::from_utf8(wire).unwrap();
    let no_trailer = text.replacen("x-amz-checksum-crc32:AAAAAA==\r\n", "", 1);
    let head = headers(&[("x-amz-decoded-content-length", "5")]);
    let got = Framing::parse(SIGNED, &head).unwrap().unwrap();
    let refused = decode(&got, no_trailer.as_bytes(), 4, 1024).unwrap_err();
    assert_eq!(kind(refused), invalid);

    let signature = text.split_once("signature:").unwrap().1[..64].to_owned();
    let line = format!("x-amz-trailer-signature:{signature}\r\n");
    let twice = text.replacen(&line, &format!("{line}{line}"), 1);
    let got = framing(SIGNED_TRAILER, 5);
    let refused = decode(&got, twice.as_bytes(), 4, 1024).unwrap_err();
    assert_eq!(kind(refused), invalid);
}

fn refused_text(framing: &Framing, wire: &[u8], max_line: usize) -> String {
    decode(framing, wire, 4, max_line)
        .unwrap_err()
        .message()
        .to_owned()
}

/// Asserts that the length the request declared is held: more data than it says is 400
/// `IncompleteBody` as soon as the extra byte arrives, less data, a body cut anywhere before its
/// trailer ends, and a trailer that was announced and is not there are refused at the end.
#[test]
fn a_body_that_is_not_the_length_or_the_shape_it_announced_is_refused() {
    let wire = plain_wire(&[DATA], Some((CRC32, "AAAAAA==")));
    let incomplete = (400, "IncompleteBody");
    let long = framing(UNSIGNED_TRAILER, DATA.len() - 1);
    let refused = decode(&long, &wire, 1000, 1024).unwrap_err();
    assert_eq!(kind(refused), incomplete);
    let short = framing(UNSIGNED_TRAILER, DATA.len() + 1);
    let refused = decode(&short, &wire, 1000, 1024).unwrap_err();
    assert_eq!(kind(refused), incomplete);
    assert!(refused_text(&short, &wire, 1024).contains(&(DATA.len() + 1).to_string()));
    let exact = framing(UNSIGNED_TRAILER, DATA.len());
    for cut in [0, 1, 5, wire.len() - 4, wire.len() - 1] {
        let refused = decode(&exact, &wire[..cut], 1000, 1024).unwrap_err();
        assert_eq!(kind(refused), incomplete, "cut at {cut}");
    }
    let missing = plain_wire(&[DATA], None);
    let refused = decode(&exact, &missing, 1000, 1024).unwrap_err();
    assert_eq!(kind(refused), (400, "InvalidRequest"));
}

/// Asserts that a signed stream cannot be decoded without the material to check it by.
#[test]
fn a_signed_stream_with_no_means_to_check_it_is_not_implemented() {
    let got = framing(SIGNED, 1);
    let mut decoder = Decoder::new(&got, None, 1024);
    let refused = decoder
        .feed(Bytes::from_static(b"1"), &mut VecDeque::new())
        .unwrap_err();
    assert_eq!(
        refused.kind(),
        (StatusCode::NOT_IMPLEMENTED, "NotImplemented")
    );
}

/// Asserts that the body minio-go sends (and so `mc`) is taken as S3 takes it: the checksum line
/// of the trailer ended by a line feed alone, then the empty line, then one more CR LF; in
/// pieces of any size. Anything but line ends after the empty line is still refused, and so is
/// a line of the trailer that is not text.
#[test]
fn the_trailer_minio_go_sends_is_decoded() {
    let wire = b"5\r\nhello\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\n\r\n\r\n";
    let got = framing(UNSIGNED_TRAILER, 5);
    for size in [1, 2, 3, 11, wire.len()] {
        let (data, trailer) = decode(&got, wire, size, 1024).unwrap();
        assert_eq!(
            (data, trailer.as_deref()),
            (b"hello".to_vec(), Some("AAAAAA=="))
        );
    }
    let more = b"5\r\nhello\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n\r\n\n";
    assert!(decode(&got, more, 4, 1024).is_ok());
    let junk = b"5\r\nhello\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n\r\nx";
    assert_eq!(
        kind(decode(&got, junk, 4, 1024).unwrap_err()),
        (400, "InvalidRequest")
    );
    let not_text = b"5\r\nhello\r\n0\r\nx-amz-checksum-crc32:\xff\xfe\r\n\r\n";
    assert_eq!(
        kind(decode(&got, not_text, 4, 1024).unwrap_err()),
        (400, "InvalidRequest")
    );
}
