//! The S3 store's conditional writes, HEAD, whole-object read and stamped listing against
//! the loopback S3 service of `s3::fake`, which has to honour the conditions as S3 does for
//! the tests of other crates to mean anything.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::s3::{ObjectHead, StampedPage};
use objstore_s3::{
    Capabilities, KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError, PageSize,
};

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

#[expect(
    clippy::disallowed_methods,
    reason = "the fake service stamps objects with the machine's time, and the test compares with it"
)]
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The status and code of a `Service` error.
fn service(error: &ObjectStoreError) -> (u16, &str) {
    match error {
        ObjectStoreError::Service { status, code, .. } => (*status, code.as_str()),
        other => panic!("expected Service, got {other:?}"),
    }
}

/// The keys, sizes and times of last write of a stamped page.
fn rows(page: &StampedPage) -> Vec<(&str, u64, u64)> {
    let it = page.objects.iter();
    let rows = it.map(|o| (o.key.as_str(), o.size, o.modified_millis));
    rows.collect()
}

/// Asserts that the service refuses a create-only PUT of a key that exists with 412, which
/// the store reports as `AlreadyExists`, and leaves the first bytes; and that a PUT without
/// the condition replaces the object.
#[tokio::test]
async fn a_create_only_put_is_refused_where_the_key_exists() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("k");
    store.put_new(&k, Bytes::from("one"), None).await.unwrap();
    let err = store
        .put_new(&k, Bytes::from("two"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::AlreadyExists(_)), "{err}");
    assert_eq!(store.get_all(&k).await.unwrap(), "one");

    let plain = s3.store(Capabilities::default());
    let three = Bytes::from("three");
    plain.put_new(&k, three, None).await.unwrap();
    assert_eq!(plain.get_all(&k).await.unwrap(), "three");
}

/// Asserts that a PUT with `If-Match` replaces the object only if it has that entity tag and
/// gives the tag of the new object; that another tag, and the tag the object had before,
/// are refused with 412 and change nothing; and that a key with no object is not found.
#[tokio::test]
async fn a_put_if_match_replaces_only_the_object_with_that_tag() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("k");
    store.put_new(&k, Bytes::from("one"), None).await.unwrap();
    let old = store.head(&k).await.unwrap().etag;

    let wrong = store
        .put_if_match(&k, Bytes::from("x"), "\"not-the-tag\"")
        .await
        .unwrap_err();
    assert_eq!(service(&wrong), (412, "PreconditionFailed"));
    assert_eq!(store.get_all(&k).await.unwrap(), "one");

    let new = store
        .put_if_match(&k, Bytes::from("two"), &old)
        .await
        .unwrap();
    assert_ne!(new, old);
    assert_eq!(store.head(&k).await.unwrap().etag, new);
    assert_eq!(store.get_all(&k).await.unwrap(), "two");

    let stale = store
        .put_if_match(&k, Bytes::from("three"), &old)
        .await
        .unwrap_err();
    assert_eq!(service(&stale).0, 412);
    assert_eq!(store.get_all(&k).await.unwrap(), "two");

    let other = key("other");
    let absent = store
        .put_if_match(&other, Bytes::from("x"), &old)
        .await
        .unwrap_err();
    assert!(
        matches!(&absent, ObjectStoreError::NotFound(missing) if *missing == other),
        "{absent}"
    );
}

/// Asserts that a HEAD gives the size, the entity tag (the quoted hex MD5 of the bytes,
/// equal for equal bytes and different for different ones) and the time of last write the
/// service was told, in milliseconds; and that an absent key is not found.
#[tokio::test]
async fn a_head_gives_the_size_the_tag_of_the_bytes_and_the_time_of_last_write() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    s3.set_time(1_255_369_830);
    for name in ["a", "b"] {
        let body = Bytes::from("abc");
        store.put_new(&key(name), body, None).await.unwrap();
    }
    s3.set_time(1_255_369_831);
    let body = Bytes::from("abd");
    store.put_new(&key("c"), body, None).await.unwrap();

    let a = store.head(&key("a")).await.unwrap();
    let tag = "\"900150983cd24fb0d6963f7d28e17f72\"";
    let want = ObjectHead {
        size: 3,
        etag: tag.to_owned(),
        modified_millis: 1_255_369_830_000,
    };
    assert_eq!(a, want);
    assert_eq!(store.head(&key("b")).await.unwrap(), a);
    let c = store.head(&key("c")).await.unwrap();
    assert_ne!(c.etag, a.etag);
    assert_eq!(c.modified_millis, 1_255_369_831_000);

    let err = store.head(&key("absent")).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::NotFound(_)), "{err}");
}

