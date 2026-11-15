//! The fill of a range of a pack: what it keeps, what it refuses, and how fills share and wait.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::sync::atomic::Ordering;

use super::super::layout::{Sidecar, meta_path, name_of};
use super::super::metrics::read;
use super::super::pack_testing::Pack;
use super::super::testing::{Quirk, Rig, digest, until};
use super::super::{Mode, Tick, locked};
use super::files::{ExtentSidecar, data_of, meta_of};
use super::{RangeError, RangeWant};
use crate::{Backend, Error, Op};

/// The pack `cas/pack/one` of three blobs (1000, 2000 and 3000 bytes) in the upstream of `rig`.
pub(super) async fn pack_in(rig: &Rig) -> Pack {
    Pack::put(rig, "cas/pack/one", &[1000, 2000, 3000]).await
}

/// The range `[from, to)` of `pack` asked for under the digest of its true bytes.
pub(super) async fn fill(rig: &Rig, pack: &Pack, from: u64, to: u64) -> Result<(), RangeError> {
    let bytes = &pack.bytes()[from as usize..to as usize];
    let sha = digest(bytes);
    let want = RangeWant {
        pack: &pack.key,
        known: None,
        offset: from,
        len: to - from,
        sha256: &sha,
    };
    rig.cache.fill_range(&want).await
}

/// The data file of the entry of `pack` by extents.
pub(super) fn data(rig: &Rig, pack: &Pack) -> std::path::PathBuf {
    data_of(&rig.config.path, &name_of(&pack.key).hex)
}

/// The sidecar of the entry of `pack` by extents.
fn sidecar(rig: &Rig, pack: &Pack) -> ExtentSidecar {
    let text = std::fs::read(meta_of(&rig.config.path, &name_of(&pack.key).hex)).unwrap();
    serde_json::from_slice(&text).unwrap()
}

/// Asserts that a range is kept in a sparse file of the pack's full size beside `obj/`: the bytes
/// of the range are in it at their own offsets and every other byte is a hole; the sidecar lists
/// the one range, the size, and the tag the upstream gave; the pack is not an object of `obj/`;
/// the cache's bytes count the range once and its objects none; and the upstream was asked for
/// the pack once (HEAD) and for the range once (GET), nothing else. A defect that kept the whole
/// pack, wrote the range at offset 0, or counted the file's apparent size would show here.
#[tokio::test]
async fn a_range_is_kept_in_a_sparse_file_of_the_packs_size() {
    let rig = Rig::new("tsext-range");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    let file = std::fs::read(data(&rig, &pack)).unwrap();
    assert_eq!(file.len(), 6000);
    assert_eq!(file[1000..3000], pack.bytes()[1000..3000]);
    assert!(file[..1000].iter().chain(&file[3000..]).all(|b| *b == 0));
    let mode = std::fs::metadata(data(&rig, &pack))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644);
    let side = sidecar(&rig, &pack);
    assert_eq!((side.key.as_str(), side.size), ("cas/pack/one", 6000));
    assert_eq!(side.extents, [[1000, 2000]]);
    assert_eq!(side.tag, digest(pack.bytes()));
    assert!(!rig.has_file(&pack.key));
    assert_eq!(rig.cache.held(), (2000, 0));
    assert_eq!(rig.cache.extents_held(), (1, 2000));
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (1, 1));
    assert_eq!(rig.upstream.get_ranges(), [Some((1000, 2000))]);
    let counters = &rig.cache.inner.extents.counters;
    assert_eq!(
        (read(&counters.fills), read(&counters.fill_bytes)),
        (1, 2000)
    );
}

/// Asserts that a caller that knows the pack's HEAD (the create call has it from its
/// validation) spares the upstream the HEAD: the entry is made of what it said.
#[tokio::test]
async fn a_caller_that_knows_the_pack_spares_the_head() {
    let rig = Rig::new("tsext-known");
    let pack = pack_in(&rig).await;
    let meta = rig.tamper.head(&pack.key).await.unwrap();
    let sha = digest(&pack.bytes()[..500]);
    let want = RangeWant {
        pack: &pack.key,
        known: Some(&meta),
        offset: 0,
        len: 500,
        sha256: &sha,
    };
    rig.cache.fill_range(&want).await.unwrap();
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (0, 1));
    assert_eq!(sidecar(&rig, &pack).tag, meta.tag);
}

