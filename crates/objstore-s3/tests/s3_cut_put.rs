//! A single PUT whose body fails, against the loopback S3 service of `s3::fake` made to keep a PUT
//! cut short as SeaweedFS 4.54 can: what the store is left with under the key, and what the look
//! after the PUT counts (`s3/cut_put.rs`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use md5::Md5;
use objstore_s3::s3::fake::{Failure, FakeS3};
use objstore_s3::s3::{CutPuts, PutChecksums, S3Store, S3Tuning};
use objstore_s3::{BodyStream, Capabilities, ObjectKey, ObjectStore as _, ObjectStoreError};
use sha2::{Digest as _, Sha256};

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

/// A store, shared by the test and the put it spawns, that looks at the key at once after a PUT
/// that got no answer: the service's [`FakeS3::hold_heads`] orders the look after the cut PUT, as
/// the wait does on a real store.
fn store(s3: &FakeS3) -> Arc<S3Store> {
    let tuning = S3Tuning {
        cut_put_look_after: Duration::ZERO,
        ..S3Tuning::default()
    };
    Arc::new(s3.store_tuned(CONDITIONAL, tuning))
}

/// `bytes` as a body of one chunk.
fn body(bytes: &'static [u8]) -> BodyStream {
    futures::stream::once(async move { Ok(Bytes::from_static(bytes)) }).boxed()
}

/// A body that gives `pieces` and then fails, as the client of an upload that went away: each
/// item waits a turn of the runtime first, so the HTTP client sends what it has (the head, and
/// every piece but the one it holds back) before the failure ends the request.
fn cut(pieces: &[&'static [u8]]) -> BodyStream {
    let mut items: Vec<Result<Bytes, ObjectStoreError>> = pieces
        .iter()
        .map(|piece| Ok(Bytes::from_static(piece)))
        .collect();
    items.push(Err(ObjectStoreError::Body("the client went away".into())));
    futures::stream::iter(items)
        .then(|item| async move {
            tokio::task::yield_now().await;
            item
        })
        .boxed()
}

/// How many requests the service took with this method, of any key.
fn taken(s3: &FakeS3, method: &str) -> usize {
    let prefix = format!("{method} ");
    s3.requests()
        .iter()
        .filter(|line| line.starts_with(&prefix))
        .count()
}

/// The counts of [`CutPuts`], in the order removed, left, failed.
fn counts(removed: u64, left: u64, failed: u64) -> CutPuts {
    CutPuts {
        removed,
        left,
        failed,
    }
}

/// Runs `put` against a service that keeps cut bodies, with its HEADs held until the service
/// has taken `cuts` requests cut short, so the look after the PUT sees what the service kept of
/// it; gives the put's error.
async fn cut_put<F>(s3: &FakeS3, cuts: u64, put: F) -> ObjectStoreError
where
    F: Future<Output = Result<(), ObjectStoreError>> + Send + 'static,
{
    cut_put_after(s3, 0, cuts, put).await
}

/// [`cut_put`] for a put that sends `heads` HEADs of its own before its PUT: those are let
/// through.
async fn cut_put_after<F>(s3: &FakeS3, heads: u32, cuts: u64, put: F) -> ObjectStoreError
where
    F: Future<Output = Result<(), ObjectStoreError>> + Send + 'static,
{
    s3.keep_cut_bodies();
    s3.hold_heads(heads);
    let put = tokio::spawn(put);
    s3.wait_for(|s| s.cut_requests() == cuts).await;
    s3.release_heads();
    put.await.unwrap().unwrap_err()
}

/// Asserts that a create-only PUT whose body fails before its first byte, on a store that keeps it
/// as an object of 0 bytes, leaves no object under the key, and that the look counts one removal.
/// Catches: a failed put that leaves the 0-byte object a create-only writer is refused by for ever.
#[tokio::test]
async fn a_put_cut_before_its_first_byte_leaves_no_object() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let k = key("cas/cut-empty");
    let put = {
        let (store, k) = (store.clone(), k.clone());
        async move { store.put_stream(&k, 10, cut(&[])).await }
    };
    let failed = cut_put(&s3, 1, put).await;
    assert!(matches!(failed, ObjectStoreError::Body(_)), "{failed}");
    assert_eq!(s3.object_len(&k), None);
    assert_eq!(store.cut_puts(), counts(1, 0, 0));
    assert_eq!((taken(&s3, "HEAD"), taken(&s3, "DELETE")), (1, 1));
    // The PUT, the HEAD and the DELETE: the cut PUT is one whose head the service read.
    assert_eq!(s3.requests_begun(), 3);
}