/// Asserts that a service whose time was not set stamps the objects with the system clock's
/// second, so a HEAD gives a time between the seconds before and after the write.
#[tokio::test]
async fn objects_are_stamped_with_the_system_clock_until_a_time_is_set() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let before = now_seconds();
    store
        .put_new(&key("k"), Bytes::from("v"), None)
        .await
        .unwrap();
    let after = now_seconds();
    let modified = store.head(&key("k")).await.unwrap().modified_millis;
    assert_eq!(modified % 1000, 0);
    assert!((before..=after).contains(&(modified / 1000)), "{modified}");
}

/// Asserts that a read of the whole object gives all its bytes, an empty object gives none,
/// and an absent key is not found.
#[tokio::test]
async fn a_whole_object_read_gives_every_byte() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let full = Bytes::from("0123456789");
    store.put_new(&key("full"), full, None).await.unwrap();
    store
        .put_new(&key("empty"), Bytes::new(), None)
        .await
        .unwrap();
    assert_eq!(store.get_all(&key("full")).await.unwrap(), "0123456789");
    assert_eq!(store.get_all(&key("empty")).await.unwrap(), "");
    let err = store.get_all(&key("absent")).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::NotFound(_)), "{err}");
}

/// Asserts that a stamped listing gives the objects under a prefix in key order with their
/// sizes and times of last write; that a page is cut at `max-keys` and carries a token for
/// the next, which is the last page when it carries none; and that a listing started after
/// a key gives only the keys that sort after it.
#[tokio::test]
async fn a_stamped_listing_pages_with_times_a_start_after_key_and_tokens() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    s3.set_time(100);
    for name in ["a/2", "a/1", "b/1"] {
        let body = Bytes::from("v");
        store.put_new(&key(name), body, None).await.unwrap();
    }
    s3.set_time(200);
    for name in ["a/5", "a/3", "a/4"] {
        let body = Bytes::from("vv");
        store.put_new(&key(name), body, None).await.unwrap();
    }
    let prefix = KeyPrefix::new("a/").unwrap();
    let two = PageSize::new(2).unwrap();

    let first = store.list_stamped(&prefix, None, two).await.unwrap();
    assert_eq!(rows(&first), [("a/1", 1, 100_000), ("a/2", 1, 100_000)]);
    let second = store
        .list_stamped(&prefix, first.next.as_ref(), two)
        .await
        .unwrap();
    assert_eq!(rows(&second), [("a/3", 2, 200_000), ("a/4", 2, 200_000)]);
    let third = store
        .list_stamped(&prefix, second.next.as_ref(), two)
        .await
        .unwrap();
    assert_eq!(rows(&third), [("a/5", 2, 200_000)]);
    assert_eq!(third.next, None);

    let after = ListToken::after(&key("a/2"));
    let rest = store
        .list_stamped(&prefix, Some(&after), PageSize::MAX)
        .await
        .unwrap();
    let names: Vec<&str> = rows(&rest).into_iter().map(|row| row.0).collect();
    assert_eq!(names, ["a/3", "a/4", "a/5"]);
    assert_eq!(rest.next, None);
}

