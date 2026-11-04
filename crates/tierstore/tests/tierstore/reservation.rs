//! Reservations as a backend holds them: the memory and the directory backend held to the
//! checks every backend passes, the calls the trait refuses for a backend that does not do them,
//! and the wrappers that pass them on (the fault injector, the RAM cache).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{
    Backend, ByteRange, CachedStore, DirBackend, Error, FaultyBackend, Listed, MemoryBackend, Meta,
    Op,
};
use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;

use crate::conformance::{self, reservation};
use crate::support::{body, chunks};

/// Runs a check over a backend of each kind that needs no service, each from empty.
macro_rules! over_each {
    ($check:path) => {{
        $check(&MemoryBackend::new()).await;
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "reservation-dir");
        $check(&DirBackend::open(dir.path()).unwrap()).await;
    }};
}

/// Asserts that a reservation heads as one, with its size and tag, reads as its bytes whole and
/// by range with the mark, is listed with its size, and that an absent key and a range past the
/// end are errors, on the in-memory and the directory backend.
#[tokio::test]
async fn a_reservation_is_marked_and_reads_as_its_bytes() {
    over_each!(conformance::a_reservation_is_marked_and_reads_as_its_bytes);
}

/// Asserts that a reservation and a put each refuse a key the other holds, and leave it as it
/// was, on the in-memory and the directory backend.
#[tokio::test]
async fn a_reservation_and_a_put_each_refuse_a_key_the_other_holds() {
    over_each!(conformance::a_reservation_and_a_put_each_refuse_a_key_the_other_holds);
}

/// Asserts that an empty object, one with the bytes of a reservation and a streamed one are not
/// reservations, on the in-memory and the directory backend.
#[tokio::test]
async fn a_plain_object_is_not_a_reservation() {
    over_each!(conformance::a_plain_object_is_not_a_reservation);
}

/// Asserts that a replace on the reservation's tag makes a real object and a replace on another
/// tag changes nothing, on the in-memory and the directory backend.
#[tokio::test]
async fn a_plain_replace_of_a_reservation_makes_a_real_object() {
    over_each!(conformance::a_plain_replace_of_a_reservation_makes_a_real_object);
}

/// Asserts that a marked replace keeps the mark and takes only the right tag, on the in-memory
/// and the directory backend.
#[tokio::test]
async fn a_marked_replace_keeps_the_mark_and_takes_only_the_tag() {
    over_each!(conformance::a_marked_replace_keeps_the_mark_and_takes_only_the_tag);
}

/// Asserts that a conditional delete takes a reservation only on its tag, on the in-memory and
/// the directory backend.
#[tokio::test]
async fn a_conditional_delete_takes_a_reservation_only_on_its_tag() {
    over_each!(conformance::a_conditional_delete_takes_a_reservation_only_on_its_tag);
}

/// A backend that does what the trait requires and no more: it holds nothing.
struct Bare;

#[async_trait]
impl Backend for Bare {
    async fn put(&self, key: &str, _body: Bytes) -> Result<(), Error> {
        Err(Error::AlreadyExists(key.to_owned()))
    }