/// Asserts that a range the cache holds is answered at once, from inside a held range too, and
/// asks the upstream nothing more; and that the same is true of a pack that is whole in `obj/`,
/// which is not given an entry by extents.
#[tokio::test]
async fn a_range_that_is_held_asks_nothing_and_makes_no_entry() {
    let rig = Rig::new("tsext-held");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    fill(&rig, &pack, 1500, 2500).await.unwrap();
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (1, 1));
    assert_eq!(read(&rig.cache.inner.extents.counters.held), 2);

    let whole = Rig::new("tsext-held-whole");
    let pack = pack_in(&whole).await;
    whole.get(&pack.key).await.unwrap();
    let gets = whole.calls(Op::Get);
    fill(&whole, &pack, 10, 20).await.unwrap();
    assert_eq!(whole.calls(Op::Get), gets);
    assert_eq!(whole.cache.extents_held(), (0, 0));
    assert!(!whole.config.path.join("extents-v1").exists());
}

/// Asserts that a range that overlaps held bytes reads the range from the upstream once and writes
/// only the bytes no verified range holds, that a range that touches one joins it in the sidecar,
/// and that a range larger than `fill_chunk` is read in chunks from its offset.
#[tokio::test]
async fn ranges_join_and_a_fill_reads_in_chunks_from_its_offset() {
    let rig = Rig::with("tsext-join", |config| config.fill_chunk = 700);
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 1000).await.unwrap();
    fill(&rig, &pack, 500, 2000).await.unwrap();
    assert_eq!(
        rig.upstream.get_ranges(),
        [
            Some((0, 700)),
            Some((700, 300)),
            Some((500, 700)),
            Some((1200, 700)),
            Some((1900, 100))
        ]
    );
    fill(&rig, &pack, 2000, 2500).await.unwrap();
    assert_eq!(sidecar(&rig, &pack).extents, [[0, 2500]]);
    let file = std::fs::read(data(&rig, &pack)).unwrap();
    assert_eq!(file[..2500], pack.bytes()[..2500]);
    assert_eq!(rig.cache.extents_held(), (1, 2500));
    assert_eq!(read(&rig.cache.inner.extents.counters.fill_bytes), 2500);
}

/// Asserts that an entry that fills to the whole pack becomes an ordinary object of `obj/`: the file
/// is the whole pack at the name `digest-v1` gives the key, mode 0444, with a sidecar that has the
/// SHA-256 of all the bytes and the tag; nothing is left in `extents-v1`; the cache counts it as
/// one object of 6000 bytes and no entry; the upstream was asked for the version once more (one
/// HEAD) before the move; and a read of the whole pack and of a range is then a hit.
#[tokio::test]
async fn an_entry_that_fills_becomes_whole() {
    let rig = Rig::new("tsext-whole");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 1000).await.unwrap();
    fill(&rig, &pack, 3000, 6000).await.unwrap();
    assert!(!rig.has_file(&pack.key));
    fill(&rig, &pack, 1000, 3000).await.unwrap();
    assert!(rig.has_file(&pack.key));
    assert_eq!(std::fs::read(rig.file(&pack.key)).unwrap(), pack.bytes());
    let meta = std::fs::metadata(rig.file(&pack.key)).unwrap();
    assert_eq!(
        (meta.permissions().mode() & 0o777, meta.nlink()),
        (0o444, 1)
    );
    let text = std::fs::read(meta_path(&rig.config.path, &name_of(&pack.key).hex)).unwrap();
    let side: Sidecar = serde_json::from_slice(&text).unwrap();
    assert_eq!(
        (side.sha256.as_str(), side.size),
        (digest(pack.bytes()).as_str(), 6000)
    );
    assert_eq!(side.tag, digest(pack.bytes()));
    let name = name_of(&pack.key).hex;
    assert!(!data_of(&rig.config.path, &name).exists());
    assert!(!meta_of(&rig.config.path, &name).exists());
    assert_eq!(rig.cache.held(), (6000, 1));
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert_eq!(read(&rig.cache.inner.extents.counters.promotions), 1);
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (2, 3));
    assert_eq!(rig.get(&pack.key).await.unwrap(), pack.bytes());
    assert_eq!(rig.calls(Op::Get), 3);
}

