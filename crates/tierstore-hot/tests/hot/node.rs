//! Section 14 rows: "Backpressure marks on a scripted hot fill" (the hot tier's part: 503
//! above `dirty_slow`, 507 above `dirty_reject` after reclaim, reclaim order); "Create-only";
//! and the hot tier's scrub, reclaim, startup sweep and reads.
//!
//! Catches: a PUT admitted above `dirty_slow`, or throttled below it; a throttle that is not
//! counted; a 507 answered while clean bytes could still be reclaimed, or not answered once
//! nothing is clean; a reclaim that drops a dirty byte, drops a clean segment before a cache
//! segment, or drops the most recently read file first; `mark_clean` without `keep_clean` that
//! leaves the file; a scrub that checks before its period, leaves a flipped byte in place,
//! repairs from a peer copy that is bad itself, or keeps a bad cache segment; a read of a
//! flipped record that returns its bytes; a startup that leaves an `.up`, `.tmp`, `.repair` or
//! cache file behind or reports it wrongly; a second PUT of a key that writes a second record,
//! answers 200 for other bytes, or is admitted when the key check is not definitive.

use crate::support::Sim;
use crate::support::config;
use bytes::Bytes;
use futures::StreamExt;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::alloc::AllocatedId;
use tierstore_format::digest::GenerationHasher;
use tierstore_format::ids::{BlockId, ExpiryClass, Generation, KeyHash, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::record::RECORD_HEADER_LEN;
use tierstore_format::seal::Ballot;
use tierstore_format::tomb::Tombstone;
use tierstore_harness::disk::FakeDisk;
use tierstore_hot::HotNode;
use tierstore_hot::config::HotConfig;
use tierstore_hot::node::ReadError;
use tierstore_index::recover::Mode;
use tierstore_metrics::labels::ReclaimKind;
use tierstore_store_api::hot::{Acked, HotError, HotStore, LargePut, SealReason, Unavailable};
use tierstore_store_api::index::{EntryLocation, Lookup};

// ---------------------------------------------------------------------------------------------
// Helpers.

/// A 100 000-byte device: `dirty_slow` is 85 000 bytes and `dirty_reject` 90 000.
fn small_device() -> HotConfig {
    let mut cfg = config(3, 3);
    cfg.capacity = 100_000;
    cfg.batch_max = 128 << 10;
    cfg
}

fn bytes_of(n: usize, salt: u8) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8 ^ salt).collect()
}

/// Appends on node `id` and runs the simulation until the answer is there.
fn put(sim: &mut Sim, id: u8, key: &str, data: &[u8]) -> Result<Acked, HotError> {
    let h = sim.append(id, key, data);
    sim.run();
    h.take().expect("the append did not finish")
}

/// Seals the open segment of `id` (it must have exactly one) by size and returns its id and
/// sealed length.
fn seal_open(sim: &Sim, id: u8) -> (BlockId, u64) {
    let node = sim.node(id);
    let open = node.open_segments();
    assert_eq!(open.len(), 1, "one open segment to seal");
    let segment = open[0].id;
    let ballot = Ballot {
        round: 2,
        node: NodeId(id),
    };
    let committed = sim
        .exec
        .block(async move { node.seal(segment, ballot, SealReason::Size).await })
        .expect("seal");
    (segment, committed.proposal.length)
}

fn mark_clean(sim: &Sim, id: u8, name: HotName) -> bool {
    let node = sim.node(id);
    sim.exec
        .block(async move { node.mark_clean(name).await })
        .expect("mark_clean")
}

fn throttled(e: &HotError) -> bool {
    matches!(
        e,
        HotError::Unavailable {
            reason: Unavailable::Throttled,
            ..
        }
    )
}

fn throttle_count(sim: &Sim, id: u8) -> u64 {
    sim.metrics(id).hot_put_throttled_total.with(()).get()
}

fn exists(sim: &Sim, id: u8, path: &str) -> bool {
    sim.disk(id).exists(path).unwrap()
}

/// A file that survives a crash: created, written, synced, its directory synced.
fn durable_file(disk: &FakeDisk, path: &str, bytes: &[u8]) {
    disk.create(path).unwrap();
    disk.write_at(path, 0, bytes).unwrap();
    disk.fdatasync(path).unwrap();
    disk.fsync_dir(tierstore_hot::dev::dir_of(path)).unwrap();
}

fn flip(disk: &FakeDisk, path: &str, offset: usize) {
    let byte = disk.read_at(path, offset, 1).unwrap()[0];
    disk.write_at(path, offset, &[byte ^ 0xff]).unwrap();
}

