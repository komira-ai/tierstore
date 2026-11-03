//! Streamed reads and streamed conditional replacements of the S3 backend: the checks every
//! backend passes over the loopback S3 service, a replacement large enough to be a multipart
//! upload, and a read whose body is held back by the store to show that its chunks are given as
//! they arrive and not after the whole body.

use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt, stream};
use tierstore::{Backend, ByteRange, Error, PartBody, ReadBody, S3Backend};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::conformance;
use crate::s3::{backend, config};
use crate::support::body;

/// Asserts that a streamed read gives the bytes of the object or the range, with an empty range and
/// a range at the end giving none (a 416 of the service is turned into one by a HEAD), a range past
/// the end an error and an absent key `NotFound`, over the S3 service.
#[tokio::test]
async fn a_streamed_read_gives_the_bytes_of_its_range() {
    let (_s3, backend) = backend().await;
    conformance::a_streamed_read_gives_the_bytes_of_its_range(&backend).await;
}

/// Asserts that an open read gives the meta a head gives and the bytes of its span with the window
/// it resolved the span to, over the S3 service: a whole read, a range (cut at the end) and the
/// open-ended range are one GET, and a 416 or a suffix is told by a HEAD.
#[tokio::test]
async fn an_open_read_gives_the_meta_and_the_bytes_of_its_span() {
    let (_s3, backend) = backend().await;
    conformance::an_open_read_gives_the_meta_and_the_bytes_of_its_span(&backend).await;
}

/// Asserts that a streamed replacement of a reservation takes effect only on the tag and a body
/// that ended whole, and then gives an object that is no reservation, over the S3 service.
#[tokio::test]
async fn a_streamed_replace_takes_the_tag_and_a_whole_body() {
    let (_s3, backend) = backend().await;
    conformance::a_streamed_replace_takes_the_tag_and_a_whole_body(&backend).await;
}

/// Asserts that a tagged streamed put to S3 gives the tag a head gives, refuses a key that exists
/// and leaves nothing for a body that fails, over the S3 service.
#[tokio::test]
async fn a_streamed_tagged_put_gives_the_tag_of_its_object() {
    let (_s3, backend) = backend().await;
    conformance::a_streamed_tagged_put_gives_the_tag_of_its_object(&backend).await;
}

/// A body of 16 MiB of zeros and one byte more, as one-MiB chunks, which is more than one PUT
/// carries; it ends with `error` instead of the last byte if there is one.
fn over_one_put(error: Option<&Error>) -> PartBody<'static> {
    let mebibyte = Bytes::from(vec![0_u8; 1 << 20]);
    let mut items: Vec<Result<Bytes, Error>> = vec![Ok(mebibyte); 16];
    items.push(match error {
        Some(error) => Err(error.clone()),
        None => Ok(body("z")),
    });
    stream::iter(items).boxed()
}

const OVER_ONE_PUT: u64 = (16 << 20) + 1;

/// Asserts that a streamed replacement of more than 16 MiB is a multipart upload whose completion
/// carries the tag: another tag, a body that fails and an absent key each leave the object as it
/// was and no upload open; the right tag replaces it with the tag the completion gave, which a
/// HEAD gives too.
#[tokio::test]
async fn a_streamed_replace_of_more_than_one_put_is_completed_with_the_tag() {
    let (s3, backend) = backend().await;
    backend.put("k", body("old")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;

    let wrong = backend.put_stream_if_match("k", Some(OVER_ONE_PUT), over_one_put(None), "no");
    assert_eq!(wrong.await, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(s3.incomplete_uploads(), 0);

    let cut = Error::Other("cut".to_owned());
    let failing = over_one_put(Some(&cut));
    let failed = backend.put_stream_if_match("k", Some(OVER_ONE_PUT), failing, &tag);
    assert_eq!(failed.await, Err(cut));
    assert_eq!(s3.incomplete_uploads(), 0);

    let absent = backend.put_stream_if_match("none", Some(OVER_ONE_PUT), over_one_put(None), &tag);
    assert_eq!(absent.await, Err(Error::NotFound("none".to_owned())));
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(backend.get("k", None).await.unwrap(), body("old"));
    assert_eq!(s3.stats().completed, 0);

    let right = backend.put_stream_if_match("k", Some(OVER_ONE_PUT), over_one_put(None), &tag);
    let new = right.await.unwrap();
    let head = backend.head("k").await.unwrap();
    assert_eq!((head.tag.as_str(), head.size), (new.as_str(), OVER_ONE_PUT));
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.stats().completed, 1);
    let tail = ByteRange::new(OVER_ONE_PUT - 3, 3);
    assert_eq!(backend.get("k", Some(tail)).await.unwrap(), body("\0\0z"));
}

/// A store on loopback that answers the first request with a body of `length` bytes of which it
/// sends `first` at once and `rest` only after `release` is notified, and then closes the
/// connection. Gives its endpoint.
async fn slow_body(length: usize, first: Vec<u8>, rest: Vec<u8>, release: Arc<Notify>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (mut seen, mut buffer) = (Vec::new(), [0_u8; 4096]);
        while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "the client closed before it sent its request");
            seen.extend_from_slice(&buffer[..count]);
        }
        let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {length}\r\n\r\n");
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(&first).await.unwrap();
        release.notified().await;
        socket.write_all(&rest).await.unwrap();
    });
    endpoint
}