/// Asserts that bytes that do not hash to the digest are refused and leave nothing: no entry, no
/// file in `extents-v1`, no byte in the cache's count, and the failure counted. An entry that had
/// nothing else is removed whole.
#[tokio::test]
async fn bytes_that_do_not_hash_to_the_digest_leave_nothing() {
    let rig = Rig::new("tsext-mismatch");
    let pack = pack_in(&rig).await;
    let sha = digest(b"other bytes");
    let want = RangeWant {
        pack: &pack.key,
        known: None,
        offset: 0,
        len: 500,
        sha256: &sha,
    };
    assert_eq!(rig.cache.fill_range(&want).await, Err(RangeError::Mismatch));
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
    assert!(!meta_of(&rig.config.path, &name_of(&pack.key).hex).exists());
    assert_eq!(rig.cache.inner.extents.counters.failures("mismatch"), 1);
}

/// Asserts that a fill whose bytes fail the hash never changes a byte a verified range holds, even
/// where the failed range overlaps it: the upstream's object was replaced by one of the same size
/// and other bytes, the entry still has the old version (it asked no HEAD), and the range read
/// from the new object over the verified one is refused; the verified bytes are as they were and
/// the sidecar lists them alone.
#[tokio::test]
async fn a_failed_fill_leaves_verified_bytes_as_they_were() {
    let rig = Rig::new("tsext-intact");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 1000).await.unwrap();
    let other: Vec<u8> = pack.bytes().iter().map(|b| b.wrapping_add(1)).collect();
    rig.replace(&pack.key, &other).await;
    assert_eq!(
        fill(&rig, &pack, 500, 1500).await,
        Err(RangeError::Mismatch)
    );
    let file = std::fs::read(data(&rig, &pack)).unwrap();
    assert_eq!(file[..1000], pack.bytes()[..1000]);
    assert_eq!(sidecar(&rig, &pack).extents, [[0, 1000]]);
    assert_eq!(rig.cache.extents_held(), (1, 1000));
}

/// Asserts that a pack of another version than the entry's (the caller's HEAD says so) replaces the
/// entry: the old ranges are gone and the new range is the only one, with the new tag.
#[tokio::test]
async fn another_version_of_the_pack_replaces_the_entry() {
    let rig = Rig::new("tsext-version");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 1000).await.unwrap();
    let other: Vec<u8> = pack.bytes().iter().map(|b| b.wrapping_add(1)).collect();
    rig.replace(&pack.key, &other).await;
    let meta = rig.tamper.head(&pack.key).await.unwrap();
    let sha = digest(&other[2000..2500]);
    let want = RangeWant {
        pack: &pack.key,
        known: Some(&meta),
        offset: 2000,
        len: 500,
        sha256: &sha,
    };
    rig.cache.fill_range(&want).await.unwrap();
    let side = sidecar(&rig, &pack);
    assert_eq!((side.extents, side.tag), (vec![[2000, 500]], meta.tag));
    assert_eq!(rig.cache.extents_held(), (1, 500));
}

