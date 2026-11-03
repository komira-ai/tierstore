//! A cache over an upstream of two tiers (`tiered_upstream`): how it tells versions of an object
//! apart, how it sends a hot-only put, and that it still checks a create-only put with a HEAD.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use super::LocalCache;
use super::testing::{Rig, pattern};
use crate::{Backend, ByteRange, Error, Listed, Meta, Op, PartBody};

/// An upstream over the rig's that counts the two kinds of streamed put, and, once a read has
/// been made, tags every object differently: the way a proxy's tag moves when it copies an
/// object from its cold tier to its hot one. With `resize` it also reports every object one byte
/// larger after a read: an object replaced by another of a different size.
struct Probe {
    inner: Arc<dyn Backend>,
    resize: bool,
    plain: AtomicUsize,
    hot_only: AtomicUsize,
    reads: AtomicUsize,
}

impl Probe {
    fn over(inner: Arc<dyn Backend>) -> Arc<Self> {
        Self::made(inner, false)
    }

    fn resizing(inner: Arc<dyn Backend>) -> Arc<Self> {
        Self::made(inner, true)
    }

    fn made(inner: Arc<dyn Backend>, resize: bool) -> Arc<Self> {
        Arc::new(Self {
            inner,
            resize,
            plain: AtomicUsize::new(0),
            hot_only: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Backend for Probe {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        let _ = self.plain.fetch_add(1, Ordering::SeqCst);
        self.inner.put_stream(key, len, body).await
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        let _ = self.hot_only.fetch_add(1, Ordering::SeqCst);
        self.inner.put_stream_hot_only(key, len, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let _ = self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let mut meta = self.inner.head(key).await?;
        if self.reads.load(Ordering::SeqCst) > 0 {
            meta.tag = format!("moved:{}", meta.tag);
            if self.resize {
                meta.size += 1;
            }
        }
        Ok(meta)
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        self.inner.list_page(prefix, start_after, max_keys).await
    }
}

/// A body of one chunk.
fn body(bytes: &'static [u8]) -> PartBody<'static> {
    stream::once(async move { Ok(Bytes::from_static(bytes)) }).boxed()
}

/// A cache over `probe` in its own directory under the rig's, with the rig's settings changed by
/// `change`.
fn cache_over(
    rig: &Rig,
    probe: &Arc<Probe>,
    change: impl FnOnce(&mut super::CacheConfig),
) -> LocalCache {
    let mut config = rig.config.clone();
    config.path = rig.dir.path().join("probed");
    config.write_through = true;
    change(&mut config);
    let upstream: Arc<dyn Backend> = probe.clone();
    LocalCache::open(config, upstream, rig.disk.clone(), rig.clock.clone()).expect("a cache")
}

fn meta(size: u64, tag: &str) -> Meta {
    Meta {
        size,
        tag: tag.to_owned(),
        modified: 0,
    }
}

/// Asserts the two readings of "one version": the same size and tag by default, and the same
/// size alone for a cache over two tiers, whose tags move with the tier.
#[tokio::test]
async fn versions_are_told_apart_by_tag_and_size_unless_the_upstream_has_two_tiers() {
    let rig = Rig::new("tscache-version");
    let by_tag = rig.cache.inner.same_version(&meta(3, "a"), &meta(3, "a"));
    assert!(by_tag);
    assert!(!rig.cache.inner.same_version(&meta(3, "a"), &meta(3, "b")));
    assert!(!rig.cache.inner.same_version(&meta(3, "a"), &meta(4, "a")));

    let tiered = rig.reopen_with(|config| config.tiered_upstream = true);
    assert!(tiered.inner.same_version(&meta(3, "a"), &meta(3, "a")));
    assert!(tiered.inner.same_version(&meta(3, "a"), &meta(3, "b")));
    assert!(!tiered.inner.same_version(&meta(3, "a"), &meta(4, "a")));
}

/// Asserts that a read of an object whose tag moves between the HEAD and the read is answered
/// from the upstream and kept nowhere by a cache of the default kind (its fill ends as changed,
/// and each read goes to the upstream again), and filled and kept by one over two tiers, which
/// then answers the next read from its directory. The first half is what the option removes.
#[tokio::test]
async fn a_fill_of_an_object_whose_tag_moves_is_served_only_when_the_upstream_has_two_tiers() {
    let rig = Rig::new("tscache-moved");
    let data = pattern(3000);
    rig.put("cas/b/obj", &data).await;
    let upstream: Arc<dyn Backend> = rig.upstream.clone();

    let plain = Probe::over(Arc::clone(&upstream));
    let unkept = cache_over(&rig, &plain, |_| {});
    assert_eq!(unkept.get("cas/b/obj", None).await.unwrap(), data);
    assert_eq!(unkept.held(), (0, 0));
    let reads = plain.reads.load(Ordering::SeqCst);
    assert_eq!(unkept.get("cas/b/obj", None).await.unwrap(), data);
    assert!(plain.reads.load(Ordering::SeqCst) > reads);

    let moving = Probe::over(upstream);
    let cache = cache_over(&rig, &moving, |config| {
        config.path = config.path.with_file_name("tiered");
        config.tiered_upstream = true;
    });
    assert_eq!(cache.get("cas/b/obj", None).await.unwrap(), data);
    assert_eq!(cache.held(), (3000, 1));
    let reads = moving.reads.load(Ordering::SeqCst);
    assert_eq!(cache.get("cas/b/obj", None).await.unwrap(), data);
    assert_eq!(moving.reads.load(Ordering::SeqCst), reads);
}

/// Asserts the same for a ranged read of an object larger than the whole-fill limit, which is
/// passed through in windows and checked against the version it began with at its last window.
#[tokio::test]
async fn a_windowed_read_of_an_object_whose_tag_moves_is_served_only_when_the_upstream_has_two_tiers()
 {
    let rig = Rig::new("tscache-moved-window");
    let data = pattern(40);
    rig.put("cas/b/big", &data).await;
    let upstream: Arc<dyn Backend> = rig.upstream.clone();
    let small = |config: &mut super::CacheConfig| {
        config.whole_limit = 4;
        config.fill_chunk = 8;
    };

    let plain = Probe::over(Arc::clone(&upstream));
    let asked = Some(ByteRange::new(0, 20));
    let refused = cache_over(&rig, &plain, small)
        .get("cas/b/big", asked)
        .await;
    assert!(matches!(refused, Err(Error::Unavailable(_))), "{refused:?}");

    let moving = Probe::over(upstream);
    let cache = cache_over(&rig, &moving, |config| {
        small(config);
        config.path = config.path.with_file_name("tiered");
        config.tiered_upstream = true;
    });
    assert_eq!(cache.get("cas/b/big", asked).await.unwrap(), &data[..20]);
}

/// Asserts that a hot-only put through the cache reaches the upstream as a hot-only put and a
/// plain streamed put as a plain one, under a key the cache keeps and under one it does not.
#[tokio::test]
async fn a_hot_only_put_reaches_the_upstream_as_a_hot_only_put() {
    let rig = Rig::new("tscache-hot-only");
    let upstream: Arc<dyn Backend> = rig.upstream.clone();
    let probe = Probe::over(upstream);
    let cache = cache_over(&rig, &probe, |_| {});

    cache
        .put_stream_hot_only("other/one", Some(1), body(b"1"))
        .await
        .unwrap();
    cache
        .put_stream_hot_only("cas/b/two", Some(1), body(b"2"))
        .await
        .unwrap();
    assert_eq!(probe.hot_only.load(Ordering::SeqCst), 2);
    assert_eq!(probe.plain.load(Ordering::SeqCst), 0);

    cache
        .put_stream("other/three", Some(1), body(b"3"))
        .await
        .unwrap();
    cache
        .put_stream("cas/b/four", Some(1), body(b"4"))
        .await
        .unwrap();
    assert_eq!(probe.hot_only.load(Ordering::SeqCst), 2);
    assert_eq!(probe.plain.load(Ordering::SeqCst), 2);
}

/// Asserts that a read cache refuses a hot-only put as it refuses every write, and that a
/// reserved key is refused as it is for a plain put, with nothing sent to the upstream.
#[tokio::test]
async fn a_hot_only_put_is_refused_by_a_read_cache_and_for_a_reserved_key() {
    let rig = Rig::new("tscache-hot-only-refused");
    let read_only = rig
        .cache
        .put_stream_hot_only("cas/b/a", Some(1), body(b"a"))
        .await;
    assert!(matches!(read_only, Err(Error::Unsupported(_))));

    let writing = rig.reopen_with(|config| config.write_through = true);
    let reserved = writing
        .put_stream_hot_only("_tiering/x", Some(1), body(b"x"))
        .await;
    assert_eq!(reserved, Err(Error::Reserved("_tiering/x".to_owned())));
    assert_eq!(rig.calls(Op::Put), 0);
    assert_eq!(rig.calls(Op::Head), 0);
}

/// Asserts that a create-only put is checked with a HEAD of the key first, with and without the
/// option: a key that is there is refused by the HEAD, no byte of the body is read and no put is
/// sent, and the refusal is counted by the cache as one for an object that exists.
#[tokio::test]
async fn a_put_over_two_tiers_is_still_checked_with_a_head_first() {
    for tiered in [false, true] {
        let rig = Rig::with("tscache-head", |config| {
            config.write_through = true;
            config.tiered_upstream = tiered;
        });
        rig.cache
            .put_stream("other/a", Some(1), body(b"a"))
            .await
            .unwrap();
        assert_eq!(rig.calls(Op::Head), 1);
        let read = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&read);
        let unread = stream::once(async move {
            let _ = counted.fetch_add(1, Ordering::SeqCst);
            Ok(Bytes::from_static(b"b"))
        })
        .boxed();
        let again = rig.cache.put_stream("other/a", Some(1), unread).await;
        assert_eq!(again, Err(Error::AlreadyExists("other/a".to_owned())));
        assert_eq!(rig.calls(Op::Head), 2);
        assert_eq!(rig.calls(Op::Put), 1);
        assert_eq!(read.load(Ordering::SeqCst), 0);
    }
}

/// Asserts that an object replaced by one of another size during a fill is refused as written
/// over by a cache over two tiers, although it compares versions by size alone, and that the
/// cache keeps nothing of it.
#[tokio::test]
async fn a_fill_of_an_object_replaced_by_one_of_another_size_is_refused_as_written_over() {
    let rig = Rig::new("tscache-resized");
    let data = pattern(3000);
    rig.put("cas/b/obj", &data).await;
    let upstream: Arc<dyn Backend> = rig.upstream.clone();
    let resized = Probe::resizing(upstream);
    let cache = cache_over(&rig, &resized, |config| config.tiered_upstream = true);
    let refused = cache.get("cas/b/obj", None).await;
    assert!(matches!(refused, Err(Error::Unavailable(_))), "{refused:?}");
    assert_eq!(cache.held(), (0, 0));
}