fn large_put(sim: &mut Sim, key: &str, data: &[u8]) -> LargePut {
    LargePut {
        key: key.as_bytes().to_vec(),
        size: Some(data.len() as u64),
        expiry: ExpiryClass(0),
        stamp: sim.stamp(),
        body: futures::stream::iter(
            data.chunks(40_000)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )
        .boxed(),
    }
}

fn put_large(sim: &mut Sim, id: u8, key: &str, data: &[u8]) -> Result<Acked, HotError> {
    let put = large_put(sim, key, data);
    let node = sim.node(id);
    let h = sim.exec.start(async move { node.put_file(put).await });
    sim.run();
    h.take().expect("the large put did not finish")
}

fn large_path(acked: &Acked) -> String {
    match acked.location {
        EntryLocation::Large(file) => HotName::Large(file).path(),
        other => panic!("not a large file: {other:?}"),
    }
}

fn live(node: &HotNode, key: &str) -> bool {
    matches!(node.index().get(key.as_bytes()), Ok(Lookup::Live(_)))
}

// ---------------------------------------------------------------------------------------------
// Backpressure marks.

#[test]
fn dirty_bytes_above_dirty_slow_answer_503_throttled_for_records_and_files_and_are_counted() {
    let mut sim = Sim::new(3, small_device());
    let mut accepted = 0;
    let refused = loop {
        let before = sim.node(1).fill();
        let key = sim.key_on(1, &format!("k{accepted}"));
        match put(&mut sim, 1, &key, &bytes_of(5_000, 1)) {
            Ok(_) => {
                assert!(
                    before.dirty <= 85_000,
                    "admitted above dirty_slow: {before:?}"
                );
                accepted += 1;
                assert!(accepted < 30, "never throttled");
            }
            Err(e) => break e,
        }
    };
    assert!(throttled(&refused), "{refused:?}");
    let fill = sim.node(1).fill();
    assert!(fill.dirty > 85_000, "throttled below dirty_slow: {fill:?}");
    assert!(
        fill.total <= 90_000,
        "the 503 is for fill below dirty_reject: {fill:?}"
    );
    assert_eq!(throttle_count(&sim, 1), 1);
    assert!(accepted > 10, "throttled long before the mark");
    // The refused PUT wrote nothing.
    assert_eq!(sim.node(1).open_segments()[0].records, accepted);
    // A record and a large file are both refused, and each refusal is counted.
    let key = sim.key_on(1, "again");
    assert!(throttled(&put(&mut sim, 1, &key, b"x").unwrap_err()));
    assert_eq!(throttle_count(&sim, 1), 2);
    let err = put_large(&mut sim, 1, "a-large-key", &bytes_of(1_000, 2)).unwrap_err();
    assert!(throttled(&err), "{err:?}");
    assert_eq!(throttle_count(&sim, 1), 3);
    assert_eq!(sim.node(1).fill(), fill, "refused PUTs changed the fill");
    match refused {
        HotError::Unavailable { retry_after_ms, .. } => assert_eq!(retry_after_ms, 1000),
        _ => unreachable!(),
    }
}

#[test]
fn total_fill_above_dirty_reject_with_nothing_clean_answers_507_not_503() {
    let mut sim = Sim::new(3, small_device());
    let first = sim.key_on(1, "first");
    put(&mut sim, 1, &first, &bytes_of(60_000, 1)).unwrap();
    // 60 000 dirty bytes are below dirty_slow, so this one is admitted and passes dirty_reject.
    let second = sim.key_on(1, "second");
    put(&mut sim, 1, &second, &bytes_of(40_000, 2)).unwrap();
    let fill = sim.node(1).fill();
    assert!(fill.total > 90_000 && fill.dirty == fill.total, "{fill:?}");
    let third = sim.key_on(1, "third");
    let err = put(&mut sim, 1, &third, b"x").unwrap_err();
    assert!(matches!(err, HotError::Full), "{err:?}");
    let err = put_large(&mut sim, 1, "large-third", &bytes_of(1_000, 3)).unwrap_err();
    assert!(matches!(err, HotError::Full), "{err:?}");
    assert_eq!(throttle_count(&sim, 1), 0, "507 is not a throttle");
    assert_eq!(sim.node(1).fill(), fill);
}