/// Asserts that the tag a stamped listing gives for an object is the tag a HEAD of it gives.
#[tokio::test]
async fn a_stamped_listing_gives_the_tag_of_a_head() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    for name in ["a/1", "a/2"] {
        store
            .put_new(&key(name), Bytes::from(name), None)
            .await
            .unwrap();
    }
    let prefix = KeyPrefix::new("a/").unwrap();
    let page = store
        .list_stamped(&prefix, None, PageSize::MAX)
        .await
        .unwrap();
    assert_eq!(page.objects.len(), 2);
    for object in &page.objects {
        let head = store.head(&object.key).await.unwrap();
        assert_eq!(object.etag, head.etag);
    }
}

/// Asserts that `len` is the size a HEAD gives, for a one-byte, an empty and a larger
/// object, and `None` for a key with no object (a 404 is an answer, not an error), also
/// after a delete.
#[tokio::test]
async fn len_is_the_size_of_a_head_and_none_for_a_missing_key() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    store
        .put_new(&key("one"), Bytes::from("x"), None)
        .await
        .unwrap();
    store
        .put_new(&key("empty"), Bytes::new(), None)
        .await
        .unwrap();
    store
        .put_new(&key("big"), Bytes::from(vec![9u8; 5000]), None)
        .await
        .unwrap();
    assert_eq!(store.len(&key("one")).await.unwrap(), Some(1));
    assert_eq!(store.len(&key("empty")).await.unwrap(), Some(0));
    assert_eq!(store.len(&key("big")).await.unwrap(), Some(5000));
    assert_eq!(store.len(&key("never")).await.unwrap(), None);
    store.delete(&key("big")).await.unwrap();
    assert_eq!(store.len(&key("big")).await.unwrap(), None);
}

/// Asserts that a DELETE with `If-Match` takes the object only if it has that entity tag: a
/// wrong tag is refused with 412 and the object still reads, the tag as a HEAD gives it
/// (quotes included) deletes it, and the key can be written again.
#[tokio::test]
async fn a_delete_with_if_match_removes_only_the_object_with_that_tag() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("k");
    store.put_new(&k, Bytes::from("one"), None).await.unwrap();
    let tag = store.head(&k).await.unwrap().etag;

    let err = store.delete_if_match(&k, "\"wrong\"").await.unwrap_err();
    assert_eq!(service(&err).0, 412, "{err}");
    assert_eq!(store.get_all(&k).await.unwrap(), "one");

    store.delete_if_match(&k, &tag).await.unwrap();
    let gone = store.head(&k).await;
    assert!(matches!(gone, Err(ObjectStoreError::NotFound(_))));
    store.put_new(&k, Bytes::from("two"), None).await.unwrap();
    assert_eq!(store.get_all(&k).await.unwrap(), "two");
}

/// Asserts that a DELETE with `If-Match` of a key with no object is `NotFound`, and that a
/// plain delete of such a key still succeeds.
#[tokio::test]
async fn a_delete_with_if_match_of_an_absent_key_is_not_found() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("absent");
    let err = store.delete_if_match(&k, "\"any\"").await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::NotFound(_)), "{err}");
    store.delete(&k).await.unwrap();
}

/// Asserts that a DELETE with `If-Match` is not supported on a store with Object Lock, and that
/// no request reaches the service.
#[tokio::test]
async fn a_delete_with_if_match_is_unsupported_where_object_lock_is_on() {
    let s3 = FakeS3::start().await;
    let locked = Capabilities {
        conditional_put: true,
        object_lock: true,
    };
    let store = s3.store(locked);
    let err = store.delete_if_match(&key("k"), "\"t\"").await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Unsupported(_)), "{err}");
    assert_eq!(s3.requests(), Vec::<String>::new());
}

/// Asserts that a service told to ignore the condition of a DELETE (as some S3-compatible
/// stores do) removes the object on a wrong tag and answers success, which is what a caller
/// that needs the refusal has to detect.
#[tokio::test]
async fn a_service_that_ignores_the_condition_deletes_on_a_wrong_tag() {
    let s3 = FakeS3::start().await;
    s3.ignore_delete_condition();
    let store = s3.store(CONDITIONAL);
    let k = key("k");
    store.put_new(&k, Bytes::from("one"), None).await.unwrap();
    store.delete_if_match(&k, "\"wrong\"").await.unwrap();
    let gone = store.head(&k).await;
    assert!(matches!(gone, Err(ObjectStoreError::NotFound(_))));
}

