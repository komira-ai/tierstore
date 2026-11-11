//! The prefetch queue: a blob queued has its place in the table of the fills, a read that finds it
//! starts it at once on a permit of its own pool, the workers have a pool of their own, and a blob
//! is dropped when its turn comes if it waited too long, is there already, or does not fit. The
//! upstream is held at a gate so that each read is made while a fill is in the middle, and the
//! clock is the test's.

use std::sync::Arc;
use std::time::Duration;

use futures::poll;

use super::fill::Stop;
use super::locked;
use super::metrics::{Evicted, PrefetchFailed, read};
use super::pack_testing::{Pack, distinct};
use super::prefetch::Prefetched;
use super::source::Blob;
use super::testing::{Rig, digest, until};
use crate::{Error, Op};

/// An object of the upstream named by its digest.
struct Stored {
    key: String,
    sha: String,
    body: Vec<u8>,
}

impl Stored {
    /// The blob as the cache is asked for it: the whole object.
    fn blob(&self) -> Blob<'_> {
        Blob {
            sha256: &self.sha,
            size: self.body.len() as u64,
            from: &self.key,
            offset: 0,
        }
    }
}

/// The object `cas/l/<sha256>` of `len` bytes, put in the upstream; `seed` makes it differ from
/// the others.
async fn stored(rig: &Rig, seed: usize, len: usize) -> Stored {
    let body = distinct(seed, len);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    Stored { key, sha, body }
}

/// A cache with `workers` prefetch workers.
fn rig(name: &str, workers: usize) -> Rig {
    Rig::with(name, |config| config.prefetch.concurrency = workers)
}

/// The permits of the fill socket's pool, the S3 front's and the workers', free now.
fn free(rig: &Rig) -> (usize, usize, usize) {
    let inner = &rig.cache.inner;
    (
        inner.source_permits.available_permits(),
        inner.permits.available_permits(),
        inner.prefetch.workers.available_permits(),
    )
}

/// The blobs that wait for a worker.
fn waiting(rig: &Rig) -> usize {
    locked(&rig.cache.inner.prefetch.queue).len()
}

/// The fills in the table, queued or running.
fn in_table(rig: &Rig) -> usize {
    locked(&rig.cache.inner.flights).len()
}

/// Asserts that a prefetch fill is joined by a request of the fill socket (the mount's open) and
/// by a read of the S3 front that come while it runs: the answer of the prefetch comes before the
/// upstream is asked for anything, neither reader takes a permit, the upstream is read once, and
/// both are given the bytes. It would catch a prefetch whose fill a read does not find.
#[tokio::test]
async fn prefetch_fill_is_joined_by_mount_open_and_s3_get() {
    let rig = rig("tprefetch-joined", 1);
    let a = stored(&rig, 1, 2000).await;
    let all = free(&rig);
    let gate = rig.tamper.hold();

    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    assert_eq!(rig.calls(Op::Get), 0);
    until("the worker is at the upstream", || rig.calls(Op::Get) == 1).await;

    let blob = a.blob();
    let mut mount = Box::pin(rig.cache.fill_blob(&blob));
    assert!(poll!(mount.as_mut()).is_pending());
    let mut s3 = Box::pin(rig.get(&a.key));
    assert!(poll!(s3.as_mut()).is_pending());
    assert_eq!(free(&rig), (all.0, all.1, all.2 - 1));

    gate.add_permits(64);
    assert_eq!(mount.await, Ok(()));
    assert_eq!(s3.await.unwrap(), a.body);
    assert_eq!(rig.calls_on(Op::Get, &a.key), 1);
    assert_eq!(rig.cache.held(), (2000, 1));
    until("the permits are back", || free(&rig) == all).await;
    let counters = rig.cache.counters();
    assert_eq!((counters.joins(false), counters.joins(true)), (1, 1));
    assert_eq!(read(&counters.prefetch_fills), 1);
    assert_eq!(read(&counters.fills), 1);
    assert_eq!(in_table(&rig), 0);
}

