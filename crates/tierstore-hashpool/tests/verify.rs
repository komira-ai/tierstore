//! Per-cell verification (3.3) on a pool with real threads: each cell of a pack is judged alone
//! against its own digest, computed here with an independent sha256 (the `sha2` crate), in any
//! order and with any subset of the pack's cells present.
//!
//! Catches: a corrupt cell passing, or a good cell failing next to a corrupt one; a flipped byte at
//! a cell's first or last offset missed; the short last cell or an empty cell judged against the
//! wrong length; a cell that waits for other cells of its pack; recheck cells judged differently
//! from fills; more than 32 cells held under a load of hundreds; a pool that does not stop.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::num::NonZeroUsize;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::join_all;
use sha2::{Digest, Sha256};
use tierstore_fetch_api::clock::{ManualClock, MonotonicClock};
use tierstore_fetch_api::list::CellDigest;
use tierstore_fetch_api::verify::{CellJob, Verdict, Verifier, VerifyPriority};
use tierstore_hashpool::{ByPriority, DEFAULT_TARGET_RATE, HashPool, QUEUE_CELLS};

const CELL: usize = 1 << 20;

fn pool(threads: usize) -> HashPool {
    HashPool::start(
        NonZeroUsize::new(threads).unwrap(),
        Arc::new(ManualClock::new()),
    )
    .unwrap()
}

fn sha256(bytes: &[u8]) -> CellDigest {
    Sha256::digest(bytes).into()
}

/// A pack of `len` bytes that differ from cell to cell, and its cell table: `digest[i] =
/// sha256(bytes [i << 20, min((i+1) << 20, len)))` (3.1).
fn pack(len: usize) -> (Vec<u8>, Vec<CellDigest>) {
    let bytes: Vec<u8> = (0..len).map(|i| ((i % 251) ^ (i / CELL)) as u8).collect();
    let digests = bytes.chunks(CELL).map(sha256).collect();
    (bytes, digests)
}

