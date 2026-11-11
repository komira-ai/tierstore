//! The background tasks of a cache that does not hold the lock of its directory, one at a time:
//! each takes its read-only branch and changes nothing in the directory (the whole tree, with
//! names, sizes, modes, inodes and times, is the same before and after), and each does its work
//! once on the same input after the cache takes the lock.
//!
//! In these tests `Rig::cache` stands for the process that held the directory and wrote to it,
//! and the caches of `Rig::locked` are the holder (the first) and the successor (the second).
//! The tree is stamped with a time long past first (`testing::age`), so that a write shows as
//! another time on a file system whose clock is coarser than the test.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;

use super::LocalCache;
use super::hold::Trace;
use super::layout::{Origin, Sidecar, data_path, meta_path, name_of, stage_dir};
use super::metrics::{Evicted, read};
use super::scrub::{ScrubPass, Scrubber};
use super::testing::{Rig, age, pattern, tree, wait_advancing};
use super::{Mode, Tick};
use crate::{Backend as _, ByteRange};

/// The interval of the loops that the tests drive on a paused clock.
const INTERVAL: Duration = Duration::from_secs(10);

/// Fills `count` objects of 30 bytes, `cas/b/k0` and so on, in the directory and the upstream.
async fn filled(rig: &Rig, count: usize) {
    for n in 0..count {
        let key = format!("cas/b/k{n}");
        rig.put(&key, &pattern(30)).await;
        let _ = rig.cache.get(&key, None).await.unwrap();
    }
}

/// Changes one byte of the data file of `key` in place, as rot does.
fn rot_data(rig: &Rig, key: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let path = rig.file(key);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&path, bytes).unwrap();
}

/// Rewrites the sidecar of `key` so that it names the hash of other bytes.
fn rot_sidecar(rig: &Rig, key: &str) {
    let path = meta_path(&rig.config.path, &name_of(key).hex);
    let mut sidecar: Sidecar = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    sidecar.sha256 = "0".repeat(64);
    std::fs::write(path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
}

/// Drives the lock loop of `successor` on a paused clock, with the holder gone, until the
/// successor has taken the lock, counting the ticks of the loop through the trace of the loop;
/// then stops the loop.
async fn take_the_lock_by_the_loop(successor: &LocalCache) {
    assert_eq!(successor.mode(), Mode::ReadOnly);
    let halt = Arc::new(Notify::new());
    let (cache, stop) = (successor.clone(), Arc::clone(&halt));
    let ticks = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&ticks);
    successor.inner.set_trace(Trace {
        after_tick: Some(Arc::new(move || {
            let _ = counting.fetch_add(1, Ordering::SeqCst);
        })),
        ..Trace::default()
    });
    let looping = tokio::spawn(async move { cache.hold(INTERVAL, stop).await });
    wait_advancing("the loop took the lock", INTERVAL, || {
        successor.mode() == Mode::ReadWrite
    })
    .await;
    // One more tick after the flip finds the lock held and changes nothing.
    let seen = ticks.load(Ordering::SeqCst);
    wait_advancing("a tick after the flip", INTERVAL, || {
        ticks.load(Ordering::SeqCst) > seen
    })
    .await;
    halt.notify_one();
    looping.await.unwrap();
}

