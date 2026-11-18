//! Per-cell verification failure and its failover (design 4.6, section 9): a cell whose bytes fail
//! the client's digest, or that the serving node reports as corrupt, is refetched once at demand
//! from the alternative source; a second failure marks only that cell `bad` for `bad_cell_ms`.
//!
//! Catches: a corrupt cell published; a refetch of more than the one cell, or from the same
//! replica when another exists; a cold block not falling back to a degraded read; a second
//! failure that is retried again, or that takes the other cells down with it; a bad cell that
//! never expires, or whose expiry does not allow a new fetch; no bitrot report to the serving node
//! (or one naming another source or range); a node-reported crc failure of a `digest=none` object
//! not treated as a verification failure; a `recheck` cell trusted when its bytes do not match,
//! refetched when they do, or trusted on a pack with no digests; a cell the list has no digest for
//! accepted.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::sync::Arc;

use support::{
    MIB, Pack, Rig, Script, TestList, TestUpstream, cold, degraded, drive, generation, hot,
    location, pattern, sha256,
};
use tierstore_fetch::TimerKind;
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::engine::{Engine, PackDigests, WaitOutcome};
use tierstore_fetch_api::entry::{EntrySpec, EntryStore};
use tierstore_fetch_api::ids::{ByteRange, JobId, PackKey};
use tierstore_fetch_api::upstream::{DeviceRef, Report, Source, UpstreamError};
use tierstore_format::ids::NodeId;

const JOB: JobId = JobId(1);
const H1: DeviceRef = DeviceRef::Hot(NodeId(1));
const H2: DeviceRef = DeviceRef::Hot(NodeId(2));
/// A byte inside cell 2.
const BAD_AT: u64 = 2 * MIB + 17;
const CELL_2: ByteRange = ByteRange {
    offset: 2 * MIB,
    len: MIB,
};

type Reported = (PackKey, Source, ByteRange, Report);

/// The reports the upstream got, once there are `n` of them (they are sent by tasks of their own).
async fn reports(up: &TestUpstream, n: usize) -> Vec<Reported> {
    loop {
        let notified = up.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let seen = up.reports();
        if seen.len() >= n {
            return seen;
        }
        notified.await;
    }
}

fn gets_of(rig: &Rig, pack: &Pack) -> Vec<(DeviceRef, u64, u64)> {
    rig.upstream
        .gets()
        .iter()
        .filter(|g| g.key == pack.key())
        .map(|g| (g.source.device, g.range.offset, g.range.len))
        .collect()
}

fn corrupt(pack: &Pack, device: DeviceRef) -> Script {
    Script::Fail(UpstreamError::Corrupt {
        key: pack.key(),
        device,
        offset: BAD_AT,
    })
}

/// A 4-cell pack with two hot replicas; the first replica serves cell 2 corrupt.
#[tokio::test]
async fn a_corrupt_cell_is_refetched_once_from_the_other_hot_replica() {
    let rig = Rig::default_rig();
    let pack = Pack::new("two-replicas", 4 * MIB, 1);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size), hot(2, size)]),
    );
    rig.upstream.script(H1, Script::Corrupt(BAD_AT));
    let (out, seen) = tokio::join!(
        rig.engine.wait(&pack.spec, pack.all(), JOB),
        reports(&rig.upstream, 1)
    );
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(
        gets_of(&rig, &pack),
        vec![(H1, 0, 4 * MIB), (H2, 2 * MIB, MIB)],
        "one part, then the refetch of exactly the corrupt cell from the other replica"
    );
    let stats = rig.engine.stats();
    assert_eq!(stats.cells_mismatched, 1);
    assert_eq!(stats.cells_bad, 0);
    assert_eq!(stats.gets(tierstore_fetch::GetKind::Retry), 1);
    assert_eq!(stats.bytes_published, size);
    assert_eq!(
        stats.bytes_wasted, MIB,
        "the corrupt cell is dropped, never written"
    );
    assert_eq!(
        seen,
        vec![(pack.key(), hot(1, size), CELL_2, Report::Bitrot)],
        "the serving node is told which source served which range"
    );
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// A cold block whose data shard serves a corrupt cell is refetched by a degraded read.
#[tokio::test]
async fn a_corrupt_cell_from_a_cold_source_is_refetched_by_a_degraded_read() {
    let rig = Rig::default_rig();
    let pack = Pack::new("cold-block", 4 * MIB, 2);
    let size = pack.spec.size;
    let reconstruct = degraded(9, &[drive(1, 1), drive(2, 2)], 0, size);
    rig.upstream.add(
        &pack,
        location(
            Some(generation(1)),
            size,
            vec![cold(1, 1, 0, size), reconstruct.clone()],
        ),
    );
    rig.upstream.script(drive(1, 1), Script::Corrupt(BAD_AT));
    let (out, seen) = tokio::join!(
        rig.engine.wait(&pack.spec, pack.all(), JOB),
        reports(&rig.upstream, 1)
    );
    assert_eq!(out, WaitOutcome::Ready);
    let gets = rig.upstream.gets();
    assert_eq!(gets.len(), 2);
    assert_eq!(gets[0].source, cold(1, 1, 0, size));
    assert_eq!(gets[1].source, reconstruct);
    assert_eq!(gets[1].range, CELL_2);
    assert_eq!(rig.engine.stats().cells_mismatched, 1);
    assert_eq!(seen[0].1, cold(1, 1, 0, size));
    assert_eq!(seen[0].2, CELL_2);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// A pack with one copy that serves cell 2 corrupt twice; returns after the cell is `bad` and the
/// others are published.
async fn single_copy_corrupt_twice() -> (Rig, Pack, WaitOutcome) {
    let rig = Rig::default_rig();
    let pack = Pack::new("single-copy", 4 * MIB, 3);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size)]),
    );
    rig.upstream.script(H1, Script::Corrupt(BAD_AT));
    rig.upstream.script(H1, Script::Corrupt(BAD_AT));
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    rig.until(|s| s.cells_published == 3).await;
    (rig, pack, out)
}