/// The first `count` bytes of `read`, taken chunk by chunk and no further: the chunks may be cut
/// anywhere, so as many are taken as it takes to have `count` bytes.
async fn take(read: &mut ReadBody, count: usize) -> Vec<u8> {
    let mut taken = Vec::new();
    while taken.len() < count {
        taken.extend_from_slice(&read.next().await.unwrap().unwrap());
    }
    taken
}

/// Asserts that a streamed read gives the first chunk of a body while the store still holds back
/// the rest of it (the test lets the store send the rest only after it took the first chunk, so a
/// read that collected the body first would wait forever), and then the rest of the bytes and the
/// end of the body.
#[tokio::test]
async fn a_streamed_read_gives_chunks_while_the_store_still_holds_back_the_rest() {
    let release = Arc::new(Notify::new());
    let (first, rest) = (vec![1_u8; 100], vec![2_u8; 50]);
    let endpoint = slow_body(150, first.clone(), rest.clone(), release.clone()).await;
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let mut read = backend.get_stream("k", None).await.unwrap();
    assert_eq!(take(&mut read, 100).await, first);

    release.notify_one();
    let mut tail = Vec::new();
    while let Some(chunk) = read.next().await {
        tail.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(tail, rest);
}

/// Asserts that a streamed read whose body is cut short by the store gives the bytes that came
/// and then an error, `Unavailable`, as its last chunk: a reader does not take the bytes for the
/// whole object.
#[tokio::test]
async fn a_streamed_read_cut_short_by_the_store_ends_with_an_error() {
    let release = Arc::new(Notify::new());
    let endpoint = slow_body(150, vec![1_u8; 100], Vec::new(), release.clone()).await;
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let mut read = backend.get_stream("k", None).await.unwrap();
    assert_eq!(take(&mut read, 100).await, vec![1_u8; 100]);
    release.notify_one();
    let ended = read.next().await.unwrap().unwrap_err();
    assert!(ended.is_unavailable(), "{ended}");
    assert!(read.next().await.is_none());
}

/// A store on loopback that answers its first request with `head` (the status line and the
/// headers, with no blank line after them) and `body`. Gives its endpoint.
async fn answers(head: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (mut seen, mut buffer) = (Vec::new(), [0_u8; 4096]);
        while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "the client closed before it sent its request");
            seen.extend_from_slice(&buffer[..count]);
        }
        let answer = format!("{head}\r\n\r\n{body}");
        socket.write_all(answer.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    endpoint
}

/// Asserts that an open read whose answer lacks a part of the meta (the time of last write, the
/// entity tag, the length of the object) is an error that names the part, and not a meta made
/// up: such a store is not S3.
#[tokio::test]
async fn an_open_read_whose_answer_lacks_part_of_the_meta_is_an_error() {
    let cases = [
        (
            "HTTP/1.1 200 OK\r\ncontent-length: 2\r\netag: \"t\"",
            "ab",
            "Last-Modified",
        ),
        (
            "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nlast-modified: Tue, 03 Nov 2026 08:49:37 GMT",
            "ab",
            "ETag",
        ),
        (
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\netag: \"t\"\r\nlast-modified: Tue, 03 Nov 2026 08:49:37 GMT",
            "2\r\nab\r\n0\r\n\r\n",
            "Content-Length",
        ),
    ];
    for (head, body, lacks) in cases {
        let endpoint = answers(head, body).await;
        let backend = S3Backend::new(config(&endpoint)).unwrap();
        let failed = backend.open_read("k", None).await.err().unwrap();
        assert!(
            matches!(&failed, Error::Other(why) if why.contains(lacks)),
            "{lacks}: {failed}"
        );
    }
}

/// Asserts that a tagged streamed put of more than 16 MiB is a multipart upload completed once,
/// giving the tag a head gives; that a body that fails leaves no upload open and no object; and
/// that a key that exists is refused with no upload left open.
#[tokio::test]
async fn a_tagged_streamed_put_of_more_than_one_put_is_completed_with_its_tag() {
    let (s3, backend) = backend().await;
    let cut = Error::Other("cut".to_owned());
    let failing = over_one_put(Some(&cut));
    let failed = backend.put_stream_tagged("k", Some(OVER_ONE_PUT), failing);
    assert_eq!(failed.await, Err(cut));
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(
        backend.head("k").await.unwrap_err(),
        Error::NotFound("k".to_owned())
    );

    let made = backend.put_stream_tagged("k", Some(OVER_ONE_PUT), over_one_put(None));
    let tag = made.await.unwrap();
    let head = backend.head("k").await.unwrap();
    assert_eq!((head.tag.as_str(), head.size), (tag.as_str(), OVER_ONE_PUT));
    assert_eq!(s3.stats().completed, 1);

    let again = backend.put_stream_tagged("k", Some(OVER_ONE_PUT), over_one_put(None));
    assert_eq!(again.await, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(s3.incomplete_uploads(), 0);
}