#[test]
fn a_full_device_with_clean_files_reclaims_them_first_and_the_put_passes() {
    let mut sim = Sim::new(3, small_device());
    let first = sim.key_on(1, "first");
    put(&mut sim, 1, &first, &bytes_of(60_000, 1)).unwrap();
    let second = sim.key_on(1, "second");
    put(&mut sim, 1, &second, &bytes_of(40_000, 2)).unwrap();
    let (segment, _) = seal_open(&sim, 1);
    let path = HotName::Segment(segment).path();
    let on_disk = sim.disk(1).len(&path).unwrap() as u64;
    // Destaged and committed: the bytes are clean, no longer dirty, and still on the disk.
    assert!(mark_clean(&sim, 1, HotName::Segment(segment)));
    let fill = sim.node(1).fill();
    assert_eq!((fill.dirty, fill.total), (0, on_disk));
    assert!(fill.total > 90_000);
    assert!(exists(&sim, 1, &path));
    let third = sim.key_on(1, "third");
    let acked = put(&mut sim, 1, &third, b"third").unwrap();
    assert_eq!(acked.copies, 3);
    assert!(
        !exists(&sim, 1, &path),
        "the clean segment was not reclaimed"
    );
    assert!(
        exists(&sim, 2, &path),
        "reclaim is local: a peer's copy stays"
    );
    let freed = sim
        .metrics(1)
        .hot_reclaim_bytes_total
        .with(ReclaimKind::CleanSegment)
        .get();
    assert_eq!(freed, on_disk);
    assert!(sim.node(1).fill().total < 1_000);
}

#[test]
fn mark_clean_keeps_the_file_by_default_and_unlinks_it_at_once_without_keep_clean() {
    for keep_clean in [true, false] {
        let mut cfg = config(3, 3);
        cfg.keep_clean = keep_clean;
        let mut sim = Sim::new(3, cfg);
        let key = sim.key_on(1, "k");
        put(&mut sim, 1, &key, &bytes_of(2_000, 1)).unwrap();
        let (segment, _) = seal_open(&sim, 1);
        let path = HotName::Segment(segment).path();
        let node = sim.node(1);
        assert_eq!(
            node.fill().dirty,
            node.fill().total,
            "a sealed segment is dirty until destaged"
        );
        let name = HotName::Segment(segment);
        assert!(mark_clean(&sim, 1, name));
        assert_eq!(
            exists(&sim, 1, &path),
            keep_clean,
            "keep_clean = {keep_clean}"
        );
        assert_eq!(node.fill().dirty, 0);
        assert_eq!(node.fill().total == 0, !keep_clean);
        assert!(exists(&sim, 2, &path), "a peer's copy is its own to mark");
    }
}

#[test]
fn mark_clean_of_a_file_the_node_does_not_hold_answers_false() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "k");
    put(&mut sim, 1, &key, b"v").unwrap();
    let open = sim.node(1).open_segments()[0].id;
    // An open segment is not sealed here yet, so it has no file to mark.
    assert!(!mark_clean(&sim, 1, HotName::Segment(open)));
    assert!(!mark_clean(&sim, 1, HotName::Cache { boot: 1, seq: 1 }));
    assert!(exists(&sim, 1, &HotName::Segment(open).path()));
}

#[test]
fn reclaim_drops_cache_segments_before_clean_ones_least_recently_read_first_and_never_a_dirty_byte()
{
    let mut sim = Sim::new(3, config(3, 3));
    // Three sealed segments, one per expiry class; `dirty` stays dirty.
    let mut sealed = Vec::new();
    for class in 0..3u8 {
        let key = sim.key_on(1, &format!("k{class}"));
        let mut rec = sim.record(&key, &bytes_of(3_000, class));
        rec.expiry = ExpiryClass(class);
        let node = sim.node(1);
        sim.exec
            .block(async move { node.append(rec).await })
            .unwrap();
        let (segment, _) = seal_open(&sim, 1);
        sealed.push(segment);
        sim.clock.advance(50);
    }
    let [a, b, dirty] = [sealed[0], sealed[1], sealed[2]];
    let node = sim.node(1);
    assert!(mark_clean(&sim, 1, HotName::Segment(a)));
    assert!(mark_clean(&sim, 1, HotName::Segment(b)));
    // Two cache segments with files on the disk.
    for seq in [1u64, 2] {
        durable_file(
            &sim.disk(1),
            &HotName::Cache { boot: 1, seq }.path(),
            &[0u8; 4096],
        );
        node.add_cache_segment(1, seq, 4096);
        sim.clock.advance(50);
    }
    // Reads move the read time: the cache segment 1 and the clean segment `a` were read last.
    for name in [HotName::Cache { boot: 1, seq: 1 }, HotName::Segment(a)] {
        sim.clock.advance(50);
        let n = node.clone();
        sim.exec
            .block(async move { n.serve(name, ByteRange { offset: 0, len: 16 }).await })
            .unwrap();
    }
    let cache = |seq| HotName::Cache { boot: 1, seq }.path();
    let order = [
        cache(2),
        cache(1),
        HotName::Segment(b).path(),
        HotName::Segment(a).path(),
    ];
    for expected in &order {
        let n = node.clone();
        let got = sim.exec.block(async move { n.reclaim(1).await }).unwrap();
        assert_eq!(
            &got.files,
            std::slice::from_ref(expected),
            "one file frees one byte's need"
        );
        assert!(!exists(&sim, 1, expected));
    }
    // Nothing clean is left: the dirty segment is never touched, however much is asked.
    let n = node.clone();
    let got = sim
        .exec
        .block(async move { n.reclaim(u64::MAX).await })
        .unwrap();
    assert!(
        got.files.is_empty() && got.cache_bytes == 0 && got.clean_bytes == 0,
        "{got:?}"
    );
    let dirty_path = HotName::Segment(dirty).path();
    assert!(exists(&sim, 1, &dirty_path));
    let key = sim.key_on(1, "k2");
    let n = node.clone();
    let (read_key, data) = sim
        .exec
        .block(async move { n.read_record(dirty, 0).await })
        .unwrap();
    assert_eq!(read_key, key.as_bytes());
    assert_eq!(&data[..], &bytes_of(3_000, 2)[..]);
}