/// Asserts that a plain PUT cut after some bytes, which the store keeps short over the object the
/// key held, is removed too: the object left is the PUT's own.
#[tokio::test]
async fn a_plain_put_cut_part_way_removes_the_short_object_it_made() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let k = key("ac/plain");
    store.put_stream(&k, 3, body(b"old")).await.unwrap();
    let put = {
        let (store, k) = (store.clone(), k.clone());
        let pieces = cut(&[b"ab", b"cd"]);
        async move {
            store
                .put_stream_replacing_answered(&k, 10, pieces)
                .await
                .map(drop)
        }
    };
    cut_put(&s3, 1, put).await;
    assert_eq!(s3.object_len(&k), None);
    assert_eq!(store.cut_puts(), counts(1, 0, 0));
}

/// Asserts that a create-only PUT cut short over a key that already held an object of another
/// length (the store refuses the PUT, and the client never hears it) leaves that object as it
/// was, and counts it as left; and that one over an object of the length declared counts nothing.
/// Catches: a look that deletes any object of the wrong length, the one there before the put
/// began included.
#[tokio::test]
async fn a_put_cut_over_an_object_there_before_leaves_it() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let k = key("cas/before");
    store.put_stream(&k, 8, body(b"eight by")).await.unwrap();
    let put = {
        let (store, k) = (store.clone(), k.clone());
        async move { store.put_stream(&k, 10, cut(&[])).await }
    };
    cut_put(&s3, 1, put).await;
    assert_eq!(s3.object_len(&k), Some(8));
    assert_eq!(store.cut_puts(), counts(0, 1, 0));

    let put = {
        let (store, k) = (store.clone(), k.clone());
        async move { store.put_stream(&k, 8, cut(&[b"eight"])).await }
    };
    cut_put(&s3, 2, put).await;
    assert_eq!(s3.object_len(&k), Some(8));
    assert_eq!(store.cut_puts(), counts(0, 1, 0));
    assert_eq!(taken(&s3, "DELETE"), 0);
}

/// Asserts that a 0-byte object a PUT of another writer left (one that carries no token of this
/// store's PUT, as a release before the look writes) is left as it is by a create-only PUT cut
/// short over it, and counted as left.
#[tokio::test]
async fn a_short_object_of_another_put_is_left() {
    let s3 = FakeS3::start().await;
    let other = store(&s3);
    let k = key("cas/other");
    s3.keep_cut_bodies();
    s3.fail(Failure::Head(1));
    let failed = other.put_stream(&k, 10, cut(&[])).await.unwrap_err();
    assert!(matches!(failed, ObjectStoreError::Body(_)), "{failed}");
    s3.wait_for(|s| s.object_len(&k) == Some(0)).await;
    assert_eq!(other.cut_puts(), counts(0, 0, 1));

    let store = store(&s3);
    let put = {
        let (store, k) = (store.clone(), k.clone());
        async move { store.put_stream(&k, 10, cut(&[])).await }
    };
    cut_put(&s3, 2, put).await;
    assert_eq!(s3.object_len(&k), Some(0));
    assert_eq!(store.cut_puts(), counts(0, 1, 0));
}

/// Asserts that a look whose DELETE fails leaves the object and counts a failure, and that a cut
/// PUT the store kept nothing of counts nothing.
#[tokio::test]
async fn a_look_that_cannot_delete_counts_a_failure() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let k = key("cas/stays");
    s3.fail(Failure::Delete(1));
    let put = {
        let (store, k) = (store.clone(), k.clone());
        async move { store.put_stream(&k, 10, cut(&[])).await }
    };
    cut_put(&s3, 1, put).await;
    assert_eq!(s3.object_len(&k), Some(0));
    assert_eq!(store.cut_puts(), counts(0, 0, 1));
}

/// Asserts that a PUT cut short on a store that takes a request only whole (the service's
/// default) is looked after once and counts nothing: the HEAD finds no object.
#[tokio::test]
async fn a_cut_put_the_store_kept_nothing_of_counts_nothing() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let k = key("cas/nothing");
    let failed = store.put_stream(&k, 10, body(b"short")).await;
    assert!(
        matches!(failed, Err(ObjectStoreError::Body(_))),
        "{failed:?}"
    );
    assert_eq!(s3.object_len(&k), None);
    assert_eq!(store.cut_puts(), CutPuts::default());
    assert_eq!((taken(&s3, "HEAD"), taken(&s3, "DELETE")), (1, 0));
}

/// Asserts that a PUT the store answered (a create-only PUT refused 412) is not looked after: no
/// HEAD follows it.
#[tokio::test]
async fn a_put_the_store_answered_is_not_looked_after() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let k = key("cas/answered");
    store.put_stream(&k, 3, body(b"one")).await.unwrap();
    let refused = store.put_stream(&k, 3, body(b"two")).await;
    assert!(matches!(refused, Err(ObjectStoreError::AlreadyExists(_))));
    assert_eq!(taken(&s3, "HEAD"), 0);
    assert_eq!(store.cut_puts(), CutPuts::default());
}

