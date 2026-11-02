//! Reservations of the S3 backend over the loopback S3 service: the checks every backend passes,
//! the metadata the mark travels as, and the errors of a key the client cannot name and of a store
//! that cannot be reached.

use std::net::TcpListener;

use tierstore::{Backend, ByteRange, Error, S3Backend};

use crate::conformance::{self, reservation};
use crate::s3::{backend, config};

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
/// before the store is asked how it does a conditional delete and after it found the store to
/// enforce `If-Match`.
#[tokio::test]
async fn a_conditional_delete_takes_a_reservation_only_on_its_tag() {
    let (_s3, backend) = backend().await;
    conformance::a_conditional_delete_takes_a_reservation_only_on_its_tag(&backend).await;
    backend.delete("k").await.unwrap();
    let way = backend.settle_conditional_delete("probe/conditional").await;
    assert_eq!(way.as_deref(), Some("if-match"));
    conformance::a_conditional_delete_takes_a_reservation_only_on_its_tag(&backend).await;
}

/// Asserts that a whole read with the mark is one GET and no HEAD, and that a ranged one is a HEAD,
/// a ranged GET and a HEAD, so that the bytes are given only for an object the two HEADs agree on.
#[tokio::test]
async fn a_read_with_the_mark_is_one_get_for_a_whole_object_and_a_get_between_two_heads_for_a_range()
 {
    let (s3, backend) = backend().await;
    backend.put_reservation("k", reservation(1)).await.unwrap();
    let before = s3.requests().len();
    backend.get_marked("k", None).await.unwrap();
    let whole: Vec<String> = s3.requests()[before..].to_vec();
    assert_eq!(whole.len(), 1, "{whole:?}");
    assert!(whole[0].starts_with("GET "), "{whole:?}");

    let before = s3.requests().len();
    backend
        .get_marked("k", Some(ByteRange::new(0, 4)))
        .await
        .unwrap();
    let sent = s3.requests();
    let methods: Vec<&str> = sent[before..]
        .iter()
        .map(|line| line.split(' ').next().unwrap_or(""))
        .collect();
    assert_eq!(methods, ["HEAD", "GET", "HEAD"]);
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
    let read = backend.get_marked(key, None).await;
    assert!(matches!(created, Err(Error::Other(_))), "{created:?}");
    assert!(matches!(replaced, Err(Error::Other(_))), "{replaced:?}");
    assert!(matches!(read, Err(Error::Other(_))), "{read:?}");
    assert_eq!(s3.requests(), Vec::<String>::new());
}

/// Asserts that a store that cannot be reached makes each reservation call, and a read with the
/// mark, `Unavailable`.
#[tokio::test]
async fn a_store_that_cannot_be_reached_fails_the_reservation_calls_as_unavailable() {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let created = backend.put_reservation("k", reservation(1)).await;
    let replaced = backend
        .put_reservation_if_match("k", reservation(1), "t")
        .await;
    let read = backend.get_marked("k", None).await;
    for failed in [
        created.unwrap_err(),
        replaced.unwrap_err(),
        read.unwrap_err(),
    ] {
        assert!(failed.is_unavailable(), "{failed}");
    }
}