#[test]
fn a_destaged_large_file_is_reclaimed_as_a_clean_large_file() {
    let mut sim = Sim::new(3, config(3, 3));
    let acked = put_large(&mut sim, 1, "large-key", &bytes_of(100_000, 5)).unwrap();
    let path = large_path(&acked);
    let EntryLocation::Large(file) = acked.location else {
        panic!("a large file");
    };
    let on_disk = sim.disk(1).len(&path).unwrap() as u64;
    let n = sim.node(1);
    let none = sim
        .exec
        .block(async move { n.reclaim(u64::MAX).await })
        .unwrap();
    assert!(
        none.files.is_empty(),
        "a large file is dirty until destaged"
    );
    assert!(mark_clean(&sim, 1, HotName::Large(file)));
    let n = sim.node(1);
    let got = sim.exec.block(async move { n.reclaim(1).await }).unwrap();
    assert_eq!((got.files, got.clean_bytes), (vec![path.clone()], on_disk));
    assert!(!exists(&sim, 1, &path));
    assert!(exists(&sim, 2, &path));
    let freed = sim
        .metrics(1)
        .hot_reclaim_bytes_total
        .with(ReclaimKind::CleanLargeFile)
        .get();
    assert_eq!(freed, on_disk);
}

#[test]
fn reclaim_counts_what_it_freed_by_kind_and_stops_when_enough_is_free() {
    let sim = Sim::new(3, config(3, 3));
    let node = sim.node(1);
    for seq in [1u64, 2, 3] {
        durable_file(
            &sim.disk(1),
            &HotName::Cache { boot: 1, seq }.path(),
            &[0u8; 4096],
        );
        node.add_cache_segment(1, seq, 4096);
    }
    let n = node.clone();
    let got = sim
        .exec
        .block(async move { n.reclaim(5_000).await })
        .unwrap();
    assert_eq!(got.files.len(), 2, "two segments cover 5 000 bytes");
    assert_eq!((got.cache_bytes, got.clean_bytes), (8192, 0));
    let freed = sim
        .metrics(1)
        .hot_reclaim_bytes_total
        .with(ReclaimKind::CacheSegment)
        .get();
    assert_eq!(freed, 8192);
    assert_eq!(node.fill().total, 4096);
}

// ---------------------------------------------------------------------------------------------
// Scrub and reads.

const WEEK_MS: u64 = 7 * 24 * 3600 * 1000;

/// Three nodes, two records put on node 1 and the segment sealed everywhere; its id, path and
/// the offset of the first record's data.
fn sealed_segment(sim: &mut Sim) -> (BlockId, String, String, u64) {
    let k1 = sim.key_on(1, "scrub");
    put(sim, 1, &k1, &bytes_of(3_000, 1)).unwrap();
    let k2 = sim.key_on(1, "scrub2");
    put(sim, 1, &k2, &bytes_of(2_000, 2)).unwrap();
    let (segment, length) = seal_open(sim, 1);
    (segment, HotName::Segment(segment).path(), k1, length)
}

fn scrub(sim: &Sim, id: u8) -> tierstore_hot::node::ScrubReport {
    let node = sim.node(id);
    sim.exec
        .block(async move { node.scrub().await })
        .expect("scrub")
}

