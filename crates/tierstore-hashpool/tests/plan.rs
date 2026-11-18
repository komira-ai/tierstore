//! The number of hash threads (4.7) and the startup measurement of a core's sha256 rate, on a
//! scripted clock.
//!
//! Catches: a thread count rounded down (too few threads for the cache write ceiling) or off by
//! one at an exact quotient; zero threads; more threads than CPUs or than the queue's cells; an
//! overflow at extreme rates; a division by a zero rate or a zero elapsed time; a measurement that
//! stops before its time or never stops on a clock that does not move.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use tierstore_fetch_api::clock::Clock;
use tierstore_hashpool::{
    BytesPerSec, DEFAULT_TARGET_RATE, QUEUE_CELLS, RECHECK_CELLS, hash_threads,
    measure_core_sha_rate,
};

fn threads(target: u64, core: u64, cpus: usize) -> usize {
    hash_threads(
        BytesPerSec(target),
        BytesPerSec(core),
        NonZeroUsize::new(cpus).unwrap(),
    )
    .get()
}

#[test]
fn the_thread_count_is_ceil_of_1_2_target_over_core_rate() {
    // Today's build nodes: no SHA-NI, about 0.29 GB/s per core with ring (4.7): 3 threads.
    assert_eq!(threads(500_000_000, 290_000_000, 88), 3);
    // A core with SHA-NI, about 1.5 GB/s: 1 thread.
    assert_eq!(threads(500_000_000, 1_500_000_000, 88), 1);
    // The sha2 crate's rate on today's nodes would need 4.
    assert_eq!(threads(500_000_000, 150_000_000, 88), 4);
    // An exact quotient is not rounded up; one byte per second less is.
    assert_eq!(threads(500_000_000, 600_000_000, 88), 1);
    assert_eq!(threads(500_000_000, 599_999_999, 88), 2);
    assert_eq!(threads(1_000_000_000, 400_000_000, 88), 3);
    assert_eq!(threads(1_000_000_000, 399_999_999, 88), 4);
    assert_eq!(DEFAULT_TARGET_RATE, BytesPerSec(500_000_000));
}

#[test]
fn the_thread_count_is_at_least_one_and_at_most_the_cpus_and_the_queue() {
    assert_eq!(threads(0, 290_000_000, 88), 1, "no target still hashes");
    assert_eq!(
        threads(500_000_000, 290_000_000, 2),
        2,
        "capped at the CPUs"
    );
    assert_eq!(
        threads(500_000_000, 0, 88),
        QUEUE_CELLS,
        "a zero rate is capped, not divided by"
    );
    assert_eq!(threads(u64::MAX, 1, 1000), QUEUE_CELLS);
    assert_eq!(threads(u64::MAX, u64::MAX, 88), 2);
    assert_eq!((QUEUE_CELLS, RECHECK_CELLS), (32, 16));
}

/// A clock that reads the scripted times in order, then its last one forever.
struct Scripted(Mutex<VecDeque<u64>>);

impl Scripted {
    fn new(times: &[u64]) -> Self {
        Self(Mutex::new(times.iter().copied().collect()))
    }

    fn left(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

impl Clock for Scripted {
    fn now_ms(&self) -> u64 {
        let mut q = self.0.lock().unwrap();
        if q.len() > 1 {
            q.pop_front().unwrap()
        } else {
            q[0]
        }
    }
}

#[test]
fn the_core_rate_is_the_bytes_hashed_over_the_time_they_took() {
    // Start at 1000, then 1050 after one cell and 1100 after the second: 2 MiB in 100 ms.
    let clock = Scripted::new(&[1000, 1050, 1100, 5000]);
    assert_eq!(
        measure_core_sha_rate(&clock),
        BytesPerSec(2 * (1 << 20) * 10)
    );
    assert_eq!(
        clock.left(),
        1,
        "it stops reading the clock once 100 ms have passed"
    );
}

#[test]
fn the_measurement_stops_at_256_cells_on_a_clock_that_does_not_move() {
    let clock = Scripted::new(&[7]);
    // 256 MiB in "0 ms", counted as 1 ms.
    assert_eq!(
        measure_core_sha_rate(&clock),
        BytesPerSec(256 * (1 << 20) * 1000)
    );
}

#[test]
fn a_clock_one_tick_short_of_100_ms_keeps_the_measurement_going() {
    let clock = Scripted::new(&[0, 99, 99, 100]);
    assert_eq!(
        measure_core_sha_rate(&clock),
        BytesPerSec(3 * (1 << 20) * 10)
    );
}