/// Asserts the refusals that need no upstream and the ones the upstream or the disk gives, each
/// by its name: a key the cache does not keep, an empty range, a range whose end overflows, a bad
/// digest, a range past the end of the pack (both for an entry and for a whole pack), a pack the
/// upstream does not have (404), an upstream that gives no tag (501), a body of another length,
/// a range larger than the budget, a directory that cannot be written, and a cache that does not
/// hold the lock. None leaves an entry.
#[tokio::test]
async fn each_refusal_has_its_name_and_leaves_nothing() {
    let rig = Rig::with("tsext-refuse", |config| config.budget = 4000);
    let pack = pack_in(&rig).await;
    let sha = digest(&pack.bytes()[..10]);
    let ask = |key: &'static str, offset: u64, len: u64, sha: String| {
        let cache = rig.cache.clone();
        async move {
            let want = RangeWant {
                pack: key,
                known: None,
                offset,
                len,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        }
    };
    let one = "cas/pack/one";
    assert_eq!(
        ask("other/pack", 0, 10, sha.clone()).await,
        Err(RangeError::NotKept)
    );
    assert_eq!(ask(one, 0, 0, sha.clone()).await, Err(RangeError::Empty));
    assert_eq!(
        ask(one, u64::MAX, 2, sha.clone()).await,
        Err(RangeError::Empty)
    );
    assert_eq!(
        ask(one, 0, 10, "xyz".to_owned()).await,
        Err(RangeError::Empty)
    );
    assert_eq!(
        ask(one, 5990, 100, sha.clone()).await,
        Err(RangeError::Past)
    );
    assert_eq!(
        ask("cas/pack/none", 0, 10, sha.clone()).await,
        Err(RangeError::Upstream(404))
    );
    assert_eq!(
        ask(one, 0, 5000, sha.clone()).await,
        Err(RangeError::NoSpace)
    );
    rig.tamper.blank_tags.store(true, Ordering::SeqCst);
    assert_eq!(
        ask(one, 0, 10, sha.clone()).await,
        Err(RangeError::Upstream(501))
    );
    rig.tamper.blank_tags.store(false, Ordering::SeqCst);
    rig.tamper.short.store(true, Ordering::SeqCst);
    assert_eq!(ask(one, 0, 10, sha.clone()).await, Err(RangeError::Short));
    rig.tamper.short.store(false, Ordering::SeqCst);
    std::fs::remove_dir_all(rig.config.path.join("extents-v1")).unwrap();
    std::fs::write(rig.config.path.join("extents-v1"), b"a file").unwrap();
    assert_eq!(ask(one, 0, 10, sha).await, Err(RangeError::Io));
    assert_eq!(rig.cache.extents_held(), (0, 0));
    let counters = &rig.cache.inner.extents.counters;
    for (why, count) in [("upstream", 2), ("short", 1), ("no_space", 1), ("io", 1)] {
        assert_eq!(counters.failures(why), count, "{why}");
    }
    assert_eq!(counters.failures("nothing"), 0);

    let whole = Rig::new("tsext-refuse-whole");
    let pack = pack_in(&whole).await;
    whole.get(&pack.key).await.unwrap();
    let sha = digest(b"x");
    let want = RangeWant {
        pack: &pack.key,
        known: None,
        offset: 5990,
        len: 100,
        sha256: &sha,
    };
    assert_eq!(whole.cache.fill_range(&want).await, Err(RangeError::Past));
}

/// Asserts that a cache that does not hold the lock of its directory fills nothing and reads no
/// entry the holder made: it is `read-only` to the caller, with the directory as it was.
#[tokio::test]
async fn a_cache_without_the_lock_fills_nothing() {
    let rig = Rig::new("tsext-readonly");
    let pack = pack_in(&rig).await;
    let holder = rig.locked();
    let waiting = rig.locked();
    let sha = digest(&pack.bytes()[..10]);
    let want = RangeWant {
        pack: &pack.key,
        known: None,
        offset: 0,
        len: 10,
        sha256: &sha,
    };
    assert_eq!(waiting.fill_range(&want).await, Err(RangeError::ReadOnly));
    assert_eq!(waiting.extents_held(), (0, 0));
    holder.fill_range(&want).await.unwrap();
    assert!(data(&rig, &pack).is_file());
    let later = rig.locked();
    assert_eq!(later.extents_held(), (0, 0));
    assert!(
        waiting
            .metrics()
            .contains("tierstore_cache_extent_entries 0\n")
    );
    assert!(
        holder
            .metrics()
            .contains("tierstore_cache_extent_entries 1\n")
    );
}

