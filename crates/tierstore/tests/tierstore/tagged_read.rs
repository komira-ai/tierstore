//! The read with its tag as a backend gives it: the memory and the directory backend held to the
//! check every backend passes, the call the trait refuses for a backend that does not do it, and
//! the wrappers that pass it on (the fault injector, the RAM cache).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{
    Backend, ByteRange, CachedStore, DirBackend, Error, FaultyBackend, Listed, MemoryBackend, Meta,
    Op,
};
use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;

use crate::conformance;
use crate::support::{body, chunks};

/// Runs a check over a backend of each kind that needs no service, each from empty.
macro_rules! over_each {
    ($check:path) => {{
        $check(&MemoryBackend::new()).await;
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tagged-read-dir");
        $check(&DirBackend::open(dir.path()).unwrap()).await;
    }};
}

/// Asserts that a read with its tag gives the bytes and the tag a HEAD gives, whole and by range,
/// that an absent key is `NotFound` and a range past the end an error, on the in-memory and the
/// directory backend.
#[tokio::test]
async fn a_tagged_read_gives_the_bytes_and_the_tag_of_one_object() {
    over_each!(conformance::a_tagged_read_gives_the_bytes_and_the_tag_of_one_object);
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

/// Asserts that a backend that does not override the read with its tag and the streamed
/// replacement refuses each with `Unsupported`, naming the call, and so never makes up a read
/// with its tag from a HEAD and a read, which could pair the bytes of one object with the tag of
/// another.
#[tokio::test]
async fn a_backend_without_a_tagged_read_refuses_the_calls_that_need_one() {
    let read = Bare.get_tagged("k", None).await.unwrap_err();
    let ranged = Bare.get_tagged("k", Some(ByteRange::new(0, 1)));
    let ranged = ranged.await.unwrap_err();
    let streamed = Bare.put_stream_if_match("k", None, chunks(&["x"]), "t");
    let streamed = streamed.await.unwrap_err();
    let calls = [
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

/// Asserts that the fault injector counts a read with its tag as a get and records its range,
/// passes the tag through, and fails it when gets are told to fail.
#[tokio::test]
async fn an_injector_counts_a_tagged_read_as_a_get() {
    let backend = faulty();
    let tag = backend.put_tagged("k", body("0123456789")).await.unwrap();
    let range = Some(ByteRange::new(2, 3));
    let read = backend.get_tagged("k", range).await.unwrap();
    assert_eq!(read, (body("234"), tag));
    assert_eq!(backend.calls(Op::Get), 1);
    assert_eq!(backend.get_ranges(), [Some((2, 3))]);

    backend.fail_next(Op::Get, 1);
    let failed = backend.get_tagged("k", None).await.unwrap_err();
    assert!(failed.is_unavailable(), "{failed}");
}

/// A backend over memory whose replacement is paused after it was committed: it tells
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
        let tag = self.inner.put_if_match(key, body, etag).await?;
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
    ) -> Result<(Bytes, String), Error> {
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

/// Asserts that a read with its tag through a RAM cache is the backend's own read and never the
/// cache's copy: the cache holds the bytes A of a key, a replacement of the key is committed in
/// the backend and paused before the cache is told, and the read with its tag, whole or by range,
/// gives the bytes and the tag of the new object, while a plain read still gives the copy A; once
/// the replacement returns, plain reads give the new bytes too.
#[tokio::test]
async fn a_tagged_read_through_a_cache_is_the_backends_read_and_not_its_copy() {
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
    let replacing =
        tokio::spawn(async move { racing.put_if_match("k", body("plain b"), &tag).await });
    inner.committed.notified().await;
    assert_eq!(cache.get("k", None).await.unwrap(), body("plain a"));
    let whole = cache.get_tagged("k", None).await.unwrap();
    assert_eq!(whole.0, body("plain b"));
    let range = Some(ByteRange::new(0, 4));
    let part = cache.get_tagged("k", range).await.unwrap();
    assert_eq!(part.0, body("plai"));

    inner.release.notify_one();
    let new = replacing.await.unwrap().unwrap();
    assert_eq!((whole.1, part.1), (new.clone(), new.clone()));
    assert_eq!(cache.get("k", None).await.unwrap(), body("plain b"));
    assert_eq!(cache.head("k").await.unwrap().tag, new);
}