#[test]
fn a_scrub_before_the_period_checks_nothing_and_at_the_period_checks_every_file() {
    let mut sim = Sim::new(3, config(3, 3));
    sealed_segment(&mut sim);
    sim.clock.advance(WEEK_MS - 1);
    assert_eq!(scrub(&sim, 1).checked, 0);
    sim.clock.advance(1);
    let report = scrub(&sim, 1);
    assert_eq!(
        (report.checked, report.bad, report.repaired, report.dropped),
        (1, 0, 0, 0)
    );
    // The pass moved the file's scrub time: it is not due again.
    assert_eq!(scrub(&sim, 1).checked, 0);
}

#[test]
fn a_flipped_byte_in_a_sealed_segment_is_found_and_replaced_by_a_peers_good_copy() {
    let mut sim = Sim::new(3, config(3, 3));
    let (segment, path, key, _) = sealed_segment(&mut sim);
    flip(&sim.disk(1), &path, RECORD_HEADER_LEN + key.len() + 10);
    assert_ne!(
        sim.disk(1).read_all(&path).unwrap(),
        sim.disk(2).read_all(&path).unwrap()
    );
    sim.clock.advance(WEEK_MS);
    let report = scrub(&sim, 1);
    assert_eq!(
        (report.checked, report.bad, report.repaired, report.dropped),
        (1, 1, 1, 0)
    );
    assert_eq!(
        sim.disk(1).read_all(&path).unwrap(),
        sim.disk(2).read_all(&path).unwrap()
    );
    assert!(
        !exists(&sim, 1, &format!("{path}.repair")),
        "the repair file was renamed over the copy"
    );
    assert_eq!(sim.metrics(1).hot_scrub_bad_total.with(()).get(), 1);
    // The record reads again.
    let node = sim.node(1);
    let (read_key, data) = sim
        .exec
        .block(async move { node.read_record(segment, 0).await })
        .unwrap();
    assert_eq!(read_key, key.as_bytes());
    assert_eq!(&data[..], &bytes_of(3_000, 1)[..]);
    // The good copy is clean for the next pass.
    sim.clock.advance(WEEK_MS);
    let again = scrub(&sim, 1);
    assert_eq!((again.checked, again.bad), (1, 0));
}

#[test]
fn a_flipped_byte_in_the_trailer_of_a_sealed_segment_is_found_too() {
    let mut sim = Sim::new(3, config(3, 3));
    let (_, path, _, _) = sealed_segment(&mut sim);
    let end = sim.disk(1).len(&path).unwrap();
    flip(&sim.disk(1), &path, end - 8);
    sim.clock.advance(WEEK_MS);
    let report = scrub(&sim, 1);
    assert_eq!((report.bad, report.repaired), (1, 1));
    assert_eq!(
        sim.disk(1).read_all(&path).unwrap(),
        sim.disk(3).read_all(&path).unwrap()
    );
}

#[test]
fn a_bad_peer_copy_is_skipped_and_the_next_peers_good_copy_repairs() {
    let mut sim = Sim::new(3, config(3, 3));
    let (_, path, key, _) = sealed_segment(&mut sim);
    let at = RECORD_HEADER_LEN + key.len() + 10;
    flip(&sim.disk(1), &path, at);
    flip(&sim.disk(2), &path, at + 1);
    sim.clock.advance(WEEK_MS);
    let report = scrub(&sim, 1);
    assert_eq!((report.bad, report.repaired), (1, 1));
    assert_eq!(
        sim.disk(1).read_all(&path).unwrap(),
        sim.disk(3).read_all(&path).unwrap()
    );
    assert_ne!(
        sim.disk(1).read_all(&path).unwrap(),
        sim.disk(2).read_all(&path).unwrap()
    );
}

#[test]
fn a_bad_copy_with_no_good_peer_copy_is_reported_and_left_in_place() {
    let mut sim = Sim::new(3, config(3, 3));
    let (_, path, key, _) = sealed_segment(&mut sim);
    let at = RECORD_HEADER_LEN + key.len() + 10;
    flip(&sim.disk(1), &path, at);
    sim.net.cut(1, 2);
    sim.net.cut(1, 3);
    sim.clock.advance(WEEK_MS);
    let bad = sim.disk(1).read_all(&path).unwrap();
    let report = scrub(&sim, 1);
    assert_eq!((report.checked, report.bad, report.repaired), (1, 1, 0));
    assert_eq!(
        sim.disk(1).read_all(&path).unwrap(),
        bad,
        "a bad copy was replaced by nothing"
    );
}

