//! Wait coalescing (5.4; section 14: "Wait coalescing: at most one outstanding `wait` per 8
//! cells"), against the real server and the fake engine, which logs every `wait` it receives.
//!
//! Proves: concurrent reads of one 8-cell window send one `wait`, which covers the rest of the
//! first read's span up to the window's end, and every read is answered by it; a read across
//! windows sends one `wait` per window and a second read over the same windows sends none; a read
//! whose cells are outside the outstanding `wait` is queued and sent only after it ends, never
//! beside it; a queued read whose cells are present by then is answered with no request; a read
//! whose cells are present, an empty read and a read past the end send nothing.

use std::sync::Arc;

use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::{ByteRange, JobId};

use crate::tests::common::{MIB, PACKS, ServerSide, pack_id, run, spec, stats_until};

fn bytes(offset: u64, len: u64) -> ByteRange {
    ByteRange { offset, len }
}

/// The ranges of every `wait` the engine received for pack `i`, sorted.
fn waits(side: &ServerSide, i: usize) -> Vec<ByteRange> {
    let mut w: Vec<ByteRange> = side
        .engine
        .waits()
        .into_iter()
        .filter(|(p, _, _)| *p == pack_id(i))
        .map(|(_, r, _)| r)
        .collect();
    w.sort();
    w
}

#[test]
fn concurrent_reads_of_one_window_share_one_wait() {
    let side = ServerSide::new("coalesce-one-window");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(0)).wait().unwrap();
    side.engine.park(pack_id(0));

    // The first read is of cell 0 in a file that runs to the pack's end: its wait covers the
    // whole window.
    let first = pack.wait(bytes(0, 10), PACKS[0].1, JobId(1));
    side.parked(0, 1);
    let readers: Vec<_> = (1..8u64)
        .map(|c| {
            let pack = Arc::clone(&pack);
            std::thread::spawn(move || {
                pack.wait(bytes(c * MIB + 5, 100), (c + 1) * MIB, JobId(c + 1))
                    .wait()
            })
        })
        .collect();
    let stats = stats_until(&client, |s| s.wait_parts_joined == 7);
    assert_eq!(stats.waits_sent, 1);
    assert_eq!(stats.windows_outstanding, 1);
    assert_eq!(waits(&side, 0), vec![bytes(0, 8 * MIB)]);
    // The wait carries the job of the read that started it.
    assert_eq!(side.engine.waits()[0].2, JobId(1));

    assert_eq!(side.engine.complete(&pack_id(0), WaitOutcome::Ready), 1);
    assert_eq!(first.wait(), Ok(WaitOutcome::Ready));
    for r in readers {
        assert_eq!(r.join().unwrap(), Ok(WaitOutcome::Ready));
    }
    let stats = stats_until(&client, |s| s.windows_outstanding == 0);
    assert_eq!(stats.waits_sent, 1);
    assert_eq!(stats.waits_answered, 1);
}

#[test]
fn a_read_across_windows_waits_once_per_window_and_a_second_read_joins_them() {
    let side = ServerSide::new("coalesce-windows");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(0)).wait().unwrap();
    side.engine.park(pack_id(0));

    // Cells 6 to 17: windows 0, 1 and 2; the span runs to the pack's end.
    let a = pack.wait(bytes(6 * MIB, 12 * MIB), PACKS[0].1, JobId(1));
    side.parked(0, 3);
    assert_eq!(
        waits(&side, 0),
        vec![
            bytes(6 * MIB, 2 * MIB),
            bytes(8 * MIB, 8 * MIB),
            bytes(16 * MIB, 8 * MIB),
        ]
    );
    // Cells 7 to 16 are inside the three outstanding waits.
    let b = pack.wait(bytes(7 * MIB, 10 * MIB), 17 * MIB, JobId(2));
    let stats = stats_until(&client, |s| s.wait_parts_joined == 3);
    assert_eq!(stats.waits_sent, 3);
    assert_eq!(stats.windows_outstanding, 3);

    assert_eq!(side.engine.complete(&pack_id(0), WaitOutcome::Ready), 3);
    assert_eq!(a.wait(), Ok(WaitOutcome::Ready));
    assert_eq!(b.wait(), Ok(WaitOutcome::Ready));
    assert_eq!(waits(&side, 0).len(), 3);
}

