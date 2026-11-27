//! CopyObject and UploadPartCopy of the S3 store against the loopback S3 service of `s3::fake`:
//! the copy the store makes, the source conditions it sends for the store to test, and what a
//! refused or failed copy leaves.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use bytes::Bytes;
use objstore_s3::s3::fake::{Failure, FakeS3};
use objstore_s3::s3::{CopyConditions, CopySource, S3Store};
use objstore_s3::{ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// 2026-11-03 08:49:37 UTC, the second the service stamps objects with.
const WRITTEN: u64 = 1_793_695_777;

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

/// The status and code of a `Service` error.
fn service(error: &ObjectStoreError) -> (u16, &str) {
    match error {
        ObjectStoreError::Service { status, code, .. } => (*status, code.as_str()),
        other => panic!("expected Service, got {other:?}"),
    }
}

/// A service holding `src` = `body`, written at [`WRITTEN`], and a store on it.
async fn holding(body: &'static [u8]) -> (FakeS3, S3Store) {
    let s3 = FakeS3::start().await;
    s3.set_time(WRITTEN);
    let store = s3.store(CONDITIONAL);
    let put = store.put_new(&key("src"), Bytes::from_static(body), None);
    put.await.unwrap();
    (s3, store)
}

/// The copy of `src` into `dst` under `conditions`.
async fn copy(
    store: &S3Store,
    conditions: &CopyConditions,
) -> Result<objstore_s3::s3::CopyAnswer, ObjectStoreError> {
    let source_key = key("src");
    let source = CopySource {
        key: &source_key,
        size: 5,
        conditions,
    };
    store.copy_object(&source, &key("dst")).await
}

/// Asserts that a copy makes the destination with the source's bytes, replaces a destination
/// that exists, and answers the tag a HEAD gives and the time of the write.
#[tokio::test]
async fn a_copy_makes_the_destination_and_replaces_one_that_exists() {
    let (s3, store) = holding(b"hello").await;
    let put = store.put_new(&key("dst"), Bytes::from_static(b"old bytes"), None);
    put.await.unwrap();
    s3.set_time(WRITTEN + 60);
    let before = s3.requests().len();

    let answer = copy(&store, &CopyConditions::default()).await.unwrap();

    assert_eq!(store.get_all(&key("dst")).await.unwrap(), &b"hello"[..]);
    let head = store.head(&key("dst")).await.unwrap();
    assert_eq!(answer.etag, head.etag);
    assert_eq!(answer.etag, "\"5d41402abc4b2a76b9719d911017c592\"");
    assert_eq!(answer.modified_millis, Some((WRITTEN + 60) * 1000));
    let sent = s3.requests();
    let copied = sent[before..]
        .iter()
        .filter(|line| line.starts_with("PUT /tierstore-fake/dst"));
    assert_eq!(copied.count(), 1);
}

/// Asserts that a key with characters a path escapes is named in `x-amz-copy-source` so the
/// store finds it, and that the destination may have them too.
#[tokio::test]
async fn a_source_whose_key_is_escaped_is_found() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let odd = key("dir/a b+c%d&e");
    store
        .put_new(&odd, Bytes::from_static(b"odd"), None)
        .await
        .unwrap();
    let conditions = CopyConditions::default();
    let source = CopySource {
        key: &odd,
        size: 3,
        conditions: &conditions,
    };

    store.copy_object(&source, &key("to/x y")).await.unwrap();

    assert_eq!(store.get_all(&key("to/x y")).await.unwrap(), &b"odd"[..]);
}

/// Asserts that the copy of a key the store does not hold is the store's 404 `NoSuchKey`, and
/// writes nothing.
#[tokio::test]
async fn a_copy_of_an_absent_source_is_no_such_key() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);

    let refused = copy(&store, &CopyConditions::default()).await.unwrap_err();

    assert_eq!(service(&refused), (404, "NoSuchKey"));
    assert_eq!(s3.object_len(&key("dst")), None);
}