/// With no other copy a second corruption marks only that cell `bad`: the wait covering it gets
/// EIO of that cell, the other cells are present and readable.
#[tokio::test]
async fn a_second_corruption_with_no_alternative_marks_only_that_cell_bad() {
    let (rig, pack, out) = single_copy_corrupt_twice().await;
    assert_eq!(out, WaitOutcome::Eio { cell: 2 });
    assert_eq!(
        rig.cells(&pack),
        vec![
            CellState::Present,
            CellState::Present,
            CellState::Bad,
            CellState::Present
        ]
    );
    let stats = rig.engine.stats();
    assert_eq!(stats.cells_mismatched, 2);
    assert_eq!(stats.cells_bad, 1);
    assert_eq!(
        gets_of(&rig, &pack),
        vec![(H1, 0, 4 * MIB), (H1, 2 * MIB, MIB)],
        "the cell is fetched twice and not again"
    );
    let before = rig.upstream.gets().len();
    let first = rig
        .engine
        .wait(&pack.spec, pack.range(0, 2 * MIB), JOB)
        .await;
    let last = rig
        .engine
        .wait(&pack.spec, pack.range(3 * MIB, MIB), JOB)
        .await;
    let bad = rig
        .engine
        .wait(&pack.spec, pack.range(2 * MIB, MIB), JOB)
        .await;
    assert_eq!(first, WaitOutcome::Ready);
    assert_eq!(last, WaitOutcome::Ready);
    assert_eq!(bad, WaitOutcome::Eio { cell: 2 });
    assert_eq!(rig.upstream.gets().len(), before, "no read starts a GET");
    assert_eq!(
        rig.entry_bytes(&pack, 0, 2 * MIB),
        pack.bytes[..(2 * MIB) as usize].to_vec()
    );
}

/// Both replicas serve the cell corrupt: each is told, and the cell becomes `bad`.
#[tokio::test]
async fn a_cell_corrupt_on_both_replicas_is_reported_against_each_and_marked_bad() {
    let rig = Rig::default_rig();
    let pack = Pack::new("both-corrupt", 4 * MIB, 4);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size), hot(2, size)]),
    );
    rig.upstream.script(H1, Script::Corrupt(BAD_AT));
    rig.upstream.script(H2, Script::Corrupt(BAD_AT));
    let (out, seen) = tokio::join!(
        rig.engine.wait(&pack.spec, pack.all(), JOB),
        reports(&rig.upstream, 2)
    );
    assert_eq!(out, WaitOutcome::Eio { cell: 2 });
    let mut named: Vec<(DeviceRef, ByteRange)> = seen.iter().map(|r| (r.1.device, r.2)).collect();
    named.sort_unstable();
    assert_eq!(named, vec![(H1, CELL_2), (H2, CELL_2)]);
    let stats = rig.engine.stats();
    assert_eq!(stats.cells_mismatched, 2);
    assert_eq!(stats.cells_bad, 1);
    assert_eq!(rig.cells(&pack)[2], CellState::Bad);
}