#[test]
fn a_read_outside_the_outstanding_wait_is_sent_only_after_it_ends() {
    let side = ServerSide::new("coalesce-queued");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(0)).wait().unwrap();
    side.engine.park(pack_id(0));

    // A file of cell 5 alone: its wait covers cell 5.
    let a = pack.wait(bytes(5 * MIB, 10), 6 * MIB, JobId(1));
    side.parked(0, 1);
    // Cell 2 is in window 0 but not in the outstanding wait: queued, not sent.
    let mut b = pack.wait(bytes(2 * MIB, 10), 3 * MIB, JobId(2));
    let stats = stats_until(&client, |s| s.wait_parts_queued == 1);
    assert_eq!(stats.waits_sent, 1);
    assert_eq!(waits(&side, 0), vec![bytes(5 * MIB, MIB)]);
    assert_eq!(b.try_take(), None);

    // The first wait ends; then, and only then, the queued one is sent (the engine now answers
    // every wait of the pack at once).
    side.engine.complete(&pack_id(0), WaitOutcome::Ready);
    assert_eq!(a.wait(), Ok(WaitOutcome::Ready));
    assert_eq!(b.wait(), Ok(WaitOutcome::Ready));
    assert_eq!(
        waits(&side, 0),
        vec![bytes(2 * MIB, MIB), bytes(5 * MIB, MIB)]
    );
    let stats = stats_until(&client, |s| s.windows_outstanding == 0);
    assert_eq!(stats.waits_sent, 2);
}

#[test]
fn a_queued_read_whose_cells_became_present_is_answered_with_no_request() {
    let side = ServerSide::new("coalesce-queued-present");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(0)).wait().unwrap();
    side.engine.park(pack_id(0));

    let a = pack.wait(bytes(5 * MIB, 10), 6 * MIB, JobId(1));
    side.parked(0, 1);
    let b = pack.wait(bytes(3 * MIB, 10), 4 * MIB, JobId(2));
    stats_until(&client, |s| s.wait_parts_queued == 1);
    // Cell 3 is fetched by another reader's fill meanwhile.
    side.publish(0, run(3, 1));

    side.engine.complete(&pack_id(0), WaitOutcome::Ready);
    assert_eq!(a.wait(), Ok(WaitOutcome::Ready));
    assert_eq!(b.wait(), Ok(WaitOutcome::Ready));
    let stats = stats_until(&client, |s| s.windows_outstanding == 0);
    assert_eq!(stats.waits_sent, 1);
    assert_eq!(stats.waits_ready_local, 1);
    assert_eq!(waits(&side, 0), vec![bytes(5 * MIB, MIB)]);
}

#[test]
fn reads_of_present_cells_empty_reads_and_reads_past_the_end_send_nothing() {
    let side = ServerSide::new("coalesce-local");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(1)).wait().unwrap();
    side.publish(1, run(0, 4));

    // Every cell of the pack, the short last one included.
    assert_eq!(
        pack.wait(bytes(0, PACKS[1].1), PACKS[1].1, JobId(1)).wait(),
        Ok(WaitOutcome::Ready)
    );
    assert_eq!(
        pack.wait(bytes(10, 0), PACKS[1].1, JobId(1)).wait(),
        Ok(WaitOutcome::Ready)
    );
    assert_eq!(
        pack.wait(bytes(PACKS[1].1, 10), PACKS[1].1, JobId(1))
            .wait(),
        Ok(WaitOutcome::Ready)
    );
    assert_eq!(
        pack.wait(bytes(u64::MAX - 1, 10), u64::MAX, JobId(1))
            .wait(),
        Ok(WaitOutcome::Ready)
    );
    let stats = client.stats();
    assert_eq!(stats.waits_ready_local, 1);
    assert_eq!(stats.waits_sent, 0);
    assert!(side.engine.waits().is_empty());
}