/// Asserts that a request of the fill socket for a blob that waits in the queue starts it at once,
/// on a permit of the fill socket's pool, ahead of the blobs before it: with the only worker held
/// at another blob, the upstream is read for the queued blob once, by the request, the queue
/// forgets it, and the worker later finds nothing to start. It would catch a read that waits
/// behind the queue, and one that fetches the blob again when the worker comes to it.
#[tokio::test]
async fn read_of_queued_prefetch_blob_promotes_it() {
    let rig = rig("tprefetch-promote", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let all = free(&rig);
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Queued);
    assert_eq!((waiting(&rig), in_table(&rig)), (1, 2));
    assert_eq!(rig.calls_on(Op::Get, &b.key), 0);

    assert_eq!(rig.cache.fill_blob(&b.blob()).await, Ok(()));
    assert_eq!(rig.calls_on(Op::Get, &b.key), 1);
    assert_eq!(waiting(&rig), 0);
    assert_eq!(rig.cache.held(), (2000, 1));
    assert_eq!(read(&rig.cache.counters().prefetch_promotions), 1);

    gate.add_permits(64);
    until("the worker kept a", || rig.cache.held() == (5000, 2)).await;
    until("the permits are back", || free(&rig) == all).await;
    assert_eq!(rig.calls_on(Op::Get, &b.key), 1);
    // Only the fill the worker ran is the worker's.
    assert_eq!(read(&rig.cache.counters().prefetch_fills), 1);
    assert_eq!(in_table(&rig), 0);
}

/// Asserts the same for a `GET` of the S3 front, on a permit of the S3 pool: it is given the
/// bytes of the fill it started, and the pool has its permit back when the fill ends.
#[tokio::test]
async fn s3_get_of_queued_prefetch_blob_promotes_it() {
    let rig = rig("tprefetch-promote-s3", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let all = free(&rig);
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Queued);

    assert_eq!(rig.get(&b.key).await.unwrap(), b.body);
    assert_eq!(rig.calls_on(Op::Get, &b.key), 1);
    assert_eq!(waiting(&rig), 0);
    assert_eq!(read(&rig.cache.counters().prefetch_promotions), 1);

    gate.add_permits(64);
    until("the worker kept a", || rig.cache.held() == (5000, 2)).await;
    until("the permits are back", || free(&rig) == all).await;
    assert_eq!(rig.calls_on(Op::Get, &b.key), 1);
}

/// Asserts that an S3 read that finds a queued blob and every permit of the S3 pool taken is
/// answered from the upstream like any read that finds none, and leaves the blob queued: the
/// worker fills it when it is free. It would catch a read that takes the blob out of the queue and
/// then fills nothing.
#[tokio::test]
async fn s3_get_of_queued_blob_with_no_permit_is_served_upstream_and_leaves_it_queued() {
    let rig = rig("tprefetch-promote-busy", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Queued);
    let pool = rig.cache.inner.permits.available_permits();
    let taken = Arc::clone(&rig.cache.inner.permits).try_acquire_many_owned(pool as u32);
    let taken = taken.unwrap();

    assert_eq!(rig.get(&b.key).await.unwrap(), b.body);
    assert_eq!(rig.calls_on(Op::Get, &b.key), 1);
    assert_eq!(waiting(&rig), 1);
    assert_eq!(read(&rig.cache.counters().fills_skipped_busy), 1);
    assert_eq!(read(&rig.cache.counters().prefetch_promotions), 0);

    drop(taken);
    gate.add_permits(64);
    until("the worker kept both", || rig.cache.held() == (5000, 2)).await;
    assert_eq!(rig.calls_on(Op::Get, &b.key), 2);
}