/// After `bad_cell_ms` the cell is absent again and a new wait fetches it.
#[tokio::test]
async fn a_bad_cell_is_absent_again_after_the_bad_cell_time_and_a_new_wait_refetches_it() {
    let (rig, pack, _) = single_copy_corrupt_twice().await;
    assert_eq!(rig.cells(&pack)[2], CellState::Bad);
    rig.until(|s| s.armed(TimerKind::BadCell) == 1).await;
    rig.clock.advance(60_000);
    rig.until(|s| s.armed(TimerKind::BadCell) == 0).await;
    assert_eq!(rig.cells(&pack)[2], CellState::Absent);
    let out = rig
        .engine
        .wait(&pack.spec, pack.range(2 * MIB, MIB), JOB)
        .await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(
        gets_of(&rig, &pack).last().copied(),
        Some((H1, 2 * MIB, MIB)),
        "the one cell is fetched again"
    );
    assert_eq!(
        rig.entry_bytes(&pack, 0, pack.spec.size),
        pack.bytes.to_vec()
    );
    assert_eq!(rig.engine.stats().cells_bad, 1);
}

/// A `digest=none` object: the serving node's crc failure is a verification failure, the cell is
/// told to the node and the part is retried on the other replica.
#[tokio::test]
async fn a_crc_failure_reported_by_the_node_for_a_digest_none_object_goes_to_the_other_replica() {
    let rig = Rig::default_rig();
    let pack = Pack::etag("etag-two-replicas", 4 * MIB, 5, generation(7));
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(7)), size, vec![hot(1, size), hot(2, size)]),
    );
    rig.upstream.script(H1, corrupt(&pack, H1));
    let (out, seen) = tokio::join!(
        rig.engine.wait(&pack.spec, pack.all(), JOB),
        reports(&rig.upstream, 1)
    );
    assert_eq!(out, WaitOutcome::Ready);
    let gets = gets_of(&rig, &pack);
    assert_eq!(gets.len(), 2);
    assert_eq!(gets[0], (H1, 0, 4 * MIB));
    assert_eq!(gets[1].0, H2, "the retry goes to the other replica");
    assert!(
        gets[1].1 <= 2 * MIB && gets[1].1 + gets[1].2 >= 3 * MIB,
        "the retry covers the corrupt cell"
    );
    assert_eq!(rig.engine.stats().cells_mismatched, 1);
    assert_eq!(
        seen,
        vec![(pack.key(), hot(1, size), CELL_2, Report::Bitrot)]
    );
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// A `digest=none` object on a cold block: the data shard's drive reports a crc failure and the
/// cell comes from a degraded read.
#[tokio::test]
async fn a_crc_failure_on_a_cold_digest_none_object_is_served_by_degraded_read() {
    let rig = Rig::default_rig();
    let pack = Pack::etag("etag-cold", 4 * MIB, 6, generation(7));
    let size = pack.spec.size;
    let reconstruct = degraded(9, &[drive(1, 1), drive(2, 2)], 0, size);
    rig.upstream.add(
        &pack,
        location(
            Some(generation(7)),
            size,
            vec![cold(1, 1, 0, size), reconstruct.clone()],
        ),
    );
    rig.upstream
        .script(drive(1, 1), corrupt(&pack, drive(1, 1)));
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    let gets = rig.upstream.gets();
    assert_eq!(gets.len(), 2);
    assert_eq!(gets[1].source, reconstruct);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// A `digest=none` object with one copy whose node reports the crc failure twice: that cell is
/// `bad`, the other cells are fetched and present.
#[tokio::test]
async fn a_digest_none_cell_the_node_reports_corrupt_twice_is_bad_and_the_others_are_present() {
    let rig = Rig::default_rig();
    let pack = Pack::etag("etag-single", 4 * MIB, 7, generation(7));
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(7)), size, vec![hot(1, size)]),
    );
    rig.upstream.script(H1, corrupt(&pack, H1));
    rig.upstream.script(H1, corrupt(&pack, H1));
    let out = rig.engine.wait(&pack.spec, pack.all(), JOB).await;
    assert_eq!(out, WaitOutcome::Eio { cell: 2 });
    rig.until(|s| s.cells_published == 3).await;
    assert_eq!(
        rig.cells(&pack),
        vec![
            CellState::Present,
            CellState::Present,
            CellState::Bad,
            CellState::Present
        ]
    );
    let stats = rig.engine.stats();
    assert_eq!(stats.cells_mismatched, 2);
    assert_eq!(stats.cells_bad, 1);
}