/// Asserts (T28) the scrub of a read-only cache over a corrupt data file and a corrupt sidecar:
/// a tick by hand and the loop with a ticker read and drop nothing and count nothing, and the
/// tree is the same; after the cache takes the lock the walk drops each of the two objects once
/// (files, entry, one mismatch each) and a second tick finds nothing more to drop. Catches a
/// scrub that deletes what the holder may still be serving.
#[tokio::test(start_paused = true)]
async fn the_scrub_leaves_a_corrupt_object_to_the_holder_and_drops_it_once_after_the_take() {
    let rig = Rig::new("tscache-ro-task-scrub");
    filled(&rig, 3).await;
    rot_data(&rig, "cas/b/k0");
    rot_sidecar(&rig, "cas/b/k1");
    let holder = rig.locked();
    let reader = rig.locked();
    let root = &rig.config.path;
    assert_eq!(reader.held(), (90, 3));
    age(root);
    let before = tree(root);

    let mut state = Scrubber::new(1 << 20);
    assert_eq!(reader.scrub_once(&mut state), ScrubPass::default());
    assert_eq!(reader.scrub_once(&mut state), ScrubPass::default());
    let halt = Arc::new(Notify::new());
    let (cache, stop) = (reader.clone(), Arc::clone(&halt));
    let ticker = super::scrub::Every::new(INTERVAL);
    let walking = tokio::spawn(async move { cache.scrub(Some(1 << 20), 0, ticker, stop).await });
    for _ in 0..5 {
        tokio::time::advance(INTERVAL).await;
        tokio::task::yield_now().await;
    }
    halt.notify_one();
    walking.await.unwrap();
    assert_eq!(tree(root), before);
    assert_eq!(read(&reader.counters().scrubbed_objects), 0);
    assert_eq!(read(&reader.counters().scrub_mismatches), 0);
    assert_eq!(reader.held(), (90, 3));
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);

    drop(holder);
    take_the_lock_by_the_loop(&reader).await;
    assert_eq!(reader.held(), (90, 3));
    let mut state = Scrubber::new(1 << 20);
    let pass = reader.scrub_once(&mut state);
    assert_eq!((pass.scrubbed, pass.dropped), (3, 2));
    assert_eq!(read(&reader.counters().scrub_mismatches), 2);
    assert!(!rig.has_file("cas/b/k0") && !rig.has_file("cas/b/k1"));
    assert!(rig.has_file("cas/b/k2"));
    assert!(!meta_path(root, &name_of("cas/b/k1").hex).exists());
    assert_eq!(reader.held(), (30, 1));
    assert_eq!(reader.scrub_once(&mut state).dropped, 0);
    assert_eq!(read(&reader.counters().scrub_mismatches), 2);
}

/// Asserts (T28) the eviction of a read-only cache whose budget the hits exceed: the pass by
/// hand and the loop on a paused clock evict nothing, free nothing, stamp no file and leave the
/// tree as it was, and the objects it was asked to read are still hits; after the cache takes the
/// lock the loop evicts down to the budget, each object once, and a pass after that evicts
/// nothing. Catches an evictor that frees the space the holder has accounted for.
#[tokio::test(start_paused = true)]
async fn the_eviction_leaves_an_over_budget_cache_to_the_holder_and_evicts_once_after_the_take() {
    let rig = Rig::new("tscache-ro-task-evict");
    filled(&rig, 3).await;
    let holder = rig.locked();
    let reader = rig.locked_with(|config| config.budget = 10);
    let root = &rig.config.path;
    for n in 0..3 {
        let key = format!("cas/b/k{n}");
        assert_eq!(reader.get(&key, None).await.unwrap(), pattern(30));
    }
    assert_eq!(read(&reader.counters().hits), 3);
    age(root);
    let before = tree(root);

    assert_eq!(reader.evict().evicted, 0);
    let halt = Arc::new(Notify::new());
    let (cache, stop) = (reader.clone(), Arc::clone(&halt));
    let looping = tokio::spawn(async move { cache.maintain(INTERVAL, stop).await });
    for _ in 0..5 {
        tokio::time::advance(INTERVAL).await;
        tokio::task::yield_now().await;
    }
    halt.notify_one();
    looping.await.unwrap();
    assert!(rig.disk.touched.lock().unwrap().is_empty());
    assert_eq!(tree(root), before);
    assert_eq!(reader.held(), (90, 3));
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);

    drop(holder);
    take_the_lock_by_the_loop(&reader).await;
    let halt = Arc::new(Notify::new());
    let (cache, stop) = (reader.clone(), Arc::clone(&halt));
    let looping = tokio::spawn(async move { cache.maintain(INTERVAL, stop).await });
    wait_advancing("the loop evicted", INTERVAL, || reader.held() == (0, 0)).await;
    halt.notify_one();
    looping.await.unwrap();
    assert_eq!(reader.counters().evictions(Evicted::Budget), 3);
    assert!((0..3).all(|n| !rig.has_file(&format!("cas/b/k{n}"))));
    assert_eq!(reader.evict().evicted, 0);
    assert_eq!(reader.counters().evictions(Evicted::Budget), 3);
}

