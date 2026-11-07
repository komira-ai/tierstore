//! The plain put of a cache that writes through (`put_stream_replacing`, a `PUT` with no
//! `If-None-Match`): what the upstream is sent, what the cache keeps and forgets, and what a read
//! finds afterwards, also after a restart. The races are driven by the gates of the upstream
//! fixture; no test waits for a time.

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use super::layout::{Sidecar, meta_path, name_of};
use super::metrics::{SkippedKeep, read};
use super::testing::{Rig, wait_until};
use crate::{Backend, Error, Op, PartBody, PutAnswer, PutClaim};

/// A body of one chunk holding `bytes`.
fn whole(bytes: &'static str) -> PartBody<'static> {
    let chunk = Bytes::from_static(bytes.as_bytes());
    stream::once(async move { Ok(chunk) }).boxed()
}

/// A plain put of `bytes` through the cache, of the declared length.
async fn plain(rig: &Rig, key: &str, bytes: &'static str) -> Result<Option<PutAnswer>, Error> {
    let len = Some(bytes.len() as u64);
    let claim = PutClaim::default();
    rig.cache
        .put_stream_replacing(key, len, whole(bytes), &claim)
        .await
}

/// The sidecar of the copy of `key` the cache holds.
fn sidecar(rig: &Rig, key: &str) -> Sidecar {
    let path = meta_path(&rig.config.path, &name_of(key).hex);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// A cache that has read the object `key` (holding "old") into its directory.
async fn holding_old(name: &str, key: &str, change: impl FnOnce(&mut super::CacheConfig)) -> Rig {
    let rig = Rig::with(name, |config| {
        config.write_through = true;
        change(config);
    });
    rig.put(key, b"old").await;
    assert_eq!(rig.get(key).await.unwrap(), "old");
    assert_eq!(rig.cache.held(), (3, 1));
    rig
}

/// Asserts that a plain put of a key the upstream holds, and the cache has read, replaces the
/// object (the answer is `Ok`, the upstream has the new bytes) with one put and no HEAD whether
/// `head_before_put` is on or off, and that the next read is the new bytes served from the disk:
/// no GET and no HEAD reach the upstream, the sidecar holds the tag the upstream gave the new
/// object, and it is a hit. A cache whose plain put is create-only is refused by the HEAD or by
/// the upstream's own answer.
#[tokio::test]
async fn a_plain_put_replaces_the_object_with_no_head_and_the_next_read_is_a_local_hit() {
    for on in [true, false] {
        let key = "cas/b/k";
        let rig = holding_old("tscache-replace-local", key, |config| {
            config.head_before_put = on;
        })
        .await;
        let (heads, gets, hits) = (
            rig.calls(Op::Head),
            rig.calls(Op::Get),
            read(&rig.cache.counters().hits),
        );
        assert_eq!(rig.calls(Op::Put), 0, "on {on}");

        assert!(matches!(plain(&rig, key, "new").await, Ok(None)), "on {on}");
        assert_eq!(rig.calls(Op::Put), 1, "on {on}");
        assert_eq!(rig.calls(Op::Head), heads, "on {on}");
        let upstream = rig.tamper.memory.get(key, None).await.unwrap();
        assert_eq!(upstream, Bytes::from_static(b"new"), "on {on}");

        assert_eq!(rig.get(key).await.unwrap(), "new", "on {on}");
        assert_eq!(rig.calls(Op::Get), gets, "on {on}");
        assert_eq!(rig.calls(Op::Head), heads, "on {on}");
        assert_eq!(read(&rig.cache.counters().hits), hits + 1, "on {on}");
        let told = rig.tamper.memory.head(key).await.unwrap();
        let kept = sidecar(&rig, key);
        assert_eq!((kept.tag, kept.size), (told.tag, 3), "on {on}");
        assert_eq!(rig.cache.held(), (3, 1), "on {on}");
        assert_eq!(rig.staged_files(), 0, "on {on}");
    }
}

/// Asserts that a create-only put of a key the upstream holds is still refused as
/// `AlreadyExists`, with the HEAD before it on or off, and that the cache's copy of the object is
/// neither dropped nor replaced: the next read is the old bytes, a hit.
#[tokio::test]
async fn a_create_only_put_of_a_held_key_is_refused_and_the_cached_copy_stays() {
    for on in [true, false] {
        let key = "cas/b/k";
        let rig = holding_old("tscache-replace-create", key, |config| {
            config.head_before_put = on;
        })
        .await;
        let gets = rig.calls(Op::Get);

        let made = rig.cache.put_stream(key, Some(3), whole("new")).await;
        assert_eq!(made, Err(Error::AlreadyExists(key.to_owned())), "on {on}");
        assert_eq!(rig.get(key).await.unwrap(), "old", "on {on}");
        assert_eq!(rig.calls(Op::Get), gets, "on {on}");
        assert_eq!(rig.cache.held(), (3, 1), "on {on}");
    }
}

/// Asserts that a plain put whose copy is not kept (a body of no declared length cannot be
/// reserved for) still drops the cache's old copy, in the index and on the disk, and that after a
/// restart a read is the new bytes from the upstream and not the old ones from the directory.
#[tokio::test]
async fn a_plain_put_that_keeps_nothing_drops_the_old_copy_also_after_a_restart() {
    let key = "cas/b/k";
    let rig = holding_old("tscache-replace-unkept", key, |_| {}).await;
    let claim = PutClaim::default();
    let made = rig
        .cache
        .put_stream_replacing(key, None, whole("new"), &claim)
        .await;
    assert!(matches!(made, Ok(None)), "{made:?}");
    assert_eq!(rig.cache.counters().keeps_skipped(SkippedKeep::Budget), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(key));

    let restarted = rig.reopen();
    assert_eq!(restarted.held(), (0, 0));
    let read = restarted.get(key, None).await.unwrap();
    assert_eq!(read, Bytes::from_static(b"new"));
}

/// Asserts that the copy a plain put keeps is what a restarted cache serves, from its directory
/// and with no call of the upstream: the old bytes are not in the directory any more.
#[tokio::test]
async fn the_copy_of_a_plain_put_is_served_by_a_restarted_cache_and_the_old_one_is_gone() {
    let key = "cas/b/k";
    let rig = holding_old("tscache-replace-restart", key, |_| {}).await;
    assert!(matches!(plain(&rig, key, "new").await, Ok(None)));
    let (gets, heads) = (rig.calls(Op::Get), rig.calls(Op::Head));

    let restarted = rig.reopen();
    assert_eq!(restarted.held(), (3, 1));
    let read = restarted.get(key, None).await.unwrap();
    assert_eq!(read, Bytes::from_static(b"new"));
    assert_eq!((rig.calls(Op::Get), rig.calls(Op::Head)), (gets, heads));
}

/// Asserts that a plain put the upstream refuses (or whose answer is lost after it committed)
/// leaves the cache without its old copy: it is forgotten before the put, so that the next read
/// goes to the upstream, which says what the key holds now.
#[tokio::test]
async fn a_plain_put_that_fails_leaves_no_old_copy_in_the_cache() {
    let key = "cas/b/k";
    let rig = holding_old("tscache-replace-fails", key, |_| {}).await;
    rig.upstream
        .fail_next_with(Op::Put, Error::Unavailable("503".to_owned()));

    let made = plain(&rig, key, "new").await;
    assert_eq!(made, Err(Error::Unavailable("503".to_owned())));
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(key));
    let gets = rig.calls(Op::Get);
    assert_eq!(rig.get(key).await.unwrap(), "old");
    assert_eq!(rig.calls(Op::Get), gets + 1);
}

/// Asserts that a read that is filling the key from the old object while a plain put replaces it
/// does not put the old object back: the fill is told the object was replaced and keeps nothing,
/// and the next read is the new bytes. A cache that forgot the key only before the put would
/// keep the old bytes for good.
#[tokio::test]
async fn a_fill_that_read_the_old_object_does_not_keep_it_over_a_plain_put() {
    let key = "cas/b/k";
    let rig = Rig::writing("tscache-replace-fill");
    rig.put(key, b"old").await;
    let parked = rig.tamper.hold_key_after_read(key);
    let (cache, name) = (rig.cache.clone(), key.to_owned());
    let reader = tokio::spawn(async move { cache.get(&name, None).await });
    wait_until("the fill read the object and parked", || {
        rig.tamper.parked() >= 1
    })
    .await;

    assert!(matches!(plain(&rig, key, "new").await, Ok(None)));
    parked.add_permits(16);
    drop(reader.await.unwrap());
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(key));

    assert_eq!(rig.get(key).await.unwrap(), "new");
    assert_eq!(rig.cache.held(), (3, 1));
    assert_eq!(sidecar(&rig, key).size, 3);
}