/// The conditions of one header, and whether the copy goes ahead under them, for a source tagged
/// `"5d41402abc4b2a76b9719d911017c592"` and written at [`WRITTEN`].
fn cases() -> Vec<(&'static str, CopyConditions, bool)> {
    let tag = "\"5d41402abc4b2a76b9719d911017c592\"".to_owned();
    let other = "\"00000000000000000000000000000000\"".to_owned();
    let before = "Tue, 03 Nov 2026 08:49:36 GMT".to_owned();
    let at = "Tue, 03 Nov 2026 08:49:37 GMT".to_owned();
    let none = CopyConditions::default();
    vec![
        (
            "if-match the tag",
            CopyConditions {
                if_match: Some(tag.clone()),
                ..none.clone()
            },
            true,
        ),
        (
            "if-match a list with the tag",
            CopyConditions {
                if_match: Some(format!("{other}, {tag}")),
                ..none.clone()
            },
            true,
        ),
        (
            "if-match *",
            CopyConditions {
                if_match: Some("*".to_owned()),
                ..none.clone()
            },
            true,
        ),
        (
            "if-match another tag",
            CopyConditions {
                if_match: Some(other.clone()),
                ..none.clone()
            },
            false,
        ),
        (
            "if-none-match the tag",
            CopyConditions {
                if_none_match: Some(tag.clone()),
                ..none.clone()
            },
            false,
        ),
        (
            "if-none-match another tag",
            CopyConditions {
                if_none_match: Some(other.clone()),
                ..none.clone()
            },
            true,
        ),
        (
            "if-modified-since a second before",
            CopyConditions {
                if_modified_since: Some(before.clone()),
                ..none.clone()
            },
            true,
        ),
        (
            "if-modified-since the second of the write",
            CopyConditions {
                if_modified_since: Some(at.clone()),
                ..none.clone()
            },
            false,
        ),
        (
            "if-unmodified-since the second of the write",
            CopyConditions {
                if_unmodified_since: Some(at.clone()),
                ..none.clone()
            },
            true,
        ),
        (
            "if-unmodified-since a second before",
            CopyConditions {
                if_unmodified_since: Some(before.clone()),
                ..none.clone()
            },
            false,
        ),
        (
            "if-unmodified-since that is not a date",
            CopyConditions {
                if_unmodified_since: Some("yesterday".to_owned()),
                ..none.clone()
            },
            true,
        ),
        (
            "if-match the tag outweighs if-unmodified-since",
            CopyConditions {
                if_match: Some(tag.clone()),
                if_unmodified_since: Some(before.clone()),
                ..none.clone()
            },
            true,
        ),
        (
            "if-none-match another tag outweighs if-modified-since",
            CopyConditions {
                if_none_match: Some(other),
                if_modified_since: Some(at),
                ..none
            },
            true,
        ),
    ]
}

/// Asserts that each source condition is sent and tested by the store as S3 tests it: a copy
/// whose condition holds is made, and one whose condition fails is 412 and writes nothing.
#[tokio::test]
async fn each_source_condition_is_tested_by_the_store() {
    for (name, conditions, goes) in cases() {
        let (s3, store) = holding(b"hello").await;

        let copied = copy(&store, &conditions).await;

        match copied {
            Ok(_) => assert!(goes, "{name}: copied"),
            Err(refused) => {
                assert!(!goes, "{name}: refused: {refused}");
                assert_eq!(service(&refused), (412, "PreconditionFailed"), "{name}");
                assert_eq!(s3.object_len(&key("dst")), None, "{name}");
            }
        }
    }
}