/// Opens the entry of `pack` and restores its cell 0 as `recheck` with `bytes`.
fn restore_recheck(rig: &Rig, pack: &Pack, bytes: &[u8]) {
    rig.store
        .open(&EntrySpec {
            pack: pack.spec.id.clone(),
            size: pack.spec.size,
        })
        .unwrap();
    rig.store
        .mem_entry(&pack.spec.id)
        .unwrap()
        .restore_as_recheck(0, bytes)
        .unwrap();
}

/// A `recheck` cell whose bytes match the list's digest becomes present with no GET.
#[tokio::test]
async fn a_recheck_cell_with_matching_bytes_becomes_present_without_a_get() {
    let rig = Rig::default_rig();
    let pack = Pack::new("recheck-match", 2 * MIB, 8);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size)]),
    );
    restore_recheck(&rig, &pack, &pack.bytes[..MIB as usize]);
    let out = rig.engine.wait(&pack.spec, pack.range(0, MIB), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(
        rig.cells(&pack),
        vec![CellState::Present, CellState::Absent]
    );
    assert!(rig.upstream.gets().is_empty());
    assert_eq!(
        rig.entry_bytes(&pack, 0, MIB),
        pack.bytes[..MIB as usize].to_vec()
    );
}

/// A `recheck` cell whose bytes do not match becomes absent and is fetched.
#[tokio::test]
async fn a_recheck_cell_with_wrong_bytes_is_fetched_again() {
    let rig = Rig::default_rig();
    let pack = Pack::new("recheck-wrong", 2 * MIB, 9);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size)]),
    );
    restore_recheck(&rig, &pack, &pattern(MIB, 200));
    let out = rig.engine.wait(&pack.spec, pack.range(0, MIB), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(gets_of(&rig, &pack), vec![(H1, 0, MIB)]);
    assert_eq!(
        rig.entry_bytes(&pack, 0, MIB),
        pack.bytes[..MIB as usize].to_vec()
    );
    assert_eq!(rig.engine.stats().cells_published, 1);
}

/// A `digest=none` pack has nothing to re-hash against: a `recheck` cell is fetched again even
/// when its bytes are right.
#[tokio::test]
async fn a_recheck_cell_of_a_digest_none_pack_is_fetched_again() {
    let rig = Rig::default_rig();
    let pack = Pack::etag("etag-recheck", 2 * MIB, 10, generation(7));
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(7)), size, vec![hot(1, size)]),
    );
    restore_recheck(&rig, &pack, &pack.bytes[..MIB as usize]);
    let out = rig.engine.wait(&pack.spec, pack.range(0, MIB), JOB).await;
    assert_eq!(out, WaitOutcome::Ready);
    assert_eq!(gets_of(&rig, &pack), vec![(H1, 0, MIB)]);
    assert_eq!(
        rig.entry_bytes(&pack, 0, MIB),
        pack.bytes[..MIB as usize].to_vec()
    );
}

/// A list that lacks a cell's digest cannot verify that cell: it fails verification (twice, so it
/// is `bad`) while the cells the list covers are accepted.
#[tokio::test]
async fn a_cell_the_list_has_no_digest_for_fails_verification() {
    let rig = Rig::default_rig();
    let mut pack = Pack::new("short-list", 4 * MIB, 11);
    let size = pack.spec.size;
    pack.spec.digests = Some(PackDigests {
        list: Arc::new(TestList {
            key: pack.key(),
            size,
            digests: pack
                .bytes
                .chunks(MIB as usize)
                .take(2)
                .map(sha256)
                .collect(),
        }),
        pack: 0,
    });
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size)]),
    );
    let (two, three) = tokio::join!(
        rig.engine.wait(&pack.spec, pack.range(2 * MIB, MIB), JOB),
        rig.engine.wait(&pack.spec, pack.range(3 * MIB, MIB), JOB)
    );
    assert_eq!(two, WaitOutcome::Eio { cell: 2 });
    assert_eq!(three, WaitOutcome::Eio { cell: 3 });
    let cells = rig.cells(&pack);
    assert_eq!(cells[2..], [CellState::Bad, CellState::Bad]);
    let stats = rig.engine.stats();
    assert_eq!(stats.cells_mismatched, 4);
    assert_eq!(stats.cells_bad, 2);
    assert_eq!(stats.cells_published, 0);
}