/// Asserts that two plain puts of one key at once keep no copy: the second begins after the
/// first has replaced the object and before it is answered, so the order in which the upstream
/// took them is not known to the cache. Both keeps are skipped, the cache holds
/// nothing, and the next read is what the upstream holds (the second's bytes), which it then
/// keeps. A cache that kept the first put's copy would serve "aaa" over the second's "bbb".
#[tokio::test]
async fn two_plain_puts_of_a_key_at_once_keep_no_copy() {
    let key = "cas/b/k";
    let rig = Rig::writing("tscache-replace-overlap");
    rig.put(key, b"old").await;
    let held = rig.tamper.hold_replacing();
    let (first, second) = (rig.cache.clone(), rig.cache.clone());
    let claim = PutClaim::default();
    let (one, other) = (claim.clone(), claim);
    let a = tokio::spawn(async move {
        first
            .put_stream_replacing(key, Some(3), whole("aaa"), &one)
            .await
    });
    wait_until("the first put replaced the object", || {
        rig.tamper.replaced() >= 1
    })
    .await;
    let b = tokio::spawn(async move {
        second
            .put_stream_replacing(key, Some(3), whole("bbb"), &other)
            .await
    });
    wait_until("the second put replaced the object", || {
        rig.tamper.replaced() >= 2
    })
    .await;

    held.add_permits(2);
    assert!(matches!(a.await.unwrap(), Ok(None)));
    assert!(matches!(b.await.unwrap(), Ok(None)));
    // The keeps run at once: one is told the puts overlapped, and the other may find the first
    // one's place taken.
    let counters = rig.cache.counters();
    let skipped =
        counters.keeps_skipped(SkippedKeep::Deleted) + counters.keeps_skipped(SkippedKeep::Busy);
    assert_eq!(skipped, 2);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(key));
    assert!(super::locked(&rig.cache.inner.writers).is_empty());

    assert_eq!(rig.get(key).await.unwrap(), "bbb");
    assert_eq!(rig.cache.held(), (3, 1));
}

/// Asserts which puts of one object are told they overlapped: two create-only puts never are (one
/// of them is refused by the upstream, and the other's copy is right), a plain put that begins
/// beside a create-only one is, and so is a create-only put that begins beside a plain one;
/// the record of the object is gone once the last of its puts ends.
#[test]
fn only_a_plain_put_makes_the_puts_of_an_object_overlap() {
    let rig = Rig::writing("tscache-replace-registry");
    let inner = &rig.cache.inner;

    let (first, second) = (inner.begin_write("h", false), inner.begin_write("h", false));
    assert!(!first.overlapped() && !second.overlapped());
    let plain_one = inner.begin_write("h", true);
    assert!(first.overlapped() && second.overlapped() && plain_one.overlapped());
    drop((first, second, plain_one));
    assert!(super::locked(&inner.writers).is_empty());

    let plain_first = inner.begin_write("h", true);
    assert!(!plain_first.overlapped());
    let create = inner.begin_write("h", false);
    assert!(plain_first.overlapped() && create.overlapped());
    drop(plain_first);
    assert!(create.overlapped());
    drop(create);
    assert!(super::locked(&inner.writers).is_empty());
}