/// Asserts that a queued blob cut out of a pack is no object of the upstream for the S3 front,
/// as a running one is not: a `GET` of the digest is answered by the upstream (not found) and
/// leaves the blob queued; the mount's request for it starts it.
#[tokio::test]
async fn s3_get_of_a_queued_span_goes_upstream_and_leaves_it_queued() {
    let rig = rig("tprefetch-span", 1);
    let a = stored(&rig, 1, 3000).await;
    let pack = Pack::put(&rig, "packs/queued", &[900, 700]).await;
    let key = format!("cas/l/{}", pack.blobs[0].0);
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.prefetch(&pack.blob(0)), Prefetched::Queued);

    assert_eq!(rig.get(&key).await, Err(Error::NotFound(key.clone())));
    assert_eq!(waiting(&rig), 1);
    assert_eq!(read(&rig.cache.counters().prefetch_promotions), 0);

    assert_eq!(rig.cache.fill_blob(&pack.blob(0)).await, Ok(()));
    assert_eq!(waiting(&rig), 0);
    assert_eq!(read(&rig.cache.counters().prefetch_promotions), 1);
    gate.add_permits(64);
    until("the worker kept a", || rig.cache.held() == (3900, 2)).await;
}

/// Asserts that the reads and the prefetch each have a pool of permits of their own: with every
/// permit of the fill socket's pool and of the S3 pool taken, a prefetch fills; with the only
/// worker taken, a request of the fill socket and a read of the S3 front fill.
#[tokio::test]
async fn foreground_and_prefetch_use_their_own_permits() {
    let rig = rig("tprefetch-pools", 1);
    let blobs = [
        stored(&rig, 1, 300).await,
        stored(&rig, 2, 400).await,
        stored(&rig, 3, 500).await,
    ];
    let inner = &rig.cache.inner;
    let (mount, s3) = (&inner.source_permits, &inner.permits);
    let mount = Arc::clone(mount).try_acquire_many_owned(mount.available_permits() as u32);
    let s3 = Arc::clone(s3).try_acquire_many_owned(s3.available_permits() as u32);
    let (mount, s3) = (mount.unwrap(), s3.unwrap());
    assert_eq!(rig.cache.prefetch(&blobs[0].blob()), Prefetched::Queued);
    until("the worker kept it", || rig.cache.held() == (300, 1)).await;
    drop((mount, s3));

    let worker = Arc::clone(&inner.prefetch.workers).try_acquire_owned();
    let worker = worker.unwrap();
    assert_eq!(rig.cache.fill_blob(&blobs[1].blob()).await, Ok(()));
    assert_eq!(rig.get(&blobs[2].key).await.unwrap(), blobs[2].body);
    assert_eq!(rig.cache.held(), (1200, 3));
    drop(worker);
    assert_eq!(rig.calls(Op::Get), 3);
}

/// Asserts that a prefetch of a blob that has a fill queued or running joins it and makes no
/// second one, and that a blob the cache holds costs no request.
#[tokio::test]
async fn prefetch_of_a_blob_that_is_queued_running_or_present_makes_no_second_fetch() {
    let rig = rig("tprefetch-twice", 1);
    let (a, b, c) = (
        stored(&rig, 1, 300).await,
        stored(&rig, 2, 400).await,
        stored(&rig, 3, 500).await,
    );
    assert_eq!(rig.cache.fill_blob(&c.blob()).await, Ok(()));
    let gets = rig.calls(Op::Get);
    let gate = rig.tamper.hold();
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Queued);

    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Joined);
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Joined);
    assert_eq!(rig.cache.prefetch(&c.blob()), Prefetched::Present);
    assert_eq!(waiting(&rig), 1);
    gate.add_permits(64);
    until("both are kept", || rig.cache.held() == (1200, 3)).await;
    assert_eq!(rig.calls(Op::Get), gets + 2);
    let counters = rig.cache.counters();
    assert_eq!(counters.blobs(Prefetched::Joined), 2);
    assert_eq!(counters.blobs(Prefetched::Present), 1);
}