#[test]
fn a_flipped_byte_in_a_large_object_is_found_and_replaced_by_a_peers_good_copy() {
    let mut sim = Sim::new(3, config(3, 3));
    let data = bytes_of(100_000, 7);
    let acked = put_large(&mut sim, 1, "a-large-object", &data).unwrap();
    assert_eq!(acked.copies, 3);
    let path = large_path(&acked);
    flip(
        &sim.disk(1),
        &path,
        RECORD_HEADER_LEN + "a-large-object".len() + 50_000,
    );
    assert_ne!(
        sim.disk(1).read_all(&path).unwrap(),
        sim.disk(2).read_all(&path).unwrap()
    );
    sim.clock.advance(WEEK_MS);
    let report = scrub(&sim, 1);
    assert_eq!(
        (report.checked, report.bad, report.repaired, report.dropped),
        (1, 1, 1, 0)
    );
    assert_eq!(
        sim.disk(1).read_all(&path).unwrap(),
        sim.disk(2).read_all(&path).unwrap()
    );
    // The repaired copy hashes to the generation in its name.
    let whole = sim.disk(1).read_all(&path).unwrap();
    let mut hasher = GenerationHasher::new();
    hasher.update(&whole[RECORD_HEADER_LEN + "a-large-object".len()..]);
    assert_eq!(hasher.finish(), acked.generation);
}

#[test]
fn a_flipped_byte_in_the_header_of_a_large_object_is_found_and_repaired() {
    let mut sim = Sim::new(3, config(3, 3));
    let data = bytes_of(100_000, 9);
    let acked = put_large(&mut sim, 1, "a-large-header", &data).unwrap();
    let path = large_path(&acked);
    flip(&sim.disk(1), &path, 20);
    sim.clock.advance(WEEK_MS);
    let report = scrub(&sim, 1);
    assert_eq!(
        (report.checked, report.bad, report.repaired, report.dropped),
        (1, 1, 1, 0)
    );
    assert_eq!(
        sim.disk(1).read_all(&path).unwrap(),
        sim.disk(2).read_all(&path).unwrap()
    );
}

#[test]
fn a_bad_cache_segment_is_dropped_and_a_good_one_is_kept() {
    let mut sim = Sim::new(3, config(3, 3));
    let (_, path, key, length) = sealed_segment(&mut sim);
    // The records of a sealed segment, without its trailer, are a valid cache segment.
    let records = sim.disk(1).read_at(&path, 0, length as usize).unwrap();
    let node = sim.node(1);
    for seq in [1u64, 2] {
        durable_file(
            &sim.disk(1),
            &HotName::Cache { boot: 1, seq }.path(),
            &records,
        );
        node.add_cache_segment(1, seq, length);
    }
    let bad = HotName::Cache { boot: 1, seq: 2 }.path();
    flip(&sim.disk(1), &bad, RECORD_HEADER_LEN + key.len() + 5);
    sim.clock.advance(WEEK_MS);
    let report = scrub(&sim, 1);
    assert_eq!(
        (report.checked, report.bad, report.repaired, report.dropped),
        (3, 1, 0, 1)
    );
    assert!(!exists(&sim, 1, &bad));
    assert!(exists(&sim, 1, &HotName::Cache { boot: 1, seq: 1 }.path()));
    assert_eq!(sim.metrics(1).hot_scrub_bad_total.with(()).get(), 1);
}

#[test]
fn reading_a_flipped_record_answers_corrupt_and_a_missing_file_not_found() {
    let mut sim = Sim::new(3, config(3, 3));
    let (segment, path, key, _) = sealed_segment(&mut sim);
    let node = sim.node(1);
    let read = |offset: u64| {
        let n = node.clone();
        sim.exec
            .block(async move { n.read_record(segment, offset).await })
    };
    assert_eq!(&read(0).unwrap().1[..], &bytes_of(3_000, 1)[..]);
    // A flipped data byte fails the record's crc; a flipped magic fails its header.
    flip(&sim.disk(1), &path, RECORD_HEADER_LEN + key.len() + 3);
    assert!(matches!(read(0), Err(ReadError::Corrupt { offset: 0 })));
    flip(&sim.disk(1), &path, 0);
    assert!(matches!(read(0), Err(ReadError::Corrupt { offset: 0 })));
    // The second record is a different one and still reads.
    let second_key = sim.key_on(1, "scrub2");
    let Ok(Lookup::Live(entry)) = node.index().get(second_key.as_bytes()) else {
        panic!("the second record is indexed");
    };
    let EntryLocation::Block { offset: second, .. } = entry.location else {
        panic!("a record in a segment");
    };
    assert_eq!(&read(second).unwrap().1[..], &bytes_of(2_000, 2)[..]);
    // The same bytes served by `serve` are the flipped ones: serve does not verify.
    let n = node.clone();
    let got = sim
        .exec
        .block(async move {
            n.serve(HotName::Segment(segment), ByteRange { offset: 0, len: 4 })
                .await
        })
        .unwrap();
    assert_eq!(got.len(), 4);
    let missing = HotName::Segment(BlockId::make(NodeId(1), 999_999).unwrap());
    let n = node.clone();
    let err = sim
        .exec
        .block(async move { n.serve(missing, ByteRange { offset: 0, len: 4 }).await })
        .unwrap_err();
    assert!(matches!(err, ReadError::NotFound), "{err:?}");
    let n = node.clone();
    let err = sim
        .exec
        .block(async move {
            n.read_record(BlockId::make(NodeId(1), 999_999).unwrap(), 0)
                .await
        })
        .unwrap_err();
    assert!(matches!(err, ReadError::NotFound), "{err:?}");
}