/// Asserts that two requests for one range and digest are one fill: one GET, both answered `ok`,
/// the second counted as a join.
#[tokio::test]
async fn two_requests_for_one_range_are_one_fill() {
    let rig = Rig::new("tsext-join-flight");
    let pack = pack_in(&rig).await;
    let gate = rig.tamper.hold();
    let ask = || {
        let cache = rig.cache.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[1000..3000]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: 1000,
                len: 2000,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    let first = ask();
    until("the first fill is at the upstream", || {
        rig.calls(Op::Get) == 1
    })
    .await;
    let second = ask();
    let counters = &rig.cache.inner.extents.counters;
    until("the second joined", || read(&counters.joins) == 1).await;
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(rig.calls(Op::Get), 1);
    assert_eq!(rig.cache.extents_held(), (1, 2000));
    assert!(rig.cache.inner.extents.flights.lock().unwrap().is_empty());
}

/// Asserts that a request for a range that overlaps a fill that runs waits for it and then reads
/// only what is left: while the first is at the upstream the second has no flight of its own, and
/// when the first ends the second fills the rest, so the sidecar lists one joined range and the
/// bytes are right.
#[tokio::test]
async fn an_overlapping_range_waits_for_the_fill_that_runs() {
    let rig = Rig::new("tsext-overlap");
    let pack = pack_in(&rig).await;
    let gate = rig.tamper.hold();
    let ask = |from: usize, to: usize| {
        let cache = rig.cache.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[from..to]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: from as u64,
                len: (to - from) as u64,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    let first = ask(0, 3000);
    until("the first fill is at the upstream", || {
        rig.calls(Op::Get) == 1
    })
    .await;
    let second = ask(2000, 4000);
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(rig.calls(Op::Get), 1);
    assert_eq!(rig.cache.inner.extents.flights.lock().unwrap().len(), 1);
    gate.add_permits(2);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(sidecar(&rig, &pack).extents, [[0, 4000]]);
    let file = std::fs::read(data(&rig, &pack)).unwrap();
    assert_eq!(file[..4000], pack.bytes()[..4000]);
}

/// Asserts that the pool of the fill socket is waited for, and `busy` is the answer when no
/// permit comes free in time, with the fill that held the permit not hurt and the entry of the
/// refused range not made. The clock is paused and moves by itself when every task waits.
#[tokio::test(start_paused = true)]
async fn a_fill_is_busy_when_no_permit_comes_free() {
    let rig = Rig::with("tsext-busy", |config| {
        config.fill_socket_concurrency = 1;
        config.fill_socket_wait = std::time::Duration::from_secs(5);
    });
    let pack = pack_in(&rig).await;
    let gate = rig.tamper.hold();
    let first = {
        let cache = rig.cache.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[..100]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: 0,
                len: 100,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    until("the first fill is at the upstream", || {
        rig.calls(Op::Get) == 1
    })
    .await;
    assert_eq!(fill(&rig, &pack, 4000, 4100).await, Err(RangeError::Busy));
    gate.add_permits(2);
    first.await.unwrap().unwrap();
    assert_eq!(rig.cache.extents_held(), (1, 100));
    assert_eq!(rig.cache.inner.extents.counters.failures("busy"), 1);
    fill(&rig, &pack, 4000, 4100).await.unwrap();
}

/// Asserts that a fill that a write or a delete through the cache overtakes is `changed`: the entry
/// is removed with its files, and what was read is not recorded.
#[tokio::test]
async fn a_delete_during_a_fill_removes_the_entry_and_the_fill_is_changed() {
    let rig = Rig::new("tsext-forget");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 100).await.unwrap();
    let gate = rig.tamper.hold();
    let task = {
        let cache = rig.cache.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[200..300]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: 200,
                len: 100,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    until("the fill is at the upstream", || rig.calls(Op::Get) == 2).await;
    rig.cache.inner.forget(&pack.key);
    assert_eq!(rig.cache.extents_held(), (0, 0));
    gate.add_permits(1);
    assert_eq!(task.await.unwrap(), Err(RangeError::Changed));
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
    assert_eq!(rig.cache.inner.extents.counters.failures("changed"), 1);
}

/// Asserts that a fill whose task panics is `aborted`, counted once, with no entry left and the
/// flight out of the table so that the next request fills.
#[tokio::test]
async fn a_fill_that_panics_is_aborted() {
    let rig = Rig::new("tsext-abort");
    let pack = pack_in(&rig).await;
    rig.tamper.panic_once.store(true, Ordering::SeqCst);
    assert_eq!(fill(&rig, &pack, 0, 100).await, Err(RangeError::Aborted));
    assert_eq!(rig.cache.inner.extents.counters.failures("aborted"), 1);
    assert_eq!(rig.cache.extents_held(), (0, 0));
    fill(&rig, &pack, 0, 100).await.unwrap();
}

/// Asserts that a cache that loses the lock of its directory in the middle of a fill writes no
/// more: the fill is `read-only` and the sidecar is not written.
#[tokio::test]
async fn a_fill_stops_when_the_lock_is_lost() {
    let rig = Rig::new("tsext-lost");
    let pack = pack_in(&rig).await;
    let holder = rig.locked();
    let gate = rig.tamper.hold();
    let task = {
        let cache = holder.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[..100]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: 0,
                len: 100,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    until("the fill is at the upstream", || rig.calls(Op::Get) == 1).await;
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    assert_eq!(holder.tick().unwrap(), Tick::Lost);
    assert_eq!(*holder.watch_mode().borrow(), Mode::ReadOnly);
    gate.add_permits(1);
    assert_eq!(task.await.unwrap(), Err(RangeError::ReadOnly));
}

/// Asserts that an entry that is whole is not moved into `obj/` if the upstream says the pack is
/// another version now (the entry is removed, the range is kept as a fill), and that it stays
/// where it is, whole, if the HEAD fails (it serves its ranges), or the link fails.
#[tokio::test]
async fn a_whole_entry_that_cannot_move_is_removed_or_stays() {
    let rig = Rig::new("tsext-move");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 5000).await.unwrap();
    rig.tamper.lie_size.store(true, Ordering::SeqCst);
    fill(&rig, &pack, 5000, 6000).await.unwrap();
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert!(!rig.has_file(&pack.key));
    assert!(!data(&rig, &pack).exists());
    rig.tamper.lie_size.store(false, Ordering::SeqCst);

    rig.upstream.fail_next(Op::Head, 0);
    fill(&rig, &pack, 0, 5000).await.unwrap();
    rig.upstream.fail_next_on(
        Op::Head,
        "cas/pack/one",
        Error::Unavailable("down".to_owned()),
    );
    fill(&rig, &pack, 5000, 6000).await.unwrap();
    assert_eq!(rig.cache.extents_held(), (1, 6000));
    assert!(!rig.has_file(&pack.key));
    assert_eq!(sidecar(&rig, &pack).extents, [[0, 6000]]);

    let rig = Rig::new("tsext-move-link");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 5000).await.unwrap();
    rig.disk
        .set(Quirk::LinkFails(std::io::ErrorKind::PermissionDenied));
    fill(&rig, &pack, 5000, 6000).await.unwrap();
    assert_eq!(rig.cache.extents_held(), (1, 6000));
    assert!(!rig.has_file(&pack.key));
}

/// Asserts that an entry that is whole and meets a whole object that was linked at the same name
/// meanwhile is taken out and its files removed, the object kept; and that a key named by its content
/// whose bytes do not hash to its name is removed instead of moved.
#[tokio::test]
async fn a_whole_entry_gives_way_to_an_object_that_is_there() {
    let rig = Rig::new("tsext-existed");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 5000).await.unwrap();
    rig.disk
        .set(Quirk::LinkFails(std::io::ErrorKind::AlreadyExists));
    fill(&rig, &pack, 5000, 6000).await.unwrap();
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
    assert!(!meta_of(&rig.config.path, &name_of(&pack.key).hex).exists());

    let rig = Rig::new("tsext-content");
    let wrong = format!("cas/l/{}", digest(b"not the bytes"));
    rig.put(&wrong, &[7; 300]).await;
    let sha = digest(&[7; 300]);
    let want = RangeWant {
        pack: &wrong,
        known: None,
        offset: 0,
        len: 300,
        sha256: &sha,
    };
    rig.cache.fill_range(&want).await.unwrap();
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that an entry that was filled whole while it was being moved, and that a delete
/// overtook (the hook runs between the link and the index), leaves nothing in `obj/` either.
#[tokio::test]
async fn a_delete_during_the_move_leaves_no_object() {
    let rig = Rig::new("tsext-move-forget");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 5000).await.unwrap();
    let inner = rig.cache.inner.clone();
    let key = pack.key.clone();
    *locked(&rig.cache.inner.before_insert) = Some(Box::new(move || inner.forget(&key)));
    fill(&rig, &pack, 5000, 6000).await.unwrap();
    assert!(!rig.has_file(&pack.key));
    assert_eq!(rig.cache.held(), (0, 0));
    assert_eq!(rig.cache.extents_held(), (0, 0));
}

/// Asserts that an entry whose file is gone when the move starts stays as it is: the hash of
/// a file that is not there is no reason to remove or link anything.
#[tokio::test]
async fn a_whole_entry_whose_file_is_gone_is_left_alone() {
    let rig = Rig::new("tsext-move-gone");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 5000).await.unwrap();
    let gate = rig.tamper.hold_heads();
    let task = {
        let cache = rig.cache.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[5000..]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: 5000,
                len: 1000,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    until("the move waits for the upstream's HEAD", || {
        rig.tamper.heading() == 1
    })
    .await;
    std::fs::remove_file(data(&rig, &pack)).unwrap();
    gate.add_permits(1);
    task.await.unwrap().unwrap();
    assert!(!rig.has_file(&pack.key));
    assert_eq!(rig.cache.held(), (6000, 0));
}

/// Asserts the words each way a fill of a range can end is answered with.
#[test]
fn each_way_a_range_fill_can_end_has_its_words() {
    let words = [
        (RangeError::NotKept, "not kept"),
        (RangeError::Empty, "empty range or bad digest"),
        (RangeError::Past, "past the end"),
        (RangeError::Upstream(404), "upstream 404"),
        (RangeError::Mismatch, "mismatch"),
        (RangeError::Short, "short"),
        (RangeError::NoSpace, "no space"),
        (RangeError::Busy, "busy"),
        (RangeError::ReadOnly, "read-only"),
        (RangeError::Io, "io"),
        (RangeError::Changed, "changed"),
        (RangeError::Aborted, "aborted"),
    ];
    for (error, text) in words {
        assert_eq!(error.to_string(), text);
    }
    assert_eq!(
        RangeError::from(Error::NotFound("k".to_owned())),
        RangeError::Upstream(404)
    );
}

/// Asserts that a range past the end of an entry is refused as such, as it is for a pack that
/// is whole in the cache.
#[tokio::test]
async fn a_range_past_the_end_of_an_entry_is_refused() {
    let rig = Rig::new("tsext-past");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 100).await.unwrap();
    let sha = digest(b"x");
    let want = RangeWant {
        pack: &pack.key,
        known: None,
        offset: 5990,
        len: 100,
        sha256: &sha,
    };
    assert_eq!(rig.cache.fill_range(&want).await, Err(RangeError::Past));
    assert_eq!(rig.cache.extents_held(), (1, 100));
}