/// The digests of `bytes`, as [`PutChecksums`] of both.
fn checksums(bytes: &[u8]) -> PutChecksums {
    PutChecksums {
        md5: Some(Md5::digest(bytes).into()),
        sha256: Some(Sha256::digest(bytes).into()),
    }
}

/// Asserts that a create-only PUT and a plain PUT that carry the digests of the whole body are
/// refused by a store that keeps cut bodies when cut short (nothing kept, nothing to remove), are
/// taken whole when the body is whole, and that a body that is not the digest it names is refused
/// `BadDigest`, for the MD5 alone as for the SHA-256 alone.
/// Catches: digests that are not sent with the PUT, so the store keeps the body cut short.
#[tokio::test]
async fn a_put_that_names_its_digests_is_refused_cut_short_and_taken_whole() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let whole: &'static [u8] = b"0123456789";
    let sums = checksums(whole);
    let new = key("cas/checked-new");
    let put = {
        let (store, new) = (store.clone(), new.clone());
        let pieces = cut(&[b"01", b"23"]);
        async move {
            store
                .put_stream_new_checked(&new, 10, pieces, sums)
                .await
                .map(drop)
        }
    };
    cut_put(&s3, 1, put).await;
    let plain = key("ac/checked-plain");
    let put = {
        let (store, plain) = (store.clone(), plain.clone());
        let pieces = cut(&[b"01", b"23"]);
        async move {
            store
                .put_stream_replacing_checked(&plain, 10, pieces, sums)
                .await
                .map(drop)
        }
    };
    cut_put(&s3, 2, put).await;
    assert_eq!((s3.object_len(&new), s3.object_len(&plain)), (None, None));
    assert_eq!(store.cut_puts(), CutPuts::default());

    let made = store.put_stream_new_checked(&new, 10, body(whole), sums);
    assert!(made.await.unwrap().etag.is_some());
    let made = store.put_stream_replacing_checked(&plain, 10, body(whole), sums);
    assert!(made.await.unwrap().etag.is_some());
    assert_eq!(
        (s3.object_len(&new), s3.object_len(&plain)),
        (Some(10), Some(10))
    );

    let other = checksums(b"9876543210");
    for (name, wrong) in [
        (
            "md5",
            PutChecksums {
                sha256: None,
                ..other
            },
        ),
        ("sha256", PutChecksums { md5: None, ..other }),
    ] {
        let k = key(&format!("cas/wrong-{name}"));
        let refused = store
            .put_stream_new_checked(&k, 10, body(whole), wrong)
            .await;
        match refused {
            Err(ObjectStoreError::Service { status, code, .. }) => {
                assert_eq!((status, code.as_str()), (400, "BadDigest"), "{name}");
            }
            other => panic!("{name}: expected BadDigest, got {other:?}"),
        }
        assert_eq!(s3.object_len(&k), None, "{name}");
    }
}

/// Asserts that the content-bound write, whose HEAD before its PUT finds no object, then cut short
/// (its PUT names the digest, so the store keeps nothing of it), is looked after with a second
/// HEAD that finds nothing, and counts nothing.
#[tokio::test]
async fn a_content_bound_write_cut_short_leaves_nothing_to_remove() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let k = key("cas/bound");
    let digest: [u8; 32] = Sha256::digest(b"0123456789").into();
    let put = {
        let (store, k) = (store.clone(), k.clone());
        let pieces = cut(&[b"01", b"23"]);
        async move {
            let put = store.put_stream_content_bound(&k, 10, pieces, digest);
            put.await.map(drop)
        }
    };
    let failed = cut_put_after(&s3, 1, 1, put).await;
    assert!(matches!(failed, ObjectStoreError::Body(_)), "{failed}");
    assert_eq!(s3.object_len(&k), None);
    assert_eq!(taken(&s3, "HEAD"), 2);
    assert_eq!(store.cut_puts(), CutPuts::default());
}

/// Asserts that every single PUT names itself with a token of 32 hex digits, which the object
/// keeps as its metadata, and that two PUTs carry different ones.
#[tokio::test]
async fn every_put_carries_a_token_of_its_own() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let tokens: Vec<String> = {
        let mut tokens = Vec::new();
        for name in ["cas/t1", "cas/t2"] {
            store.put_stream(&key(name), 1, body(b"x")).await.unwrap();
            let token = s3.object_meta(&key(name), "x-amz-meta-write-token");
            tokens.push(token.unwrap());
        }
        tokens
    };
    assert_ne!(tokens[0], tokens[1]);
    for token in &tokens {
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
    }
}