/// Asserts that a failure asked for a HEAD or a DELETE of an object hits the request it counts
/// to and no other: the second HEAD from now answers 500 while the first and third are served,
/// and the next DELETE answers 500 and leaves the object, after which one is served again.
#[tokio::test]
async fn a_failure_of_a_head_or_a_delete_hits_the_request_it_counts_to() {
    use objstore_s3::s3::fake::Failure;

    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("k");
    store.put_new(&k, Bytes::from("one"), None).await.unwrap();
    store.head(&k).await.unwrap();
    s3.fail(Failure::Head(2));
    store.head(&k).await.unwrap();
    let refused = store.head(&k).await.unwrap_err();
    assert_eq!(service(&refused).0, 500, "{refused}");
    store.head(&k).await.unwrap();

    s3.fail(Failure::Delete(1));
    let refused = store.delete(&k).await.unwrap_err();
    assert_eq!(service(&refused).0, 500, "{refused}");
    store.head(&k).await.unwrap();
    store.delete(&k).await.unwrap();
    let gone = store.head(&k).await;
    assert!(matches!(gone, Err(ObjectStoreError::NotFound(_))));
}

/// Asserts that a tagged create against the service gives the tag a HEAD of the new object
/// gives, and is refused (`AlreadyExists`) where the key has an
/// object, which stays as it was.
#[tokio::test]
async fn a_tagged_create_gives_the_tag_of_the_object_it_wrote() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("k");
    let tag = store.put_new_tagged(&k, Bytes::from("one")).await.unwrap();
    let head = store.head(&k).await.unwrap();
    assert_eq!(head.etag, tag);
    let again = store.put_new_tagged(&k, Bytes::from("two")).await;
    assert!(matches!(again, Err(ObjectStoreError::AlreadyExists(_))));
    assert_eq!(store.get_all(&k).await.unwrap(), "one");
}

/// Asserts that a create-only write is refused without a request where the store does not claim
/// conditional puts, since without the header it would replace the object.
#[tokio::test]
async fn a_tagged_create_is_refused_where_conditional_puts_are_not_claimed() {
    let s3 = FakeS3::start().await;
    let store = s3.store(Capabilities::default());
    let err = store
        .put_new_tagged(&key("k"), Bytes::from("one"))
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Unsupported(_)), "{err}");
    assert_eq!(s3.requests(), Vec::<String>::new());
}

/// Asserts that a whole read with its tag gives the bytes and the tag of the object the answer
/// came from: the tag the create gave, and after a replace the tag the replace gave.
#[tokio::test]
async fn a_tagged_whole_read_gives_the_bytes_and_the_tag_of_its_answer() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("k");
    let tag = store.put_new_tagged(&k, Bytes::from("one")).await.unwrap();
    let read = store.get_all_tagged(&k).await.unwrap();
    assert_eq!(read, (Bytes::from("one"), Some(tag.clone())));

    let two = store
        .put_if_match(&k, Bytes::from("two"), &tag)
        .await
        .unwrap();
    let read = store.get_all_tagged(&k).await.unwrap();
    assert_eq!(read, (Bytes::from("two"), Some(two)));
}

/// Asserts that a replace of a key with no object is `NotFound`, and that a whole read with its
/// tag of such a key is too.
#[tokio::test]
async fn a_replace_and_a_tagged_read_of_an_absent_key_are_not_found() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("absent");
    let replace = store
        .put_if_match(&k, Bytes::from("x"), "\"t\"")
        .await
        .unwrap_err();
    assert!(
        matches!(replace, ObjectStoreError::NotFound(_)),
        "{replace}"
    );
    let read = store.get_all_tagged(&k).await.unwrap_err();
    assert!(matches!(read, ObjectStoreError::NotFound(_)), "{read}");
}