/// Asserts the answers that need no queue: `prefetch_concurrency` of 0 turns it off, a cache that
/// does not hold the lock of its directory fills nothing, and a blob larger than the budget is
/// turned away; none of them puts anything in the table.
#[tokio::test]
async fn a_blob_is_turned_away_when_the_queue_is_off_the_cache_is_read_only_or_it_is_too_large() {
    let off = rig("tprefetch-off", 0);
    let a = stored(&off, 1, 300).await;
    assert_eq!(off.cache.prefetch(&a.blob()), Prefetched::Off);
    assert_eq!(in_table(&off), 0);

    let rig = Rig::with("tprefetch-ro", |config| config.budget = 1000);
    let big = stored(&rig, 2, 1001).await;
    assert_eq!(rig.cache.prefetch(&big.blob()), Prefetched::TooLarge);
    let _holder = rig.locked();
    let reader = rig.locked();
    let a = stored(&rig, 1, 300).await;
    assert_eq!(reader.prefetch(&a.blob()), Prefetched::ReadOnly);
    assert_eq!(locked(&reader.inner.flights).len(), 0);
    assert_eq!(reader.held(), (0, 0));
    assert_eq!(rig.calls(Op::Get), 0);
    assert_eq!(reader.counters().blobs(Prefetched::ReadOnly), 1);
    assert_eq!(rig.cache.counters().blobs(Prefetched::TooLarge), 1);
    assert_eq!(off.cache.counters().blobs(Prefetched::Off), 1);
}

/// Asserts the bound of the queue: with the only worker at a blob and the queue of two, the next
/// blob is turned away `busy`, leaves no place in the table, and is accepted once the queue has
/// room again.
#[tokio::test]
async fn queue_bound_busy() {
    let rig = Rig::with("tprefetch-busy", |config| {
        config.prefetch.concurrency = 1;
        config.prefetch.queue = 2;
    });
    let blobs = [
        stored(&rig, 1, 100).await,
        stored(&rig, 2, 110).await,
        stored(&rig, 3, 120).await,
        stored(&rig, 4, 130).await,
    ];
    let gate = rig.tamper.hold();
    let answers = blobs
        .each_ref()
        .map(|blob| rig.cache.prefetch(&blob.blob()));
    until("the worker is at the first", || rig.calls(Op::Get) == 1).await;
    use Prefetched::{Busy, Queued};
    assert_eq!(answers, [Queued, Queued, Queued, Busy]);
    assert_eq!((waiting(&rig), in_table(&rig)), (2, 3));
    assert_eq!(rig.cache.counters().blobs(Busy), 1);

    gate.add_permits(64);
    until("the three are kept", || rig.cache.held() == (330, 3)).await;
    assert_eq!(rig.cache.prefetch(&blobs[3].blob()), Queued);
    until("the fourth is kept", || rig.cache.held() == (460, 4)).await;
    assert_eq!(rig.calls_on(Op::Get, &blobs[3].key), 1);
}

/// Asserts the wait a queued blob is allowed, on the cache's clock: with the only worker held, a
/// blob that has waited longer than `prefetch_queue_ttl` when its turn comes is dropped without a
/// request and leaves the table, and one that has waited exactly that long is filled. A read of the
/// dropped blob is an ordinary miss.
#[tokio::test]
async fn queue_ttl_expires() {
    let rig = Rig::with("tprefetch-ttl", |config| {
        config.prefetch.concurrency = 1;
        config.prefetch.ttl = Duration::from_secs(10);
    });
    let (a, old, young) = (
        stored(&rig, 1, 300).await,
        stored(&rig, 2, 310).await,
        stored(&rig, 3, 320).await,
    );
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.prefetch(&old.blob()), Prefetched::Queued);
    rig.clock.advance(5_000);
    assert_eq!(rig.cache.prefetch(&young.blob()), Prefetched::Queued);
    rig.clock.advance(10_000);

    gate.add_permits(64);
    until("the young one is kept", || rig.cache.held() == (620, 2)).await;
    assert_eq!(rig.calls_on(Op::Get, &old.key), 0);
    assert_eq!((waiting(&rig), in_table(&rig)), (0, 0));
    let counters = rig.cache.counters();
    assert_eq!(counters.prefetch_failures(PrefetchFailed::Expired), 1);
    assert_eq!(read(&counters.prefetch_fills), 2);
    assert_eq!(rig.get(&old.key).await.unwrap(), old.body);
    assert_eq!(rig.calls_on(Op::Get, &old.key), 1);
}