/// Asserts (T28) the record of use of a read-only cache: the hits it serves are not stamped on
/// the files, by the end of a loop or by a flush, and the tree is the same; after the cache takes
/// the lock the hits it serves are stamped once, one stamp for each object however often it was
/// read, and a flush after that stamps nothing. Catches a successor that writes the times of the
/// holder's files.
#[tokio::test(start_paused = true)]
async fn the_record_of_use_is_not_stamped_by_a_read_only_cache_and_is_stamped_once_after_the_take()
{
    let rig = Rig::new("tscache-ro-task-stamp");
    filled(&rig, 2).await;
    let holder = rig.locked();
    let reader = rig.locked();
    let root = &rig.config.path;
    let read_both = || async {
        for _ in 0..2 {
            for n in 0..2 {
                let key = format!("cas/b/k{n}");
                assert_eq!(reader.get(&key, None).await.unwrap(), pattern(30));
            }
        }
    };
    read_both().await;
    age(root);
    let before = tree(root);
    for _ in 0..2 {
        let halt = Arc::new(Notify::new());
        halt.notify_one();
        reader.maintain(INTERVAL, halt).await;
    }
    assert!(rig.disk.touched.lock().unwrap().is_empty());
    assert_eq!(tree(root), before);
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);

    drop(holder);
    take_the_lock_by_the_loop(&reader).await;
    read_both().await;
    for _ in 0..2 {
        let halt = Arc::new(Notify::new());
        halt.notify_one();
        reader.maintain(INTERVAL, halt).await;
    }
    let stamped = rig.disk.touched.lock().unwrap().clone();
    let mut paths: Vec<_> = stamped.into_iter().map(|(path, _)| path).collect();
    paths.sort();
    let mut wanted: Vec<_> = (0..2)
        .map(|n| data_path(root, &name_of(&format!("cas/b/k{n}")).hex))
        .collect();
    wanted.sort();
    assert_eq!(paths, wanted);
}

/// What the holder leaves while it works: a sidecar whose data file is not linked yet, a data
/// file with no sidecar, and a fill in `.stage/`.
fn leave_the_middle_of_a_publish_and_a_fill(root: &Path) {
    let write = |path: &Path, bytes: &[u8]| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    let hex = |first: &str| format!("{first}{}", "0".repeat(64 - first.len()));
    let sidecar = Sidecar {
        key: "cas/b/x".to_owned(),
        size: 4,
        sha256: "0".repeat(64),
        tag: "tag".to_owned(),
        modified: 0,
        origin: Origin::Object,
    };
    write(
        &meta_path(root, &hex("aabb")),
        &serde_json::to_vec(&sidecar).unwrap(),
    );
    write(&data_path(root, &hex("ccdd")), b"no sidecar");
    write(&stage_dir(root).join("0000000000000009"), b"half a fill");
}

/// Asserts (T28) the take of the lock, with the deletes it has noted: a read-only cache that
/// is told of a delete and of a replacing put, and ticks while the holder holds the lock,
/// removes no file and empties no stage (the tree is the same, and the tick is `Waiting`); after
/// the cache takes the lock the scan removes the stage file, the sidecar without data and the data
/// without sidecar once, and the files of the deleted and the replaced objects once, and a tick
/// after that is `Held` and changes nothing. Catches a delete-scan that runs under the holder.
#[tokio::test(start_paused = true)]
async fn the_delete_scan_waits_for_the_lock_and_removes_what_was_deleted_once() {
    let rig = Rig::writing("tscache-ro-task-promote");
    filled(&rig, 3).await;
    let holder = rig.locked();
    let reader = rig.locked();
    let root = &rig.config.path;
    leave_the_middle_of_a_publish_and_a_fill(root);
    reader.delete("cas/b/k0").await.unwrap();
    reader.delete("cas/b/k1").await.unwrap();
    let other = Bytes::from_static(b"other");
    reader.put("cas/b/k1", other).await.unwrap();
    age(root);
    let before = tree(root);
    for _ in 0..3 {
        assert_eq!(reader.tick().unwrap(), Tick::Waiting);
    }
    assert_eq!(tree(root), before);
    assert!(rig.has_file("cas/b/k0") && rig.has_file("cas/b/k1"));
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);

    drop(holder);
    take_the_lock_by_the_loop(&reader).await;
    assert!(!rig.has_file("cas/b/k0") && !rig.has_file("cas/b/k1"));
    assert!(rig.has_file("cas/b/k2"));
    assert_eq!(rig.staged_files(), 0);
    assert_eq!(reader.counters().discarded_at_start(), 3);
    assert_eq!(reader.held(), (30, 1));
    let after = tree(root);
    assert_eq!(reader.tick().unwrap(), Tick::Held);
    assert_eq!(tree(root), after);
    assert_eq!(reader.counters().discarded_at_start(), 3);
    let range = Some(ByteRange::new(0, 30));
    assert_eq!(reader.get("cas/b/k2", range).await.unwrap(), pattern(30));
}
