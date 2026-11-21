//! What a cache with entries by extents answers: spans, pins, ordinary reads, eviction, the
//! start-up read and the layout beside `digest-v1`.

use std::os::unix::fs::MetadataExt as _;
use std::time::Duration;

use super::super::layout::{self, name_of};
use super::super::metrics::read;
use super::super::pack_testing::Pack;
use super::super::testing::{Rig, digest, pattern};
use super::super::{Disk, SystemDisk};
use super::files::{data_of, meta_of};
use super::fill_tests::{data, fill, pack_in};
use super::{RangeWant, Span};
use crate::{Backend, ByteRange, Op};

/// The names of every file under `dir`, relative to it, sorted.
fn files_under(dir: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path.strip_prefix(dir).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    found
}

/// Asserts the cache half of a span that is not copied: a pack that is whole in the cache gives
/// the span in the pack's own file in `obj/`, and an entry by extents gives it in its own sparse file,
/// each at the offset of the range in the pack; no file is made for the span (the files of the
/// cache directory are the same before and after, the pack's file has one name and the same
/// inode), a range the cache does not hold, an empty range, a key the cache does not keep and a
/// range past a whole pack are none.
#[tokio::test]
async fn span_of_cached_pack_is_not_copied() {
    let rig = Rig::new("tsext-span");
    let pack = pack_in(&rig).await;
    rig.get(&pack.key).await.unwrap();
    let before = files_under(&rig.config.path);
    let inode = std::fs::metadata(rig.file(&pack.key)).unwrap().ino();
    let span = rig.cache.span_of(&pack.key, 1000, 2000).unwrap();
    assert_eq!(
        span,
        Span {
            path: rig.file(&pack.key),
            offset: 1000,
            whole: true
        }
    );
    assert_eq!(files_under(&rig.config.path), before);
    let held = std::fs::metadata(&span.path).unwrap();
    assert_eq!((held.ino(), held.nlink()), (inode, 1));
    assert_eq!(rig.cache.span_of(&pack.key, 5000, 2000), None);

    let partial = Rig::new("tsext-span-partial");
    let pack = pack_in(&partial).await;
    fill(&partial, &pack, 1000, 3000).await.unwrap();
    let before = files_under(&partial.config.path);
    let span = partial.cache.span_of(&pack.key, 1500, 500).unwrap();
    assert_eq!(
        span,
        Span {
            path: data(&partial, &pack),
            offset: 1500,
            whole: false
        }
    );
    assert_eq!(files_under(&partial.config.path), before);
    assert_eq!(std::fs::metadata(&span.path).unwrap().nlink(), 1);
    assert!(partial.cache.span_of(&pack.key, 900, 200).is_none());
    assert!(partial.cache.span_of(&pack.key, 1500, 0).is_none());
    assert!(partial.cache.span_of(&pack.key, u64::MAX, 2).is_none());
    assert!(partial.cache.span_of("other/pack", 0, 10).is_none());
    assert!(partial.cache.span_of("cas/pack/none", 0, 10).is_none());
    assert_eq!(partial.calls(Op::Get), 1);
}