// ---------------------------------------------------------------------------------------------
// Startup.

#[test]
fn startup_removes_uploads_temporary_seal_state_repair_files_and_cache_files_and_reports_them() {
    let mut sim = Sim::new(3, config(3, 3));
    let (_, seg_path, _, _) = sealed_segment(&mut sim);
    let acked = put_large(&mut sim, 1, "a-large-key", &bytes_of(100_000, 4)).unwrap();
    let large = large_path(&acked);
    let upload = HotName::LargeUpload {
        key_hash: KeyHash::of(b"half-written"),
        nonce: 7,
    }
    .path();
    let junk = [
        upload.clone(),
        "seals/9.seal.tmp".to_owned(),
        "seg/77.seg.repair".to_owned(),
        HotName::Cache { boot: 1, seq: 1 }.path(),
        HotName::Cache { boot: 2, seq: 5 }.path(),
    ];
    for path in &junk {
        durable_file(&sim.disk(1), path, &[1, 2, 3]);
    }
    sim.crash(1);
    let report = sim.restart(1);
    // The sweep goes directory by directory: big, seals, seg, blk, cache.
    assert_eq!(report.swept, junk, "swept files, in order");
    for path in &junk {
        assert!(!exists(&sim, 1, path), "{path} survived the sweep");
    }
    // What was acknowledged stays, and the index is rebuilt from it.
    assert!(exists(&sim, 1, &seg_path));
    assert!(exists(&sim, 1, &large));
    assert!(report.recovery.is_some());
    let node = sim.node(1);
    assert!(live(&node, &sim.key_on(1, "scrub")));
    assert!(live(&node, "a-large-key"));
    // A second start finds nothing left to sweep.
    sim.crash(1);
    assert!(sim.restart(1).swept.is_empty());
}

#[test]
fn a_restart_seals_the_open_segment_by_majority_and_indexes_what_was_acknowledged() {
    let mut sim = Sim::new(3, config(3, 3));
    let keys: Vec<String> = (0..3).map(|i| sim.key_on(1, &format!("open{i}"))).collect();
    for (i, k) in keys.iter().enumerate() {
        put(&mut sim, 1, k, &bytes_of(1_000 + i, 3)).unwrap();
    }
    let open = sim.node(1).open_segments()[0].id;
    sim.crash(1);
    let report = sim.restart(1);
    assert_eq!((report.sealed, report.unsealed), (1, 0));
    assert_eq!(
        report.recovery.as_ref().map(|r| r.mode),
        Some(Mode::Rebuild)
    );
    let node = sim.node(1);
    assert!(keys.iter().all(|k| live(&node, k)));
    assert!(node.started());
    assert!(
        node.open_segments().is_empty(),
        "the old segment is sealed, not reopened"
    );
    let n = node.clone();
    let (_, data) = sim
        .exec
        .block(async move { n.read_record(open, 0).await })
        .unwrap();
    assert_eq!(&data[..], &bytes_of(1_000, 3)[..]);
}