    async fn put_if_match(&self, key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn get(&self, key: &str, _range: Option<ByteRange>) -> Result<Bytes, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn delete(&self, _key: &str) -> Result<(), Error> {
        Ok(())
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<Listed>, Error> {
        Ok(Vec::new())
    }
}

/// Asserts that a backend that does not override the calls of reservations, of the read with the
/// mark and of the streamed replacement refuses each with `Unsupported`, naming the call, and so
/// never makes up a read with the mark from a HEAD and a read, which could pair the bytes of one
/// object with the mark of another.
#[tokio::test]
async fn a_backend_without_reservations_refuses_the_calls_that_need_them() {
    let created = Bare.put_reservation("k", reservation(1)).await.unwrap_err();
    let replaced = Bare
        .put_reservation_if_match("k", reservation(1), "t")
        .await
        .unwrap_err();
    let read = Bare.get_tagged("k", None).await.unwrap_err();
    let ranged = Bare.get_tagged("k", Some(ByteRange::new(0, 1)));
    let ranged = ranged.await.unwrap_err();
    let streamed = Bare.put_stream_if_match("k", None, chunks(&["x"]), "t");
    let streamed = streamed.await.unwrap_err();
    let calls = [
        (created, "put_reservation"),
        (replaced, "put_reservation_if_match"),
        (read, "get_tagged"),
        (ranged, "get_tagged"),
        (streamed, "put_stream_if_match"),
    ];
    for (error, call) in calls {
        assert!(matches!(error, Error::Unsupported(_)), "{error}");
        assert!(error.to_string().contains(call), "{call}: {error}");
    }
}

fn faulty() -> FaultyBackend {
    FaultyBackend::new(Arc::new(MemoryBackend::new()))
}

/// Asserts that the fault injector counts a reservation, with or without a tag to match, as a call
/// of its own kind and not as a put, passes it on, fails the next one when told to, and leaves
/// the wrapped backend as it was when it does.
#[tokio::test]
async fn an_injector_counts_and_fails_reservations_as_a_kind_of_their_own() {
    let backend = faulty();
    let tag = backend.put_reservation("k", reservation(1)).await.unwrap();
    let renewed = backend.put_reservation_if_match("k", reservation(2), &tag);
    let renewed = renewed.await.unwrap();
    assert_eq!(backend.calls(Op::PutReservation), 2);
    assert_eq!(backend.calls(Op::Put), 0);
    assert_eq!(backend.head("k").await.unwrap().tag, renewed);

    backend.fail_next(Op::PutReservation, 2);
    let create = backend.put_reservation("j", reservation(1)).await;
    let replace = backend.put_reservation_if_match("k", reservation(3), &renewed);
    for failed in [create.unwrap_err(), replace.await.unwrap_err()] {
        assert!(failed.is_unavailable(), "{failed}");
    }
    let nothing = backend.head("j").await;
    assert_eq!(nothing, Err(Error::NotFound("j".to_owned())));
    assert_eq!(backend.head("k").await.unwrap().tag, renewed);
    assert_eq!(backend.calls(Op::PutReservation), 4);
}

/// Asserts that the fault injector counts a read with the mark as a get and records its range,
/// passes the mark through, and fails it when gets are told to fail.
#[tokio::test]
async fn an_injector_counts_a_read_with_the_mark_as_a_get() {
    let backend = faulty();
    let tag = backend.put_reservation("k", reservation(1)).await.unwrap();
    let range = Some(ByteRange::new(2, 3));
    let read = backend.get_tagged("k", range).await.unwrap();
    assert_eq!(read, (reservation(1).slice(2..5), true, tag));
    assert_eq!(backend.calls(Op::Get), 1);
    assert_eq!(backend.get_ranges(), [Some((2, 3))]);

    backend.fail_next(Op::Get, 1);
    let failed = backend.get_tagged("k", None).await.unwrap_err();
    assert!(failed.is_unavailable(), "{failed}");
}

/// Asserts that a RAM cache passes a reservation on to the backend under it, and that a marked
/// replace forgets what it had cached of the key: a read after it gives the new bytes.
#[tokio::test]
async fn a_cache_passes_reservations_on_and_forgets_a_replaced_key() {
    let cache = CachedStore::new(Arc::new(MemoryBackend::new()), 1 << 20);
    let tag = cache.put_reservation("k", reservation(1)).await.unwrap();
    assert!(cache.head("k").await.unwrap().reservation);
    assert_eq!(cache.get("k", None).await.unwrap(), reservation(1));
    assert_eq!(cache.cached_bytes(), 56);

    let new = cache.put_reservation_if_match("k", reservation(2), &tag);
    let new = new.await.unwrap();
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(cache.get("k", None).await.unwrap(), reservation(2));
    let stale = cache
        .put_reservation_if_match("k", reservation(3), &tag)
        .await;
    assert_eq!(stale, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(cache.head("k").await.unwrap().tag, new);
}

/// A backend over memory whose marked replacement is paused after it was committed: it tells
/// `committed` and returns only after `release` is notified, so a test holds the moment in which
/// the object under the key is new and the caller of the replacement has not heard of it.
struct Pausing {
    inner: MemoryBackend,
    committed: Notify,
    release: Notify,
}

#[async_trait]
impl Backend for Pausing {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn put_reservation_if_match(
        &self,
        key: &str,
        body: Bytes,
        etag: &str,
    ) -> Result<String, Error> {
        let tag = self.inner.put_reservation_if_match(key, body, etag).await?;
        self.committed.notify_one();
        self.release.notified().await;
        Ok(tag)
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn get_tagged(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, bool, String), Error> {
        self.inner.get_tagged(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}

/// Asserts that a read with the mark through a RAM cache is the backend's own read and never the
/// cache's copy: the cache holds the plain bytes A of a key, a marked replacement of the key is
/// committed in the backend and paused before the cache is told, and the read with the mark, whole
/// or by range, gives the bytes and the mark of the new object, while a plain read still gives the
/// copy A; once the replacement returns, plain reads give the new bytes too.
#[tokio::test]
async fn a_read_with_the_mark_through_a_cache_is_the_backends_read_and_not_its_copy() {
    let pausing = Pausing {
        inner: MemoryBackend::new(),
        committed: Notify::new(),
        release: Notify::new(),
    };
    let inner = Arc::new(pausing);
    let cache = Arc::new(CachedStore::new(inner.clone(), 1 << 20));
    inner.put("k", body("plain a")).await.unwrap();
    assert_eq!(cache.get("k", None).await.unwrap(), body("plain a"));
    let tag = cache.head("k").await.unwrap().tag;

    let racing = Arc::clone(&cache);
    let replacing = tokio::spawn(async move {
        racing
            .put_reservation_if_match("k", reservation(2), &tag)
            .await
    });
    inner.committed.notified().await;
    assert_eq!(cache.get("k", None).await.unwrap(), body("plain a"));
    let whole = cache.get_tagged("k", None).await.unwrap();
    assert_eq!((whole.0, whole.1), (reservation(2), true));
    let range = Some(ByteRange::new(0, 4));
    let part = cache.get_tagged("k", range).await.unwrap();
    assert_eq!((part.0, part.1), (reservation(2).slice(0..4), true));

    inner.release.notify_one();
    let new = replacing.await.unwrap().unwrap();
    assert_eq!((whole.2, part.2), (new.clone(), new.clone()));
    assert_eq!(cache.get("k", None).await.unwrap(), reservation(2));
    assert_eq!(cache.head("k").await.unwrap().tag, new);
}