/// Asserts that a pack that is whole in the index but whose file is gone is not a hold: the entry
/// is dropped, as any read finds it, and the range is filled as an entry by extents.
#[tokio::test]
async fn a_whole_pack_whose_file_is_gone_is_filled_by_extents() {
    let rig = Rig::new("tsext-lie");
    let pack = pack_in(&rig).await;
    rig.get(&pack.key).await.unwrap();
    std::fs::remove_file(rig.file(&pack.key)).unwrap();
    fill(&rig, &pack, 100, 200).await.unwrap();
    assert_eq!(rig.cache.held(), (100, 0));
    assert_eq!(rig.cache.extents_held(), (1, 100));
}

/// Asserts that fills of other packs, and of ranges of one pack that do not overlap, do not wait for
/// one another: all three are at the upstream together.
#[tokio::test]
async fn fills_that_do_not_overlap_run_together() {
    let rig = Rig::new("tsext-together");
    let one = pack_in(&rig).await;
    let two = Pack::put(&rig, "cas/pack/two", &[500, 500]).await;
    let gate = rig.tamper.hold();
    let ask = |pack: &Pack, from: usize, to: usize| {
        let cache = rig.cache.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[from..to]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: from as u64,
                len: (to - from) as u64,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    let tasks = [ask(&one, 4000, 4100), ask(&two, 0, 100), ask(&one, 0, 100)];
    until("all three are at the upstream", || rig.calls(Op::Get) == 3).await;
    gate.add_permits(3);
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(rig.cache.extents_held(), (2, 300));
}

/// Asserts that a flight that finds its range held after it has its permit (another fill kept
/// it while it waited) asks the upstream nothing and is answered `ok`.
#[tokio::test]
async fn a_flight_whose_range_was_filled_while_it_waited_asks_nothing() {
    let rig = Rig::new("tsext-late");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 100).await.unwrap();
    let wanted = super::fill::Wanted {
        key: pack.key.clone(),
        known: None,
        start: 10,
        end: 90,
        sha256: digest(b"unchecked"),
    };
    let gets = rig.calls(Op::Get);
    rig.cache
        .inner
        .range_fill(&name_of(&pack.key), &wanted)
        .await
        .unwrap();
    assert_eq!(rig.calls(Op::Get), gets);
}

