//! The read with its tag of the S3 backend over the loopback S3 service: the check every backend
//! passes, the one GET each read is, and the errors of a key the client cannot name, of a store
//! that cannot be reached and of an answer that carries no `ETag`.

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use tierstore::{Backend, ByteRange, Error, S3Backend};

use crate::conformance;
use crate::s3::{backend, config};
use crate::support::body;

/// Asserts that a read with its tag gives the bytes and the tag a HEAD gives, whole and by range,
/// that an absent key is `NotFound` and a range past the end an error, over the S3 service.
#[tokio::test]
async fn a_tagged_read_gives_the_bytes_and_the_tag_of_one_object() {
    let (_s3, backend) = backend().await;
    conformance::a_tagged_read_gives_the_bytes_and_the_tag_of_one_object(&backend).await;
}

/// Reads key `k` with its tag, whole and by range, and asserts that each read gave `held` (or its
/// first four bytes) and the content tag `tag`, and that the two reads were two GETs and no other
/// request.
async fn two_reads_are_two_gets(s3: &FakeS3, backend: &S3Backend, held: Bytes, tag: &str) {
    let before = s3.requests().len();
    let whole = backend.get_tagged("k", None).await.unwrap();
    assert_eq!(whole, (held.clone(), tag.to_owned()));
    let range = Some(ByteRange::new(0, 4));
    let part = backend.get_tagged("k", range).await.unwrap();
    assert_eq!(part, (held.slice(0..4), tag.to_owned()));
    let sent = s3.requests();
    let methods: Vec<&str> = sent[before..]
        .iter()
        .map(|line| line.split(' ').next().unwrap_or(""))
        .collect();
    assert_eq!(methods, ["GET", "GET"]);
}

/// Asserts that a read with its tag, whole or by range, is one GET and no HEAD, so that the bytes
/// and the tag are those of one answer whatever is written to the key before and after it: the key
/// is an object, a replaced object, and a new object after a delete, and each read gives the tag
/// of the object it found.
#[tokio::test]
async fn a_tagged_read_is_one_get_whole_or_by_range() {
    let (s3, backend) = backend().await;
    let tag = backend.put_tagged("k", body("first object")).await.unwrap();
    two_reads_are_two_gets(&s3, &backend, body("first object"), &tag).await;
    let replaced = backend.put_if_match("k", body("a real object"), &tag);
    let replaced = replaced.await.unwrap();
    two_reads_are_two_gets(&s3, &backend, body("a real object"), &replaced).await;
    backend.delete("k").await.unwrap();
    let again = backend.put_tagged("k", body("third object")).await.unwrap();
    two_reads_are_two_gets(&s3, &backend, body("third object"), &again).await;
}

/// Asserts that a key the client cannot name is `Other` for a read with its tag, before a request
/// is made.
#[tokio::test]
async fn a_key_the_client_cannot_name_is_refused_by_a_tagged_read() {
    let (s3, backend) = backend().await;
    let read = backend.get_tagged("a b", None).await;
    assert!(matches!(read, Err(Error::Other(_))), "{read:?}");
    assert_eq!(s3.requests(), Vec::<String>::new());
}

/// An endpoint on loopback that stays bound for the whole test and resets every connection it
/// accepts without answering, so that a request to it fails in transport and no other process or
/// test can come to hold the address.
async fn resetting_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            socket.set_zero_linger().unwrap();
            drop(socket);
        }
    });
    endpoint
}

/// Asserts that a store whose connections are reset makes a read with its tag, whole or by range,
/// `Unavailable`.
#[tokio::test]
async fn a_store_that_resets_connections_fails_a_tagged_read_as_unavailable() {
    let endpoint = resetting_endpoint().await;
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let read = backend.get_tagged("k", None).await;
    let ranged = backend.get_tagged("k", Some(ByteRange::new(0, 4))).await;
    for failed in [read.unwrap_err(), ranged.unwrap_err()] {
        assert!(failed.is_unavailable(), "{failed}");
    }
}

/// An endpoint on loopback that answers each request, whole or ranged, with the bytes `abcd` and
/// no `ETag`, and stays bound for the whole test.
async fn endpoint_without_etags() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
            let ranged = text.contains("range: bytes=0-3");
            let (status, extra) = if ranged {
                ("206 Partial Content", "content-range: bytes 0-3/4\r\n")
            } else {
                ("200 OK", "")
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: 4\r\n{extra}\r\nabcd"
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    endpoint
}

/// Asserts that a read with its tag, whole or by range, whose answer carries no `ETag` is an
/// error and not bytes with a made-up tag: the read cannot say which object it read.
#[tokio::test]
async fn a_tagged_read_whose_answer_has_no_etag_is_an_error() {
    let endpoint = endpoint_without_etags().await;
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let whole = backend.get_tagged("k", None).await.unwrap_err();
    let ranged = backend.get_tagged("k", Some(ByteRange::new(0, 4)));
    let ranged = ranged.await.unwrap_err();
    for failed in [whole, ranged] {
        assert_eq!(
            failed,
            Error::Other("k: the read's answer has no ETag".to_owned())
        );
    }
}