/// Asserts that a read looks for a fill to join and for the blob being queued in one step of the
/// table: the queue drops an expired blob (it takes it out of the queue and the table with the
/// table locked) while the lookup waits for the table, and the lookup then finds no place, not a
/// place whose fill will never run. A blob that is only queued, and one that is dropped, are none to
/// join; a fill that runs is one. It would catch a lookup that clones the place and asks the queue
/// afterwards, which ends a mount open of a dropped blob as an aborted fill.
#[tokio::test]
async fn a_read_never_joins_a_queued_blob_the_queue_drops_as_it_looks() {
    let rig = Rig::with("tprefetch-lookup", |config| {
        config.prefetch.concurrency = 1;
        config.prefetch.ttl = Duration::from_secs(10);
    });
    let (a, b) = (stored(&rig, 1, 300).await, stored(&rig, 2, 310).await);
    let (a_hex, b_hex) = (
        super::layout::name_of(&a.key).hex,
        super::layout::name_of(&b.key).hex,
    );
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Queued);
    let inner = Arc::clone(&rig.cache.inner);
    assert!(inner.joinable(&a_hex).is_some());
    assert!(inner.joinable(&b_hex).is_none());
    assert!(inner.joinable("absent").is_none());
    rig.clock.advance(20_000);

    // The queue is held, so that the drop of b stops with the table locked.
    let queue = locked(&inner.prefetch.queue);
    let dropper = {
        let inner = Arc::clone(&inner);
        std::thread::spawn(move || inner.next_queued().is_none())
    };
    while inner.flights.try_lock().is_ok() {
        std::thread::yield_now();
    }
    let looker = {
        let inner = Arc::clone(&inner);
        let hex = b_hex.clone();
        std::thread::spawn(move || inner.joinable(&hex).is_none())
    };
    drop(queue);
    assert!(
        dropper.join().unwrap_or(false),
        "the queue has nothing to start"
    );
    assert!(
        looker.join().unwrap_or(false),
        "the lookup finds no place of a dropped blob"
    );
    assert_eq!(
        rig.cache
            .counters()
            .prefetch_failures(PrefetchFailed::Expired),
        1
    );
    assert_eq!(in_table(&rig), 1);

    // The read of the dropped blob is an ordinary miss that starts its own fill.
    gate.add_permits(64);
    assert_eq!(rig.cache.fill_blob(&b.blob()).await, Ok(()));
    assert_eq!(rig.calls_on(Op::Get, &b.key), 1);
}

/// Asserts that a queued blob that the cache holds when its turn comes (a put through the cache
/// kept it meanwhile) is dropped without a request, and that nothing is counted as a failure.
#[tokio::test]
async fn a_queued_blob_that_became_present_is_dropped_without_a_fetch() {
    let rig = rig("tprefetch-present", 1);
    let (a, b) = (stored(&rig, 1, 300).await, stored(&rig, 2, 310).await);
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Queued);
    let kept = (310, "tag".to_owned(), 0);
    locked(&rig.cache.inner.index).insert(&super::layout::name_of(&b.key).hex, kept, None);

    gate.add_permits(64);
    until("the table is empty", || in_table(&rig) == 0).await;
    assert_eq!(rig.calls_on(Op::Get, &b.key), 0);
    assert_eq!(waiting(&rig), 0);
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.prefetch_fills), 1);
    assert_eq!(counters.prefetch_failures(PrefetchFailed::NoSpace), 0);
    assert_eq!(counters.prefetch_failures(PrefetchFailed::Expired), 0);
}

/// A cache of a thousand bytes, high water at 900, with one worker.
fn small(name: &str) -> Rig {
    Rig::with(name, |config| {
        config.budget = 1000;
        config.prefetch.concurrency = 1;
    })
}