/// Asserts that the pins of packs count a pack's bytes once however many trees pin it: a pack the
/// cache holds nothing of is absent; two trees pin an entry for the bytes it has filled; the pin
/// grows once as the entry fills; one tree letting go leaves the pins of the other; the pin stays
/// on the pack when the entry fills whole and moves into `obj/`.
#[tokio::test]
async fn a_pack_is_pinned_once_for_every_tree_and_the_pin_follows_its_bytes() {
    let rig = Rig::new("tsext-pins");
    let pack = pack_in(&rig).await;
    let key = [name_of(&pack.key).hex];
    let answer = rig.cache.pin("m", "t1", &key).unwrap();
    assert_eq!((answer.pinned, answer.absent), (0, 1));
    fill(&rig, &pack, 0, 1000).await.unwrap();
    assert_eq!(rig.cache.pin("m", "t1", &key).unwrap().pinned, 1);
    assert_eq!(rig.cache.pin("m", "t2", &key).unwrap().pinned, 1);
    assert_eq!(rig.cache.pinned_bytes(), 1000);
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    assert_eq!(rig.cache.pinned_bytes(), 3000);
    assert_eq!(rig.cache.unpin("m", "t1").unwrap(), 1);
    assert_eq!(rig.cache.pinned_bytes(), 3000);
    fill(&rig, &pack, 3000, 6000).await.unwrap();
    assert!(rig.has_file(&pack.key));
    assert_eq!(rig.cache.pinned_bytes(), 6000);
    assert_eq!(rig.cache.unpin("m", "t2").unwrap(), 1);
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that a pin that would take the pinned bytes over `pin_limit` is refused for an entry as
/// for an object, and counts the entry as used.
#[tokio::test]
async fn an_entry_over_the_pin_limit_is_not_pinned() {
    let rig = Rig::with("tsext-pin-limit", |config| config.pin_limit = 500);
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 1000).await.unwrap();
    let answer = rig.cache.pin("m", "t", &[name_of(&pack.key).hex]).unwrap();
    assert_eq!((answer.pinned, answer.refused), (0, 1));
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that an ordinary ranged read of the pack's key is served from the entry when the range
/// is held, with no call to the upstream and counted as a hit; and that a read of a range it does not
/// hold, or of the whole pack, is a miss that gives the true bytes (never a hole): the pack is filled
/// whole, the entry is removed with its files, and the pin it had stays on the pack.
#[tokio::test]
async fn an_ordinary_read_never_serves_a_hole() {
    let rig = Rig::new("tsext-read");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    rig.cache.pin("m", "t", &[name_of(&pack.key).hex]).unwrap();
    let gets = rig.calls(Op::Get);
    let held = rig
        .cache
        .get(&pack.key, Some(ByteRange::new(1200, 300)))
        .await
        .unwrap();
    assert_eq!(held, pack.bytes()[1200..1500]);
    assert_eq!(rig.calls(Op::Get), gets);
    assert_eq!(read(&rig.cache.counters().hits), 1);
    assert_eq!(read(&rig.cache.counters().misses), 0);

    let hole = rig
        .cache
        .get(&pack.key, Some(ByteRange::new(0, 100)))
        .await
        .unwrap();
    assert_eq!(hole, pack.bytes()[..100]);
    assert!(rig.has_file(&pack.key));
    assert_eq!(std::fs::read(rig.file(&pack.key)).unwrap(), pack.bytes());
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
    assert!(!meta_of(&rig.config.path, &name_of(&pack.key).hex).exists());
    assert_eq!(rig.cache.pinned_bytes(), 6000);

    let whole = Rig::new("tsext-read-whole");
    let pack = pack_in(&whole).await;
    fill(&whole, &pack, 1000, 3000).await.unwrap();
    assert_eq!(whole.get(&pack.key).await.unwrap(), pack.bytes());
    assert_eq!(whole.cache.extents_held(), (0, 0));
}

/// Asserts that a range read the entry does not hold, of a pack larger than `whole_limit`, passes
/// through to the upstream and keeps nothing and leaves the entry as it was; that a read past the end
/// is the error of the upstream; and that with `revalidate_after` set an entry is not read at all,
/// since it has no age to check.
#[tokio::test]
async fn reads_the_entry_does_not_serve_go_to_the_upstream() {
    let rig = Rig::with("tsext-read-pass", |config| config.whole_limit = 100);
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    let passed = rig
        .cache
        .get(&pack.key, Some(ByteRange::new(0, 100)))
        .await
        .unwrap();
    assert_eq!(passed, pack.bytes()[..100]);
    assert!(!rig.has_file(&pack.key));
    assert_eq!(rig.cache.extents_held(), (1, 2000));
    assert!(
        rig.cache
            .get(&pack.key, Some(ByteRange::new(7000, 10)))
            .await
            .is_err()
    );

    let aged = Rig::with("tsext-read-aged", |config| {
        config.revalidate_after = Some(Duration::from_secs(3600));
    });
    let pack = pack_in(&aged).await;
    fill(&aged, &pack, 1000, 3000).await.unwrap();
    let gets = aged.calls(Op::Get);
    let got = aged
        .cache
        .get(&pack.key, Some(ByteRange::new(1200, 300)))
        .await
        .unwrap();
    assert_eq!(got, pack.bytes()[1200..1500]);
    assert!(aged.calls(Op::Get) > gets);
    assert_eq!(read(&aged.cache.counters().hits), 0);
}

/// Asserts that an entry whose file was removed from under the cache is dropped by the read that
/// finds it, which is then a miss that gives the true bytes.
#[tokio::test]
async fn a_read_that_finds_the_file_gone_drops_the_entry() {
    let rig = Rig::new("tsext-read-gone");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    std::fs::remove_file(data(&rig, &pack)).unwrap();
    let got = rig
        .cache
        .get(&pack.key, Some(ByteRange::new(1200, 300)))
        .await
        .unwrap();
    assert_eq!(got, pack.bytes()[1200..1500]);
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert!(rig.has_file(&pack.key));
}

/// Asserts that a file of the entry that is no regular file (a directory in its place) is not
/// served and the entry is not dropped by the read that finds it (a reason that does not say the
/// file is gone): the read is a miss that gives the true bytes, and the entry stays until the
/// whole pack that the miss fills replaces it.
#[tokio::test]
async fn a_file_that_cannot_be_opened_is_a_miss_that_does_not_drop_the_entry() {
    let rig = Rig::new("tsext-read-bad");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    std::fs::remove_file(data(&rig, &pack)).unwrap();
    std::fs::create_dir(data(&rig, &pack)).unwrap();
    let got = rig
        .cache
        .get(&pack.key, Some(ByteRange::new(1200, 300)))
        .await
        .unwrap();
    assert_eq!(got, pack.bytes()[1200..1500]);
    assert!(rig.has_file(&pack.key));
    assert_eq!(rig.cache.extents_held(), (0, 0));
}

/// Asserts that a delete through the cache removes the entry with its files and its pins: what it
/// kept is of a version that is gone.
#[tokio::test]
async fn a_delete_through_the_cache_removes_the_entry() {
    let rig = Rig::writing("tsext-delete");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    rig.cache.pin("m", "t", &[name_of(&pack.key).hex]).unwrap();
    rig.cache.delete(&pack.key).await.unwrap();
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that the entries survive a restart: the next cache reads the sidecar and finds the same
/// ranges and bytes without the upstream (the span is served, a range read is a hit), and that an
/// entry whose file is not the size its sidecar says (a file cut short) is removed at start and
/// counted.
#[tokio::test]
async fn extents_survive_restart() {
    let rig = Rig::new("tsext-restart");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    fill(&rig, &pack, 4000, 4500).await.unwrap();
    let gets = rig.calls(Op::Get);
    let again = rig.reopen();
    assert_eq!(again.extents_held(), (1, 2500));
    assert_eq!(again.held(), (2500, 0));
    assert!(again.span_of(&pack.key, 4100, 100).is_some());
    assert!(again.span_of(&pack.key, 3000, 100).is_none());
    let got = again
        .get(&pack.key, Some(ByteRange::new(1200, 300)))
        .await
        .unwrap();
    assert_eq!(got, pack.bytes()[1200..1500]);
    assert_eq!(rig.calls(Op::Get), gets);
    assert_eq!(again.counters().discarded_at_start(), 0);
    drop(again);

    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(data(&rig, &pack))
        .unwrap();
    file.set_len(5999).unwrap();
    let cut = rig.reopen();
    assert_eq!(cut.extents_held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
    assert!(cut.counters().discarded_at_start() >= 1);
}

/// Asserts that the entries of a restart are in the order of use with the objects: the entry
/// written between two objects is evicted between them, not before all of them or after.
#[tokio::test]
async fn a_restart_keeps_the_entries_in_the_order_of_use() {
    let rig = Rig::new("tsext-order");
    rig.put("cas/b/k0", &pattern(100)).await;
    rig.put("cas/b/k1", &pattern(100)).await;
    let pack = pack_in(&rig).await;
    rig.get("cas/b/k0").await.unwrap();
    fill(&rig, &pack, 0, 100).await.unwrap();
    rig.get("cas/b/k1").await.unwrap();
    let stamp = |key: &str, seconds: u64| {
        let path = if key == "pack" {
            data(&rig, &pack)
        } else {
            rig.file(key)
        };
        SystemDisk.touch(&path, seconds * 1000).unwrap();
    };
    stamp("cas/b/k0", 1000);
    stamp("pack", 2000);
    stamp("cas/b/k1", 3000);
    let again = rig.reopen();
    let order: Vec<(String, u64, bool)> = super::super::locked(&again.inner.index).oldest_all();
    let names = [
        name_of("cas/b/k0").hex,
        name_of(&pack.key).hex,
        name_of("cas/b/k1").hex,
    ];
    let got: Vec<&str> = order.iter().map(|(hex, _, _)| hex.as_str()).collect();
    assert_eq!(got, names.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(
        order.iter().map(|o| o.2).collect::<Vec<_>>(),
        [false, true, false]
    );
}

/// Asserts that the eviction pass counts an entry's filled bytes in the cache and removes it in
/// its place in the order of use among the objects: objects and the entry are removed oldest first
/// until the cache is under the low mark, the entry's files go with it, and it is counted as an
/// eviction of entries.
#[tokio::test]
async fn the_eviction_pass_removes_entries_with_the_objects_by_age() {
    let rig = Rig::with("tsext-evict", |config| {
        config.budget = 5000;
        config.high_water = 50;
        config.low_water = 20;
        config.critical = 100;
    });
    rig.disk.set_space(1_000_000, 1_000_000);
    for n in 0..2 {
        rig.put(&format!("cas/b/k{n}"), &pattern(1000)).await;
    }
    let pack = pack_in(&rig).await;
    rig.get("cas/b/k0").await.unwrap();
    fill(&rig, &pack, 1000, 2000).await.unwrap();
    rig.get("cas/b/k1").await.unwrap();
    // The fill of the last object took the cache over the high mark: the pass that fill runs
    // has removed the two oldest, the object read first and the entry after it.
    assert_eq!(rig.cache.held(), (1000, 1));
    assert!(!rig.has_file("cas/b/k0"));
    assert!(rig.has_file("cas/b/k1"));
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
    assert!(!meta_of(&rig.config.path, &name_of(&pack.key).hex).exists());
    assert_eq!(read(&rig.cache.inner.extents.counters.evictions), 1);
}

/// Asserts that an entry a tree pins is skipped by the eviction pass and counted in `pin_skips`,
/// and an entry that a fill is writing to is skipped too; both are evicted once let go.
#[tokio::test]
async fn a_pinned_entry_or_one_being_filled_is_not_evicted() {
    let rig = Rig::with("tsext-evict-pinned", |config| {
        config.budget = 5000;
        config.high_water = 10;
        config.low_water = 0;
        config.critical = 100;
    });
    rig.disk.set_space(1_000_000, 1_000_000);
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 1000).await.unwrap();
    rig.cache.pin("m", "t", &[name_of(&pack.key).hex]).unwrap();
    let pass = rig.cache.evict();
    assert_eq!((pass.evicted, pass.pin_skips), (0, 1));
    assert_eq!(rig.cache.extents_held(), (1, 1000));

    rig.cache.unpin("m", "t").unwrap();
    let hex = name_of(&pack.key).hex;
    let object = super::super::locked(&rig.cache.inner.index)
        .partials
        .get(&hex)
        .map(|entry| entry.object.clone())
        .unwrap();
    let serial = super::super::locked(&rig.cache.inner.index).extent_hold(&hex, &object);
    assert!(serial.is_some());
    assert_eq!(rig.cache.evict().evicted, 0);
    assert!(!rig.cache.inner.evict_partial(&hex));
    let empty = super::super::locked(&rig.cache.inner.index).extent_release(&hex, serial.unwrap());
    assert!(!empty);
    assert!(!super::super::locked(&rig.cache.inner.index).extent_release(&hex, 9999));
    assert!(!super::super::locked(&rig.cache.inner.index).extent_release("0000", 1));
    assert_eq!(rig.cache.evict().evicted, 1);
}

/// Asserts that the fill of a range counts against the critical mark with the objects: a range
/// that would take the cache past it is `no space`, and the bytes of entries are in the count.
#[tokio::test]
async fn entries_count_against_the_critical_mark() {
    let rig = Rig::with("tsext-critical", |config| {
        config.budget = 4000;
        config.critical = 50;
    });
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 1500).await.unwrap();
    assert_eq!(
        fill(&rig, &pack, 3000, 3600).await,
        Err(super::RangeError::NoSpace)
    );
    assert_eq!(rig.cache.extents_held(), (1, 1500));
}

/// Asserts that a cache that takes the lock after the one that held it reads the entries the holder
/// made, and one that does not hold it reads none; and that the entry of a name that was deleted
/// or replaced while this cache was read-only (the names it noted) is removed with its files rather
/// than read.
#[tokio::test]
async fn a_cache_that_takes_the_lock_reads_the_entries_and_leaves_out_the_forgotten() {
    let rig = Rig::new("tsext-takeover");
    let pack = pack_in(&rig).await;
    let other = Pack::put(&rig, "cas/pack/two", &[500, 500]).await;
    let holder = rig.locked();
    for (key, sha, from, to) in [
        (&pack.key, digest(&pack.bytes()[..100]), 0, 100),
        (&other.key, digest(&other.bytes()[..100]), 0, 100),
    ] {
        let want = RangeWant {
            pack: key,
            known: None,
            offset: from,
            len: to,
            sha256: &sha,
        };
        holder.fill_range(&want).await.unwrap();
    }
    let waiting = rig.locked();
    assert_eq!(waiting.extents_held(), (0, 0));
    waiting.inner.forget(&other.key);
    drop(holder);
    assert_eq!(waiting.tick().unwrap(), super::super::Tick::Took);
    assert_eq!(waiting.extents_held(), (1, 100));
    assert!(waiting.span_of(&pack.key, 0, 100).is_some());
    assert!(!data(&rig, &other).exists());
}

/// Asserts that the directory `extents-v1` is not made until a range is filled, that a release that
/// reads `obj/` alone (the scan `digest-v1` has always had) sees only whole objects of the cache
/// after entries by extents were made, and removes nothing of the entries; and that the entry
/// is where `digest-v1` has none.
#[tokio::test]
async fn digest_v1_holds_only_whole_objects_beside_extents_v1() {
    let rig = Rig::new("tsext-layout");
    let pack = pack_in(&rig).await;
    rig.put("cas/b/k0", &pattern(100)).await;
    rig.get("cas/b/k0").await.unwrap();
    assert!(!rig.config.path.join("extents-v1").exists());
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    assert!(rig.config.path.join("extents-v1").is_dir());
    let old = layout::scan(&rig.config.path).unwrap();
    assert_eq!(old.discarded, 0);
    let names: Vec<&str> = old.found.iter().map(|f| f.hex.as_str()).collect();
    assert_eq!(names, [name_of("cas/b/k0").hex.as_str()]);
    assert!(data(&rig, &pack).is_file());
    let read_only = layout::read_only_scan(&rig.config.path);
    assert_eq!(read_only.found.len(), 1);
    assert!(!rig.has_file(&pack.key));
    assert!(
        data_of(&rig.config.path, &name_of(&pack.key).hex)
            .starts_with(rig.config.path.join("extents-v1"))
    );
}

/// Asserts that the entries show in `/metricz`: the gauges of entries and filled bytes, the
/// counters of fills and bytes, and the failures by reason, each family with one `# TYPE` line.
#[tokio::test]
async fn the_entries_show_in_the_metrics() {
    let rig = Rig::new("tsext-metrics");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 100).await.unwrap();
    fill(&rig, &pack, 0, 100).await.unwrap();
    let wrong = digest(b"wrong");
    let want = RangeWant {
        pack: &pack.key,
        known: None,
        offset: 200,
        len: 100,
        sha256: &wrong,
    };
    assert!(rig.cache.fill_range(&want).await.is_err());
    let text = rig.cache.metrics();
    for line in [
        "# TYPE tierstore_cache_extent_entries gauge\ntierstore_cache_extent_entries 1\n",
        "# TYPE tierstore_cache_extent_bytes gauge\ntierstore_cache_extent_bytes 100\n",
        "tierstore_cache_extent_fills_total 1\n",
        "tierstore_cache_extent_fill_bytes_total 100\n",
        "tierstore_cache_extent_held_total 1\n",
        "tierstore_cache_extent_joins_total 0\n",
        "tierstore_cache_extent_promotions_total 0\n",
        "tierstore_cache_extent_evictions_total 0\n",
        "# TYPE tierstore_cache_extent_fill_failures_total counter\n",
        "tierstore_cache_extent_fill_failures_total{reason=\"mismatch\"} 1\n",
        "tierstore_cache_extent_fill_failures_total{reason=\"upstream\"} 0\n",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
    assert_eq!(
        text.matches("# TYPE tierstore_cache_extent_bytes ").count(),
        1
    );
}

/// Asserts that the record of use is written back to the file of an entry (the file of the entry
/// by extents, not a name in `obj/`): a ranged read of an entry and then a pass of the eviction
/// set the time of the entry's file to the time of the read, which a restart reads back.
#[tokio::test]
async fn the_use_of_an_entry_is_written_back_to_its_file() {
    let rig = Rig::new("tsext-touch");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    rig.clock.set(777_000);
    rig.cache
        .get(&pack.key, Some(ByteRange::new(1200, 10)))
        .await
        .unwrap();
    let _ = rig.cache.evict();
    let touched = rig.disk.touched.lock().unwrap().clone();
    assert_eq!(touched, [(data(&rig, &pack), 777_000)]);
}

/// Asserts that a cache that takes the lock after more names were forgotten than it remembers
/// removes every entry by extents with the objects it empties, and that a cache that stays
/// within the limit keeps the entries of the names that were not forgotten.
#[tokio::test]
async fn more_forgotten_names_than_the_cache_remembers_remove_the_entries() {
    let rig = Rig::writing("tsext-overflow");
    let pack = pack_in(&rig).await;
    for key in ["cas/b/a", "cas/b/b"] {
        rig.put(key, &pattern(30)).await;
    }
    let holder = rig.locked();
    fill_in(&holder, &pack, 0, 100).await;
    let reader = rig.locked_with(|config| config.forget_limit = 1);
    for key in ["cas/b/a", "cas/b/b"] {
        reader.delete(key).await.unwrap();
    }
    drop(holder);
    assert_eq!(reader.tick().unwrap(), super::super::Tick::Took);
    assert_eq!(reader.extents_held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
}

/// A fill of `[from, to)` of `pack` through `cache`.
async fn fill_in(cache: &super::super::LocalCache, pack: &Pack, from: u64, to: u64) {
    let sha = digest(&pack.bytes()[from as usize..to as usize]);
    let want = RangeWant {
        pack: &pack.key,
        known: None,
        offset: from,
        len: to - from,
        sha256: &sha,
    };
    cache.fill_range(&want).await.unwrap();
}