/// Asserts that a fill whose pack became whole in the cache while it asked the upstream for the
/// pack's version has nothing to fill, and that one whose cache lost the lock in that time
/// writes nothing and is `read-only`.
#[tokio::test]
async fn a_fill_that_waited_for_the_head_looks_again_at_the_cache() {
    let rig = Rig::new("tsext-head-wait");
    let pack = pack_in(&rig).await;
    let hex = name_of(&pack.key).hex;
    let gate = rig.tamper.hold_heads();
    let task = {
        let cache = rig.cache.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[..100]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: 0,
                len: 100,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    until("the fill waits for the HEAD", || rig.tamper.heading() == 1).await;
    locked(&rig.cache.inner.index).insert(&hex, (6000, "t".to_owned(), 1), None);
    gate.add_permits(1);
    task.await.unwrap().unwrap();
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert!(!data(&rig, &pack).exists());

    let rig = Rig::new("tsext-head-lost");
    let pack = pack_in(&rig).await;
    let holder = rig.locked();
    let gate = rig.tamper.hold_heads();
    let task = {
        let cache = holder.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[..100]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: 0,
                len: 100,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    until("the fill waits for the HEAD", || rig.tamper.heading() == 1).await;
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    assert_eq!(holder.tick().unwrap(), Tick::Lost);
    gate.add_permits(1);
    assert_eq!(task.await.unwrap(), Err(RangeError::ReadOnly));
    assert!(!data(&rig, &pack).exists());
}

/// Asserts that the sidecar is not written for an entry that is gone, nor by a cache that has not
/// the lock it started under: the answer is `ok` and the directory is as it was.
#[tokio::test]
async fn a_sidecar_is_written_only_for_an_entry_a_cache_that_owns_the_directory_has() {
    let rig = Rig::new("tsext-save");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 100).await.unwrap();
    let hex = name_of(&pack.key).hex;
    let meta = meta_of(&rig.config.path, &hex);
    std::fs::remove_file(&meta).unwrap();
    let generation = rig.cache.inner.generation();
    rig.cache
        .inner
        .extent_save(&hex, generation + 1)
        .await
        .unwrap();
    assert!(!meta.exists());
    rig.cache.inner.extent_save("cd", generation).await.unwrap();
    rig.cache.inner.extent_save(&hex, generation).await.unwrap();
    assert!(meta.is_file());
}

/// Asserts that an entry that is whole, and meets a whole object of its name that was linked
/// meanwhile, is not touched if the entry was taken out too (a delete overtook the move): the
/// files of the entry that took its place are not removed by the move that lost.
#[tokio::test]
async fn a_move_that_finds_the_object_there_and_the_entry_gone_removes_nothing() {
    let rig = Rig::new("tsext-existed-gone");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 5000).await.unwrap();
    rig.disk
        .set(Quirk::LinkFails(std::io::ErrorKind::AlreadyExists));
    let inner = rig.cache.inner.clone();
    let key = pack.key.clone();
    *locked(&rig.cache.inner.before_insert) = Some(Box::new(move || inner.forget(&key)));
    fill(&rig, &pack, 5000, 6000).await.unwrap();
    assert_eq!(rig.cache.extents_held(), (0, 0));
}