/// Asserts that a tree larger than the cache is cut at the high water mark in the order it was
/// queued: four blobs of 400 bytes, one worker, and the first two are filled and the other two
/// dropped (`no_space`) without a request, nothing is evicted, and the table is empty.
#[tokio::test]
async fn trees_larger_than_the_cache_are_cut_at_high_water_in_manifest_order() {
    let rig = small("tprefetch-cut");
    let blobs = [
        stored(&rig, 1, 400).await,
        stored(&rig, 2, 400).await,
        stored(&rig, 3, 400).await,
        stored(&rig, 4, 400).await,
    ];
    for blob in &blobs {
        assert_eq!(rig.cache.prefetch(&blob.blob()), Prefetched::Queued);
    }
    until("the table is empty", || in_table(&rig) == 0).await;
    assert_eq!(rig.cache.held(), (800, 2));
    let calls: Vec<u64> = blobs
        .iter()
        .map(|blob| rig.calls_on(Op::Get, &blob.key))
        .collect();
    assert_eq!(calls, [1, 1, 0, 0]);
    let counters = rig.cache.counters();
    assert_eq!(counters.prefetch_failures(PrefetchFailed::NoSpace), 2);
    assert_eq!(counters.evictions(Evicted::Budget), 0);
    assert_eq!(counters.blobs(Prefetched::Queued), 4);
}

/// Asserts that a prefetch never evicts: with the cache at 850 of its 900 bytes of high water, a
/// blob of 100 is dropped `no_space` and what the cache holds stays.
#[tokio::test]
async fn prefetch_never_evicts() {
    let rig = small("tprefetch-never-evicts");
    let (held, more) = (stored(&rig, 1, 850).await, stored(&rig, 2, 100).await);
    assert_eq!(rig.cache.fill_blob(&held.blob()).await, Ok(()));
    assert_eq!(rig.cache.prefetch(&more.blob()), Prefetched::Queued);
    until("the table is empty", || in_table(&rig) == 0).await;
    assert_eq!(rig.cache.held(), (850, 1));
    assert_eq!(rig.calls_on(Op::Get, &more.key), 0);
    let counters = rig.cache.counters();
    assert_eq!(counters.prefetch_failures(PrefetchFailed::NoSpace), 1);
    assert_eq!(counters.evictions(Evicted::Budget), 0);
}

/// Asserts that a fill the critical mark has no room for when it starts (two blobs admitted
/// together, which together pass it) ends without an object, is counted `no_space`, and leaves the
/// other blob.
#[tokio::test]
async fn a_fill_that_finds_no_room_when_it_starts_is_counted_no_space() {
    let rig = Rig::with("tprefetch-critical", |config| {
        config.budget = 1000;
        config.prefetch.concurrency = 2;
    });
    let (a, b) = (stored(&rig, 1, 500).await, stored(&rig, 2, 480).await);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Queued);
    until("the table is empty", || in_table(&rig) == 0).await;
    assert_eq!(rig.cache.held(), (500, 1));
    let counters = rig.cache.counters();
    assert_eq!(counters.prefetch_failures(PrefetchFailed::NoSpace), 1);
    assert_eq!(read(&counters.prefetch_fills), 1);
}

/// Asserts the reason each way a worker's fill can end without an object is counted under.
#[test]
fn each_end_of_a_fill_without_an_object_has_its_reason() {
    let not_found = Stop::Upstream(Error::NotFound("k".to_owned()));
    let down = Stop::Upstream(Error::Unavailable("down".to_owned()));
    let cases = [
        (not_found, PrefetchFailed::NotFound),
        (down, PrefetchFailed::Upstream),
        (Stop::Mismatch, PrefetchFailed::BadDigest),
        (Stop::Changed, PrefetchFailed::Changed),
        (Stop::Invalidated, PrefetchFailed::Changed),
        (Stop::Short, PrefetchFailed::Other),
        (Stop::Local, PrefetchFailed::Other),
        (Stop::Lost, PrefetchFailed::Other),
        (Stop::TooLarge, PrefetchFailed::Other),
        (Stop::Aborted, PrefetchFailed::Other),
    ];
    for (stop, reason) in cases {
        assert_eq!(PrefetchFailed::of(&stop), reason, "{stop:?}");
    }
}