/// Asserts that a copy the store answers 200 and then an `<Error>` is a failure with the code the
/// store sent, and that the store wrote nothing.
#[tokio::test]
async fn a_copy_that_fails_after_its_answer_started_is_an_error() {
    let (s3, store) = holding(b"hello").await;
    s3.fail(Failure::CopyLate);

    let failed = copy(&store, &CopyConditions::default()).await.unwrap_err();

    assert_eq!(service(&failed), (200, "InternalError"));
    assert_eq!(s3.object_len(&key("dst")), None);
}

/// Asserts that UploadPartCopy stores a part from a range of the source and one from the whole
/// source, that their tags complete the upload, and that the object has the bytes in order.
#[tokio::test]
async fn parts_copied_from_a_source_complete_an_upload() {
    let (s3, store) = holding(b"hello").await;
    s3.set_min_part_bytes(1);
    let dst = key("dst");
    let id = store.create_multipart(&dst).await.unwrap();
    let conditions = CopyConditions::default();
    let source_key = key("src");
    let source = CopySource {
        key: &source_key,
        size: 5,
        conditions: &conditions,
    };

    let first = store.upload_part_copy(&source, &dst, &id, 1, ByteRange::new(1, 3));
    let first = first.await.unwrap();
    let second = store.upload_part_copy(&source, &dst, &id, 2, None);
    let second = second.await.unwrap();
    let parts = [(1, first.etag), (2, second.etag)];
    store.complete_multipart(&dst, &id, &parts).await.unwrap();

    assert_eq!(store.get_all(&dst).await.unwrap(), &b"ellhello"[..]);
    assert_eq!(first.modified_millis, Some(WRITTEN * 1000));
}

/// Asserts that a part copy names its upload and its range as S3 checks them: an upload the
/// store does not hold is 404 `NoSuchUpload`, a range past the end of the source or one that
/// ends before it starts is 400 `InvalidArgument`, and a source condition that fails is 412.
#[tokio::test]
async fn a_part_copy_is_refused_for_its_upload_its_range_or_its_condition() {
    let (_s3, store) = holding(b"hello").await;
    let dst = key("dst");
    let id = store.create_multipart(&dst).await.unwrap();
    let none = CopyConditions::default();
    let failing = CopyConditions {
        if_match: Some("\"00000000000000000000000000000000\"".to_owned()),
        ..CopyConditions::default()
    };
    let source_key = key("src");
    let source = |conditions| CopySource {
        key: &source_key,
        size: 5,
        conditions,
    };

    let (holds, refuses) = (source(&none), source(&failing));

    let unknown = store.upload_part_copy(&holds, &dst, "no-such", 1, None);
    let past = store.upload_part_copy(&holds, &dst, &id, 1, ByteRange::new(3, 5));
    let failed = store.upload_part_copy(&refuses, &dst, &id, 1, None);

    assert_eq!(service(&unknown.await.unwrap_err()), (404, "NoSuchUpload"));
    assert_eq!(service(&past.await.unwrap_err()), (400, "InvalidArgument"));
    assert_eq!(
        service(&failed.await.unwrap_err()),
        (412, "PreconditionFailed")
    );
}

/// Asserts that a store that does not copy is the 501 it answers, for an object as for a part,
/// and that nothing is written.
#[tokio::test]
async fn a_store_that_does_not_copy_answers_501() {
    let (s3, store) = holding(b"hello").await;
    let dst = key("dst");
    let id = store.create_multipart(&dst).await.unwrap();
    let conditions = CopyConditions::default();
    let source_key = key("src");
    let source = CopySource {
        key: &source_key,
        size: 5,
        conditions: &conditions,
    };

    s3.fail(Failure::CopyUnsupported);
    let object = copy(&store, &conditions).await.unwrap_err();
    s3.fail(Failure::CopyUnsupported);
    let part = store.upload_part_copy(&source, &dst, &id, 1, None).await;

    assert_eq!(service(&object), (501, "NotImplemented"));
    assert_eq!(service(&part.unwrap_err()), (501, "NotImplemented"));
    assert_eq!(s3.object_len(&dst), None);
}