/// Asserts that a range that lies inside bytes a verified range holds is read and hashed but
/// writes none of its chunks over them: with chunks of 700 bytes the middle chunk of the range
/// is inside the held bytes, and the bytes of the file are the pack's.
#[tokio::test]
async fn a_chunk_inside_held_bytes_is_not_written() {
    let rig = Rig::with("tsext-inside", |config| config.fill_chunk = 700);
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 500, 2500).await.unwrap();
    fill(&rig, &pack, 0, 3000).await.unwrap();
    let file = std::fs::read(data(&rig, &pack)).unwrap();
    assert_eq!(file[..3000], pack.bytes()[..3000]);
    assert_eq!(sidecar(&rig, &pack).extents, vec![[0, 3000]]);
}

/// Asserts that a fill whose entry was deleted after it took its hold, and that then made the
/// data file, leaves no file behind when the hold ends: the entry is gone and so is its data.
#[tokio::test]
async fn a_hold_that_ends_on_a_removed_entry_removes_the_data_file_it_made() {
    let rig = Rig::new("tsext-orphan");
    let pack = pack_in(&rig).await;
    fill(&rig, &pack, 0, 100).await.unwrap();
    let hex = name_of(&pack.key).hex;
    let object = locked(&rig.cache.inner.index)
        .partials
        .get(&hex)
        .map(|entry| entry.object.clone())
        .unwrap();
    let serial = locked(&rig.cache.inner.index)
        .extent_hold(&hex, &object)
        .unwrap();
    let hold = super::fill::Holding {
        inner: std::sync::Arc::clone(&rig.cache.inner),
        hex: hex.clone(),
        serial,
    };
    rig.cache.inner.forget(&pack.key);
    rig.cache.inner.extent_file(&hex, 6000).await.unwrap();
    assert!(data(&rig, &pack).exists());
    drop(hold);
    assert!(!data(&rig, &pack).exists());
    assert_eq!(rig.cache.extents_held(), (0, 0));
}