/// Asserts the counters of the queue after a scripted run, by their exact values on `/metricz`: a
/// blob queued behind a worker, one joined, one turned away `busy`, one too large, one promoted by
/// a read, the worker's fill, a blob present, a blob whose bytes are not its digest and one the
/// upstream does not have; and the depth of the queue while one waits.
#[tokio::test]
async fn the_counters_of_the_queue_after_a_scripted_run() {
    let rig = Rig::with("tprefetch-counters", |config| {
        config.budget = 10_000;
        config.prefetch.concurrency = 1;
        config.prefetch.queue = 2;
    });
    let (a, b, c) = (
        stored(&rig, 1, 300).await,
        stored(&rig, 2, 310).await,
        stored(&rig, 3, 320).await,
    );
    let bad = stored(&rig, 4, 330).await;
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Queued);
    assert_eq!(rig.cache.prefetch(&b.blob()), Prefetched::Joined);
    let wrong = digest(b"not the bytes");
    let liar = Blob {
        sha256: &wrong,
        size: 330,
        from: &bad.key,
        offset: 0,
    };
    let missing = Blob {
        sha256: &c.sha,
        size: 320,
        from: "cas/l/absent",
        offset: 0,
    };
    assert_eq!(rig.cache.prefetch(&liar), Prefetched::Queued);
    assert_eq!(rig.cache.prefetch(&missing), Prefetched::Busy);
    let huge = Blob {
        size: 10_001,
        ..c.blob()
    };
    assert_eq!(rig.cache.prefetch(&huge), Prefetched::TooLarge);
    assert!(rig.cache.metrics().contains("prefetch_queue_depth 2\n"));
    assert_eq!(rig.cache.fill_blob(&b.blob()).await, Ok(()));

    gate.add_permits(64);
    until("the worker is done with the queue", || {
        in_table(&rig) == 0 && waiting(&rig) == 0
    })
    .await;
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Present);
    assert_eq!(rig.cache.prefetch(&missing), Prefetched::Queued);
    until("the table is empty again", || in_table(&rig) == 0).await;

    let text = rig.cache.metrics();
    let lines = [
        "tierstore_cache_prefetch_fills_total 1",
        "tierstore_cache_prefetch_promotions_total 1",
        "tierstore_cache_prefetch_queue_depth 0",
        "tierstore_cache_hydrate_blobs_total{outcome=\"present\"} 1",
        "tierstore_cache_hydrate_blobs_total{outcome=\"joined\"} 1",
        "tierstore_cache_hydrate_blobs_total{outcome=\"queued\"} 4",
        "tierstore_cache_hydrate_blobs_total{outcome=\"too_large\"} 1",
        "tierstore_cache_hydrate_blobs_total{outcome=\"busy\"} 1",
        "tierstore_cache_hydrate_blobs_total{outcome=\"off\"} 0",
        "tierstore_cache_hydrate_blobs_total{outcome=\"read_only\"} 0",
        "tierstore_cache_prefetch_fill_failures_total{reason=\"upstream\"} 0",
        "tierstore_cache_prefetch_fill_failures_total{reason=\"not_found\"} 1",
        "tierstore_cache_prefetch_fill_failures_total{reason=\"bad_digest\"} 1",
        "tierstore_cache_prefetch_fill_failures_total{reason=\"no_space\"} 0",
        "tierstore_cache_prefetch_fill_failures_total{reason=\"changed\"} 0",
        "tierstore_cache_prefetch_fill_failures_total{reason=\"expired\"} 0",
        "tierstore_cache_prefetch_fill_failures_total{reason=\"other\"} 0",
        "tierstore_cache_fill_joins_total{path=\"mount\"} 0",
        "tierstore_cache_fill_joins_total{path=\"s3\"} 0",
    ];
    let shown: Vec<&str> = text.lines().collect();
    for line in lines {
        assert!(shown.contains(&line), "{line}\n{text}");
    }
}
