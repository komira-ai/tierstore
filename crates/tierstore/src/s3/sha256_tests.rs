//! The S3 backend's put that names the SHA-256 of its body, against the loopback S3 service of
//! the tests: the object made, a put sent again answered by the digest a HEAD reads back (the
//! store's checksum for one PUT, the recorded digest for an object of parts or a store that keeps
//! no checksum), and what each refusal of the store is.

use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt as _, stream};
use objstore_s3::s3::fake::{Failure, FakeS3};
use objstore_s3::s3::{Credentials, S3Config, S3Tuning, StoredSha256};
use objstore_s3::{Capabilities, ObjectKey, ObjectStore as _, ObjectStoreError};

use super::{compare, refused, same};
use crate::{Backend, Error, PartBody, PutOutcome, S3Backend, TreeDigest};

fn sha(bytes: &[u8]) -> TreeDigest {
    TreeDigest::of(bytes)
}

fn body(bytes: &[u8]) -> PartBody<'static> {
    let chunk = Bytes::copy_from_slice(bytes);
    stream::once(async move { Ok(chunk) }).boxed()
}

/// A backend over the bucket of `s3` that sends a body over `single_put_max` bytes as a multipart
/// upload of 5 MiB parts.
fn backend(s3: &FakeS3, single_put_max: u64) -> S3Backend {
    let config = S3Config {
        endpoint: s3.endpoint().to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tier-test".to_owned(),
        credentials: Credentials::new("test-key", "test-secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(60),
        min_body_rate: None,
    };
    let tuning = S3Tuning {
        single_put_max,
        part_bytes: 5 << 20,
        ..S3Tuning::default()
    };
    S3Backend::new_tuned(config, tuning).unwrap()
}

/// `len` bytes that are not all zeros.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 253) as u8).collect()
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

/// Asserts that a put of a free key makes the object with the digest kept by the store, that the
/// same put again is `Existed` (the store's whole-object checksum is the digest), and that other
/// bytes under the key are `AlreadyExists` and leave the object as it was.
#[tokio::test]
async fn one_put_makes_the_object_and_a_put_again_is_answered_by_the_checksum() {
    let s3 = FakeS3::start().await;
    let backend = backend(&s3, 1 << 20);
    let made = backend.put_stream_sha256("cas/b/1/pack", 3, body(b"abc"), sha(b"abc"));
    assert_eq!(made.await.unwrap(), PutOutcome::Created);
    let again = backend.put_stream_sha256("cas/b/1/pack", 3, body(b"abc"), sha(b"abc"));
    assert_eq!(again.await.unwrap(), PutOutcome::Existed);
    let other = backend.put_stream_sha256("cas/b/1/pack", 3, body(b"abd"), sha(b"abd"));
    let refused = other.await.unwrap_err();
    assert_eq!(refused, Error::AlreadyExists("cas/b/1/pack".to_owned()));
    assert_eq!(backend.get("cas/b/1/pack", None).await.unwrap(), "abc");
}

/// Asserts that a body longer than one PUT is made from parts, and that a put of it again is
/// `Existed` by the digest recorded in the object's metadata, since the store's own checksum of an
/// object of parts is not its digest; other bytes of the same length are `AlreadyExists`.
#[tokio::test]
async fn a_put_of_parts_again_is_answered_by_the_recorded_digest() {
    let s3 = FakeS3::start().await;
    let backend = backend(&s3, 1 << 20);
    let bytes = pattern(6 << 20);
    let len = bytes.len() as u64;
    let made = backend.put_stream_sha256("big", len, body(&bytes), sha(&bytes));
    assert_eq!(made.await.unwrap(), PutOutcome::Created);
    assert_eq!(s3.stats().completed, 1);
    let again = backend.put_stream_sha256("big", len, body(&bytes), sha(&bytes));
    assert_eq!(again.await.unwrap(), PutOutcome::Existed);
    let mut other = bytes.clone();
    other[0] ^= 1;
    let differs = backend.put_stream_sha256("big", len, body(&other), sha(&other));
    assert_eq!(
        differs.await.unwrap_err(),
        Error::AlreadyExists("big".to_owned())
    );
}

/// Asserts that over a store that does no checksums the backend still stores nothing of a body
/// that is not its digest (it checks the body itself), and that a put sent again is answered by
/// the recorded digest.
#[tokio::test]
async fn over_a_store_that_checks_nothing_the_body_is_checked_here() {
    let s3 = FakeS3::start().await;
    s3.ignore_checksums();
    let backend = backend(&s3, 1 << 20);
    let wrong = backend.put_stream_sha256("k", 3, body(b"abc"), sha(b"abd"));
    assert_eq!(wrong.await.unwrap_err(), Error::BadDigest("k".to_owned()));
    assert_eq!(s3.object_len(&key("k")), None);
    let made = backend.put_stream_sha256("k", 3, body(b"abc"), sha(b"abc"));
    assert_eq!(made.await.unwrap(), PutOutcome::Created);
    let again = backend.put_stream_sha256("k", 3, body(b"abc"), sha(b"abc"));
    assert_eq!(again.await.unwrap(), PutOutcome::Existed);
}

/// Asserts that an object written without a digest is never taken for the one a put meant to
/// make, even with the same bytes: there is nothing to compare, so it is `AlreadyExists`.
#[tokio::test]
async fn an_object_with_no_digest_is_not_the_same() {
    let s3 = FakeS3::start().await;
    let backend = backend(&s3, 1 << 20);
    backend.put("k", Bytes::from_static(b"abc")).await.unwrap();
    let put = backend.put_stream_sha256("k", 3, body(b"abc"), sha(b"abc"));
    assert_eq!(put.await.unwrap_err(), Error::AlreadyExists("k".to_owned()));
}

/// Asserts the comparison: the size must match; the store's whole-object checksum decides when
/// there is one, whatever the recorded digest says; the recorded digest decides when there is
/// none; and nothing to compare is not the same.
#[test]
fn the_stored_checksum_decides_and_the_recorded_digest_stands_in() {
    let (want, other) = ([1u8; 32], [2u8; 32]);
    let stored = |checksum, recorded| StoredSha256 {
        size: 3,
        checksum,
        recorded,
    };
    assert!(same(&stored(Some(want), None), 3, &want));
    assert!(!same(&stored(Some(want), None), 4, &want), "another size");
    assert!(!same(&stored(Some(other), Some(want)), 3, &want));
    assert!(same(&stored(None, Some(want)), 3, &want));
    assert!(!same(&stored(None, Some(other)), 3, &want));
    assert!(!same(&stored(None, None), 3, &want));
}

/// Asserts that an object gone by the time of the HEAD is a `Conflict` (a retry gets the true
/// answer) and that a HEAD that fails is that failure, never `Existed`.
#[tokio::test]
async fn a_head_that_finds_nothing_or_fails_is_not_a_success() {
    let s3 = FakeS3::start().await;
    let backend = backend(&s3, 1 << 20);
    let gone = compare(&backend, &key("k"), 3, &[0; 32]).await;
    assert_eq!(gone.unwrap_err(), Error::Conflict("k".to_owned()));
    backend.put("k", Bytes::from_static(b"abc")).await.unwrap();
    s3.fail(Failure::Head(1));
    let failed = compare(&backend, &key("k"), 3, &[0; 32]).await.unwrap_err();
    assert!(matches!(failed, Error::Unavailable(_)), "{failed}");
}

/// Asserts what a refusal of the write is: a checksum the bytes do not have, in S3's code and in
/// MinIO's, is `BadDigest`; an object too large is `TooLarge`; anything else is what it is for any
/// call.
#[test]
fn a_refusal_of_the_write_is_named_by_its_code() {
    let service = |status, code: &str| ObjectStoreError::Service {
        status,
        code: code.to_owned(),
        message: "m".to_owned(),
    };
    let k = || "k".to_owned();
    assert_eq!(
        refused(service(400, "BadDigest"), "k"),
        Error::BadDigest(k())
    );
    let minio = service(400, "XAmzContentChecksumMismatch");
    assert_eq!(refused(minio, "k"), Error::BadDigest(k()));
    let large = service(400, "EntityTooLarge");
    assert_eq!(refused(large, "k"), Error::TooLarge(k()));
    let full = refused(service(507, "Full"), "k");
    assert!(matches!(full, Error::Full(_)), "{full}");
}

/// Asserts the bounds the backend refuses before it sends anything: an object more than S3's
/// multipart limit can hold is `TooLarge`, and a key the store cannot have is refused as such.
#[tokio::test]
async fn what_cannot_be_an_object_is_refused_before_it_is_sent() {
    let s3 = FakeS3::start().await;
    let backend = backend(&s3, 1 << 20);
    let huge = backend.put_stream_sha256("k", u64::MAX, body(b""), sha(b""));
    assert_eq!(huge.await.unwrap_err(), Error::TooLarge("k".to_owned()));
    let empty = backend.put_stream_sha256("", 0, body(b""), sha(b""));
    assert_eq!(empty.await.unwrap_err(), Error::InvalidKey(String::new()));
    assert_eq!(s3.requests(), Vec::<String>::new());
}

/// Asserts that the store's answer to the PUT is what made the object: it holds the bytes, and a
/// HEAD reads back the digest as the store's checksum and as the recorded digest.
#[tokio::test]
async fn the_object_carries_its_digest_twice() {
    let s3 = FakeS3::start().await;
    let backend = backend(&s3, 1 << 20);
    let put = backend.put_stream_sha256("k", 3, body(b"abc"), sha(b"abc"));
    put.await.unwrap();
    let stored = backend.store.head_sha256(&key("k")).await.unwrap();
    let digest = *sha(b"abc").as_bytes();
    assert_eq!(
        (stored.checksum, stored.recorded),
        (Some(digest), Some(digest))
    );
    let len = backend.store.len(&key("k")).await.unwrap();
    assert_eq!(len, Some(3));
}
