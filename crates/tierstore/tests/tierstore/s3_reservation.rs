//! Reservations of the S3 backend over the loopback S3 service: the checks every backend passes,
//! the metadata the mark travels as, and the errors of a key the client cannot name and of a store
//! that cannot be reached.

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use tierstore::{Backend, ByteRange, Error, S3Backend};

use crate::conformance::{self, reservation};
use crate::s3::{backend, config};
use crate::support::body;

/// Asserts that a reservation heads as one, with its size and tag, reads as its bytes whole and
/// by range with the mark, is listed with its size, and that an absent key and a range past the
/// end are errors, over the S3 service.
#[tokio::test]
async fn a_reservation_is_marked_and_reads_as_its_bytes() {
    let (_s3, backend) = backend().await;
    conformance::a_reservation_is_marked_and_reads_as_its_bytes(&backend).await;
}

/// Asserts that a reservation and a put each refuse a key the other holds, over the S3 service.
#[tokio::test]
async fn a_reservation_and_a_put_each_refuse_a_key_the_other_holds() {
    let (_s3, backend) = backend().await;
    conformance::a_reservation_and_a_put_each_refuse_a_key_the_other_holds(&backend).await;
}

/// Asserts that an empty object, one with the bytes of a reservation and a streamed one are not
/// reservations, over the S3 service.
#[tokio::test]
async fn a_plain_object_is_not_a_reservation() {
    let (_s3, backend) = backend().await;
    conformance::a_plain_object_is_not_a_reservation(&backend).await;
}

/// Asserts that a replace on the reservation's tag makes a real object and a replace on another
/// tag changes nothing, over the S3 service.
#[tokio::test]
async fn a_plain_replace_of_a_reservation_makes_a_real_object() {
    let (_s3, backend) = backend().await;
    conformance::a_plain_replace_of_a_reservation_makes_a_real_object(&backend).await;
}

/// Asserts that a marked replace keeps the mark and takes only the right tag, over the S3 service.
#[tokio::test]
async fn a_marked_replace_keeps_the_mark_and_takes_only_the_tag() {
    let (_s3, backend) = backend().await;
    conformance::a_marked_replace_keeps_the_mark_and_takes_only_the_tag(&backend).await;
}

/// Asserts that a conditional delete takes a reservation only on its tag, over the S3 service,
/// with the read of the tag before the DELETE (the default) and with `If-Match` on the DELETE
/// (`conditional_delete` on).
#[tokio::test]
async fn a_conditional_delete_takes_a_reservation_only_on_its_tag() {
    let (_s3, backend) = backend().await;
    conformance::a_conditional_delete_takes_a_reservation_only_on_its_tag(&backend).await;
    backend.delete("k").await.unwrap();
    let backend = backend.with_conditional_delete(true);
    conformance::a_conditional_delete_takes_a_reservation_only_on_its_tag(&backend).await;
}

/// Reads key `k` with the mark, whole and by range, and asserts that each read gave `held` (or its
/// first four bytes) with `marked` and the content tag `tag`, and that the two reads were two GETs
/// and no other request.
async fn two_reads_are_two_gets(
    s3: &FakeS3,
    backend: &S3Backend,
    held: Bytes,
    marked: bool,
    tag: &str,
) {
    let before = s3.requests().len();
    let whole = backend.get_tagged("k", None).await.unwrap();
    assert_eq!(whole, (held.clone(), marked, tag.to_owned()));
    let range = Some(ByteRange::new(0, 4));
    let part = backend.get_tagged("k", range).await.unwrap();
    assert_eq!(part, (held.slice(0..4), marked, tag.to_owned()));
    let sent = s3.requests();
    let methods: Vec<&str> = sent[before..]
        .iter()
        .map(|line| line.split(' ').next().unwrap_or(""))
        .collect();
    assert_eq!(methods, ["GET", "GET"]);
}

/// Asserts that a read with the mark, whole or by range, is one GET and no HEAD, so that the bytes
/// and the mark are those of one answer whatever is written to the key before and after it: the key
/// is a reservation, a real object, and a reservation again, and each read gives the mark of the
/// object it found.
#[tokio::test]
async fn a_read_with_the_mark_is_one_get_whole_or_by_range() {
    let (s3, backend) = backend().await;
    let tag = backend.put_reservation("k", reservation(1)).await.unwrap();
    two_reads_are_two_gets(&s3, &backend, reservation(1), true, &tag).await;
    let real = backend.put_if_match("k", body("a real object"), &tag);
    let real = real.await.unwrap();
    two_reads_are_two_gets(&s3, &backend, body("a real object"), false, &real).await;
    backend.delete("k").await.unwrap();
    let again = backend.put_reservation("k", reservation(3)).await.unwrap();
    two_reads_are_two_gets(&s3, &backend, reservation(3), true, &again).await;
}

/// Asserts that a key the client cannot name is `Other` for each reservation call and for a read
/// with the mark, before a request is made.
#[tokio::test]
async fn a_key_the_client_cannot_name_is_refused_by_the_reservation_calls() {
    let (s3, backend) = backend().await;
    let key = "a b";
    let created = backend.put_reservation(key, reservation(1)).await;
    let replaced = backend
        .put_reservation_if_match(key, reservation(1), "t")
        .await;
    let read = backend.get_tagged(key, None).await;
    assert!(matches!(created, Err(Error::Other(_))), "{created:?}");
    assert!(matches!(replaced, Err(Error::Other(_))), "{replaced:?}");
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

/// Asserts that a store whose connections are reset makes each reservation call, and a read with
/// the mark, `Unavailable`.
#[tokio::test]
async fn a_store_that_resets_connections_fails_the_reservation_calls_as_unavailable() {
    let endpoint = resetting_endpoint().await;
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let created = backend.put_reservation("k", reservation(1)).await;
    let replaced = backend
        .put_reservation_if_match("k", reservation(1), "t")
        .await;
    let read = backend.get_tagged("k", None).await;
    let ranged = backend.get_tagged("k", Some(ByteRange::new(0, 4))).await;
    for failed in [
        created.unwrap_err(),
        replaced.unwrap_err(),
        read.unwrap_err(),
        ranged.unwrap_err(),
    ] {
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

/// Asserts that a read with the mark, whole or by range, whose answer carries no `ETag` is an
/// error and not bytes with a made-up tag: the read cannot say which object it read.
#[tokio::test]
async fn a_read_with_the_mark_whose_answer_has_no_etag_is_an_error() {
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
