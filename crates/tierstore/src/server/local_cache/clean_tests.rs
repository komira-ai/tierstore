//! The cache after a stop that was not clean: no object a power cut left short or zeroed is
//! served, and a stop in order costs the next start nothing.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use tokio::sync::Notify;

use super::clean::{CLEAN, sealed};
use super::layout::name_of;
use super::metrics::read;
use super::testing::{Quirk, Rig, pattern};
use super::{LocalCache, locked};
use crate::{Backend as _, Op};

/// Fills `count` objects of 1000 bytes in the cache of `rig`, and gives their keys and bodies.
async fn filled(rig: &Rig, count: u8) -> Vec<(String, Vec<u8>)> {
    let mut objects = Vec::new();
    for n in 0..count {
        let key = format!("cas/b/{n}");
        let mut body = pattern(1000);
        body[0] = n;
        rig.put(&key, &body).await;
        rig.get(&key).await.unwrap();
        objects.push((key, body));
    }
    objects
}

/// Writes `bytes` over the data file of `key`, as a power cut leaves a file whose data never
/// reached the disk.
fn overwrite(rig: &Rig, key: &str, bytes: &[u8]) {
    let path = rig.file(key);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Runs the halt of the lock loop of `cache`, which is where a stop in order ends.
async fn stop_in_order(cache: &LocalCache) {
    let halt = Arc::new(Notify::new());
    halt.notify_one();
    cache.hold(Duration::from_secs(3600), halt).await;
}

/// Whether the entry of `key` in `cache` waits for its first check.
fn unverified(cache: &LocalCache, key: &str) -> bool {
    locked(&cache.inner.index).is_unverified(&name_of(key).hex)
}

/// Asserts that a fill and the copy of a put ask the machine to write its disks no time at all:
/// the cache is a copy, so no blob pays for it.
#[tokio::test]
async fn a_fill_and_a_keep_never_ask_the_disk_to_flush() {
    let rig = Rig::writing("tscache-clean-noflush");
    filled(&rig, 3).await;
    let body = Bytes::from(pattern(500));
    let made = rig.cache.put_stream(
        "cas/b/kept",
        Some(500),
        stream::once(async { Ok(body) }).boxed(),
    );
    assert_eq!(made.await, Ok(()));
    assert!(rig.has_file("cas/b/kept"));
    assert_eq!(read(&rig.cache.counters().fills), 3);
    assert_eq!(rig.disk.flushes(), 0);
}

/// Asserts that after a stop with no marker an object whose file is zero-filled with its whole
/// size, as a power cut leaves it, is not served: the read is answered by a new fill from the
/// upstream, the bad file is replaced by the right bytes, the mismatch is counted, and an object
/// that is whole is served from the disk after its first check with no call of the upstream.
#[tokio::test]
async fn a_zeroed_file_after_an_unclean_stop_is_refetched_and_never_served() {
    let rig = Rig::new("tscache-clean-zeroed");
    let objects = filled(&rig, 2).await;
    overwrite(&rig, &objects[0].0, &[0; 1000]);
    let again = rig.reopen();
    assert!(unverified(&again, &objects[0].0));

    let gets = rig.calls(Op::Get);
    assert_eq!(again.get(&objects[0].0, None).await.unwrap(), objects[0].1);
    assert_eq!(rig.calls(Op::Get), gets + 1);
    assert_eq!(
        std::fs::read(rig.file(&objects[0].0)).unwrap(),
        objects[0].1
    );
    assert_eq!(read(&again.counters().scrub_mismatches), 1);
    assert!(!unverified(&again, &objects[0].0));

    assert!(unverified(&again, &objects[1].0));
    let gets = rig.calls(Op::Get);
    assert_eq!(again.get(&objects[1].0, None).await.unwrap(), objects[1].1);
    assert_eq!(again.get(&objects[1].0, None).await.unwrap(), objects[1].1);
    assert_eq!(rig.calls(Op::Get), gets);
    assert!(!unverified(&again, &objects[1].0));
    assert_eq!(read(&again.counters().scrub_mismatches), 1);
}

/// Asserts that a file a power cut left short (or empty) is not in the index at all after an
/// unclean stop, and the read of it is a fill.
#[tokio::test]
async fn a_short_file_after_an_unclean_stop_is_refetched() {
    let rig = Rig::new("tscache-clean-short");
    let objects = filled(&rig, 2).await;
    overwrite(&rig, &objects[0].0, &objects[0].1[..400]);
    overwrite(&rig, &objects[1].0, b"");
    let again = rig.reopen();
    assert_eq!(again.held(), (0, 0));
    let gets = rig.calls(Op::Get);
    for (key, body) in &objects {
        assert_eq!(again.get(key, None).await.unwrap(), *body);
    }
    assert_eq!(rig.calls(Op::Get), gets + 2);
}

/// Asserts that a stop in order writes the marker after one flush of the disks, and that the
/// next holder removes it at start and trusts what it finds: nothing waits for a check.
#[tokio::test]
async fn a_stop_in_order_leaves_a_marker_that_saves_the_next_start_its_checks() {
    let rig = Rig::new("tscache-clean-marker");
    let objects = filled(&rig, 2).await;
    let first = rig.locked();
    stop_in_order(&first).await;
    assert!(sealed(&rig.config.path));
    assert_eq!(rig.disk.flushes(), 1);
    drop(first);

    let next = rig.locked();
    assert!(!rig.config.path.join(CLEAN).exists());
    for (key, _) in &objects {
        assert!(!unverified(&next, key));
    }
    // A stop that does not end in order leaves none: the start after it checks.
    drop(next);
    let after_crash = rig.locked();
    assert!(unverified(&after_crash, &objects[0].0));
}

/// Asserts that a cache that does not hold the lock (the holder is alive and has removed the
/// marker) checks the hits it serves, drops nothing of the holder's files when one fails, and
/// answers that read from the upstream.
#[tokio::test]
async fn a_read_only_cache_checks_a_hit_and_leaves_a_bad_file_to_the_holder() {
    let rig = Rig::new("tscache-clean-readonly");
    let objects = filled(&rig, 2).await;
    let holder = rig.locked();
    let reader = rig.locked();
    overwrite(&rig, &objects[0].0, &[0; 1000]);
    assert!(unverified(&reader, &objects[0].0));

    let gets = rig.calls(Op::Get);
    assert_eq!(reader.get(&objects[0].0, None).await.unwrap(), objects[0].1);
    assert_eq!(rig.calls(Op::Get), gets + 1);
    assert_eq!(
        std::fs::read(rig.file(&objects[0].0)).unwrap(),
        vec![0; 1000]
    );

    assert_eq!(reader.get(&objects[1].0, None).await.unwrap(), objects[1].1);
    assert_eq!(rig.calls(Op::Get), gets + 1);
    assert!(!unverified(&reader, &objects[1].0));
    drop(holder);
}

/// Asserts that a cache that has a stage file in use, or is not the holder, or whose disks
/// would not flush, writes no marker; and that a marker that cannot be removed stops the start.
#[tokio::test]
async fn no_marker_is_written_while_the_directory_may_still_change() {
    let rig = Rig::new("tscache-clean-nomarker");
    let cache = rig.locked();
    let counter = &cache.inner.counters.staging_files;
    let _ = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    cache.inner.seal();
    assert!(!sealed(&rig.config.path));
    assert_eq!(rig.disk.flushes(), 0);
    let _ = counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

    rig.disk.set(Quirk::FlushFails);
    cache.inner.seal();
    assert!(!sealed(&rig.config.path));
    assert_eq!(rig.disk.flushes(), 1);
    rig.disk.set(Quirk::None);

    let reader = {
        let reader = rig.locked();
        reader.inner.seal();
        reader
    };
    assert!(!sealed(&rig.config.path));
    assert_eq!(rig.disk.flushes(), 1);
    drop(reader);

    std::fs::create_dir(rig.config.path.join(CLEAN)).unwrap();
    cache.inner.seal();
    assert!(!sealed(&rig.config.path));
    drop(cache);
    assert!(rig.try_locked().is_err());
}