#[test]
fn recovery_after_flush_mark_replays_only_what_follows_the_mark_and_without_one_rebuilds() {
    let mut sim = Sim::new(3, config(3, 3));
    for tag in ["before1", "before2"] {
        let key = sim.key_on(1, tag);
        put(&mut sim, 1, &key, &bytes_of(1_500, 1)).unwrap();
    }
    let node = sim.node(1);
    let rebuilt = node.recover(None).unwrap();
    assert_eq!(
        (rebuilt.mode, rebuilt.facts),
        (Mode::Rebuild, 2),
        "no mark yet"
    );
    node.flush_mark().unwrap();
    let after = sim.key_on(1, "after");
    put(&mut sim, 1, &after, &bytes_of(1_500, 2)).unwrap();
    let replayed = node.recover(None).unwrap();
    assert_eq!(
        (replayed.mode, replayed.facts),
        (Mode::Replay, 1),
        "{replayed:?}"
    );
    assert_eq!(replayed.open_segments.len(), 1);
    assert!(
        ["before1", "before2", "after"]
            .iter()
            .all(|t| live(&node, &sim.key_on(1, t)))
    );
}

// ---------------------------------------------------------------------------------------------
// Create-only small appends.

fn tombstone(sim: &mut Sim, key: &str) -> Tombstone {
    Tombstone {
        key: key.as_bytes().to_vec(),
        generation: Generation::of(b""),
        stamp: sim.stamp(),
        names: Vec::new(),
    }
}

#[test]
fn the_same_key_and_bytes_answer_the_existing_ack_without_a_second_record() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "once");
    let first = put(&mut sim, 1, &key, b"the bytes").unwrap();
    let batches = sim.net.batches(1, 2).len();
    let again = put(&mut sim, 1, &key, b"the bytes").unwrap();
    assert_eq!(again.generation, first.generation);
    assert_eq!(again.location, first.location);
    assert_eq!(again.length, first.length);
    assert_eq!(again.copies, 2, "the copies the configuration promises");
    assert_eq!(sim.node(1).open_segments()[0].records, 1);
    assert_eq!(
        sim.net.batches(1, 2).len(),
        batches,
        "nothing was replicated for it"
    );
}

#[test]
fn the_same_key_with_other_bytes_answers_conflict_naming_the_existing_generation() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "once");
    let first = put(&mut sim, 1, &key, b"the bytes").unwrap();
    let err = put(&mut sim, 1, &key, b"other bytes").unwrap_err();
    match err {
        HotError::Conflict { existing } => assert_eq!(existing, first.generation),
        other => panic!("{other:?}"),
    }
    assert_eq!(sim.node(1).open_segments()[0].records, 1);
    let n = sim.node(1);
    let Ok(Lookup::Live(e)) = n.index().get(key.as_bytes()) else {
        panic!("the key stays live");
    };
    assert_eq!(e.generation, first.generation);
}

#[test]
fn a_key_with_a_newer_tombstone_answers_503_not_definitive_and_an_older_one_does_not_block() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "deleted");
    let old = sim.record(&key, b"old record");
    let tomb = tombstone(&mut sim, &key);
    sim.node(1).index().delete(&tomb).unwrap();
    let node = sim.node(1);
    let n = node.clone();
    let err = sim
        .exec
        .block(async move { n.append(old).await })
        .unwrap_err();
    assert!(
        matches!(
            err,
            HotError::Unavailable {
                reason: Unavailable::NotDefinitive,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        sim.node(1).open_segments().is_empty(),
        "nothing was written for it"
    );
    // A record stamped above the tombstone is a new PUT after the delete.
    let fresh = put(&mut sim, 1, &key, b"new record").unwrap();
    assert!(fresh.copies >= 2);
    assert!(live(&node, &key));
}

#[test]
fn an_index_that_is_not_ready_answers_503_not_definitive_until_it_is() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "early");
    sim.node(1).index().set_ready(false);
    let err = put(&mut sim, 1, &key, b"v").unwrap_err();
    assert!(
        matches!(
            err,
            HotError::Unavailable {
                reason: Unavailable::NotDefinitive,
                retry_after_ms: 1000
            }
        ),
        "{err:?}"
    );
    assert!(sim.node(1).open_segments().is_empty());
    sim.node(1).index().set_ready(true);
    put(&mut sim, 1, &key, b"v").unwrap();
}

#[test]
fn a_record_above_large_min_is_refused_and_one_at_large_min_is_stored() {
    let mut sim = Sim::new(3, config(3, 2));
    let large_min = sim.cfg.large_min as usize;
    let key = sim.key_on(1, "edge");
    let err = put(&mut sim, 1, &key, &bytes_of(large_min + 1, 1)).unwrap_err();
    match err {
        HotError::Io { op, source } => {
            assert_eq!(op, "append");
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        sim.node(1).open_segments().is_empty(),
        "nothing was written for it"
    );
    let acked = put(&mut sim, 1, &key, &bytes_of(large_min, 1)).unwrap();
    assert_eq!(acked.length, large_min as u64);
}
