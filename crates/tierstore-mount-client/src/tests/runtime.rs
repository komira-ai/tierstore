//! Separate runtimes (5.4; section 14: "All reader threads blocked in a fake `pread` while `wait`
//! completions still flow").
//!
//! Proves: with every reader thread blocked in a fake `pread` (a lock the test holds), the
//! client's runtime still takes the server's `waited` replies and completes the readers' tickets,
//! and serves a new request of another thread from start to answer; when the readers are
//! unblocked, each finds its answer already there, with no wait.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::{ByteRange, JobId};

use crate::tests::common::{MIB, PACKS, ServerSide, pack_id, spec, until};

#[test]
fn wait_completions_flow_while_every_reader_thread_is_blocked() {
    const READERS: u64 = 4;
    let side = ServerSide::new("runtime-blocked-readers");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(0)).wait().unwrap();
    side.engine.park(pack_id(0));

    // The fake `pread`: a read lock the test holds the write side of.
    let disk = Arc::new(RwLock::new(()));
    let held = disk.write().unwrap();
    let blocked = Arc::new(AtomicUsize::new(0));
    let readers: Vec<_> = (0..READERS)
        .map(|i| {
            let (pack, disk, blocked) = (pack.clone(), disk.clone(), blocked.clone());
            std::thread::spawn(move || {
                // One window each, so each read sends its own wait.
                let range = ByteRange {
                    offset: 8 * i * MIB,
                    len: 1,
                };
                let mut ticket = pack.wait(range, PACKS[0].1, JobId(i));
                blocked.fetch_add(1, Ordering::SeqCst);
                // Another read of this thread is served from the page cache... and blocks.
                let _pread = disk.read().unwrap();
                ticket.try_take()
            })
        })
        .collect();
    until(|| blocked.load(Ordering::SeqCst) == READERS as usize);
    side.parked(0, READERS as usize);

    // Every reader is blocked; the fills complete, and the client takes every reply.
    assert_eq!(
        side.engine.complete(&pack_id(0), WaitOutcome::Ready),
        READERS as usize
    );
    let stats = client.wait_for(|s| s.waits_answered == READERS);
    assert_eq!(stats.waits_sent, READERS);
    // A request of another thread is served from start to answer meanwhile.
    let other = pack.wait(
        ByteRange {
            offset: 39 * MIB,
            len: 1,
        },
        PACKS[0].1,
        JobId(9),
    );
    assert_eq!(other.wait(), Ok(WaitOutcome::Ready));
    assert!(readers.iter().all(|r| !r.is_finished()));

    drop(held);
    for r in readers {
        assert_eq!(r.join().unwrap(), Some(Ok(WaitOutcome::Ready)));
    }
}