fn cell_job(
    bytes: &[u8],
    digests: &[CellDigest],
    cell: usize,
    priority: VerifyPriority,
) -> CellJob {
    let end = ((cell + 1) * CELL).min(bytes.len());
    CellJob {
        cell: u32::try_from(cell).unwrap(),
        expected: digests[cell],
        bytes: Bytes::copy_from_slice(&bytes[cell * CELL..end]),
        priority,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_corrupt_cells_of_a_pack_fail_whatever_the_order_they_arrive_in() {
    // 8 whole cells and a short last one.
    let (good, digests) = pack(8 * CELL + 12_345);
    let mut bytes = good.clone();
    bytes[3 * CELL] ^= 0x80; // the first byte of cell 3
    bytes[6 * CELL - 1] ^= 0x01; // the last byte of cell 5
    let last = bytes.len() - 1;
    bytes[last] ^= 0x10; // the last byte of the short cell 8

    let pool = pool(3);
    let cells: Vec<usize> = (0..digests.len()).rev().collect();
    let verdicts = join_all(cells.iter().map(|&c| {
        let job = cell_job(&bytes, &digests, c, VerifyPriority::Fill);
        let pool = &pool;
        async move { (c, pool.verify(job).await.0) }
    }))
    .await;
    let failed: Vec<usize> = verdicts
        .iter()
        .filter(|(_, v)| *v == Verdict::Mismatch)
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(failed, [8, 5, 3]);
    let s = pool.stats();
    assert_eq!(
        s.verified,
        ByPriority {
            fill: 9,
            recheck: 0
        }
    );
    assert_eq!(
        s.mismatched,
        ByPriority {
            fill: 3,
            recheck: 0
        }
    );
    assert_eq!((s.held, s.queued, s.busy, s.waiting), (0, 0, 0, 0));

    // The same cells uncorrupted all pass.
    for c in [3, 5, 8] {
        let (v, _) = pool
            .verify(cell_job(&good, &digests, c, VerifyPriority::Fill))
            .await;
        assert_eq!(v, Verdict::Match, "cell {c}");
    }
}

#[tokio::test]
async fn the_last_cell_of_a_large_span_is_verified_with_no_other_cell_present() {
    // A 1 GiB span would be 1024 cells; only the last one arrives.
    let len = 1024 * CELL;
    let last: Vec<u8> = (0..CELL).map(|i| (i % 253) as u8).collect();
    let expected = sha256(&last);
    let pool = pool(1);
    let (v, back) = pool
        .verify(CellJob {
            cell: u32::try_from(len / CELL - 1).unwrap(),
            expected,
            bytes: Bytes::from(last.clone()),
            priority: VerifyPriority::Fill,
        })
        .await;
    assert_eq!(v, Verdict::Match);
    assert_eq!(back, last);
    assert_eq!(pool.stats().verified.fill, 1);
}

#[tokio::test]
async fn a_cell_is_judged_on_its_exact_bytes_not_a_padded_cell() {
    let pool = pool(1);
    let short = b"short last cell".to_vec();
    let mut padded = short.clone();
    padded.resize(CELL, 0);
    for (bytes, expected, want) in [
        (short.clone(), sha256(&short), Verdict::Match),
        (short.clone(), sha256(&padded), Verdict::Mismatch),
        (Vec::new(), sha256(b""), Verdict::Match),
        (Vec::new(), sha256(&[0]), Verdict::Mismatch),
    ] {
        let len = bytes.len();
        let (v, _) = pool
            .verify(CellJob {
                cell: 0,
                expected,
                bytes: Bytes::from(bytes),
                priority: VerifyPriority::Fill,
            })
            .await;
        assert_eq!(v, want, "{len} bytes");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recheck_cells_are_judged_like_fills_and_counted_apart() {
    let (bytes, digests) = pack(2 * CELL);
    let mut bad = digests.clone();
    bad[1][0] ^= 1;
    let pool = pool(2);
    let fill = pool.verify(cell_job(&bytes, &digests, 0, VerifyPriority::Fill));
    let recheck_ok = pool.verify(cell_job(&bytes, &digests, 0, VerifyPriority::Recheck));
    let recheck_bad = pool.verify(cell_job(&bytes, &bad, 1, VerifyPriority::Recheck));
    let (a, b, c) = futures::join!(fill, recheck_ok, recheck_bad);
    assert_eq!(
        (a.0, b.0, c.0),
        (Verdict::Match, Verdict::Match, Verdict::Mismatch)
    );
    let s = pool.stats();
    assert_eq!(
        s.verified,
        ByPriority {
            fill: 1,
            recheck: 2
        }
    );
    assert_eq!(
        s.mismatched,
        ByPriority {
            fill: 0,
            recheck: 1
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hundreds_of_cells_at_once_never_hold_more_than_32() {
    let (bytes, digests) = pack(4 * CELL);
    let pool = pool(3);
    let n = 300;
    let verdicts = join_all((0..n).map(|i| {
        let priority = if i % 3 == 0 {
            VerifyPriority::Recheck
        } else {
            VerifyPriority::Fill
        };
        pool.verify(cell_job(&bytes, &digests, i % 4, priority))
    }))
    .await;
    assert!(verdicts.iter().all(|(v, _)| *v == Verdict::Match));
    let s = pool.stats();
    assert_eq!(
        s.verified,
        ByPriority {
            fill: 200,
            recheck: 100
        }
    );
    // How many calls paused depends on how fast the threads hash; the bound does not.
    assert!(s.high_water <= QUEUE_CELLS, "{s:?}");
    assert_eq!((s.held, s.waiting, s.queued, s.busy), (0, 0, 0, 0));
}

#[test]
fn a_pool_runs_the_threads_it_was_asked_for_and_stops_them_when_dropped() {
    let pool = pool(3);
    assert_eq!(pool.threads(), 3);
    let text = format!("{pool:?}");
    assert!(
        text.starts_with("HashPool { threads: 3, stats: PoolStats {"),
        "{text}"
    );
    drop(pool); // joins its threads; a thread that never stopped would hang here
}

#[test]
fn a_measured_pool_runs_between_one_thread_and_one_per_cpu() {
    let pool =
        HashPool::start_measured(DEFAULT_TARGET_RATE, Arc::new(MonotonicClock::new())).unwrap();
    let cpus = std::thread::available_parallelism().unwrap().get();
    assert!(
        (1..=cpus.min(QUEUE_CELLS)).contains(&pool.threads()),
        "{}",
        pool.threads()
    );
}
