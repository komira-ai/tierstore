//! How many hash threads a node runs (4.7): `ceil(1.2 x target_rate / core_sha_rate)`, with
//! `target_rate` the cache write ceiling and `core_sha_rate` measured at startup.

use std::num::NonZeroUsize;

use tierstore_fetch_api::clock::Clock;

/// The cells the pool holds at once, queued or being hashed (4.7: "queue bound 32 cells"). A
/// `verify` that finds the pool full waits: that wait is the receive pause.
pub const QUEUE_CELLS: usize = 32;

/// The most of [`QUEUE_CELLS`] that `recheck` cells may hold [D]. Half the bound: a recheck
/// sweep alone never fills the pool, so it never pauses a fill's receive, and it still keeps
/// every thread busy when no fill is waiting.
pub const RECHECK_CELLS: usize = QUEUE_CELLS / 2;

/// A rate in bytes per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BytesPerSec(pub u64);

/// The cache write ceiling the pool keeps up with when a node's configuration names none: about
/// 500 MB/s (4.7) [I until T0 B measures the cache device].
pub const DEFAULT_TARGET_RATE: BytesPerSec = BytesPerSec(500_000_000);

/// The size of the buffer [`measure_core_sha_rate`] hashes: one cell.
const PROBE_BYTES: usize = 1 << 20;

/// [`measure_core_sha_rate`] hashes until this much of its clock has passed.
const PROBE_MIN_MS: u64 = 100;

/// ...or until it has hashed this many cells (256 MiB), whichever comes first: about 0.9 s on a
/// core without SHA-NI, so the bound binds only on a core above 2.5 GB/s or a clock that does not
/// move.
const PROBE_MAX_CELLS: u64 = 256;

/// The number of hash threads for a cache write ceiling of `target` on a core that hashes `core`
/// (4.7): `ceil(1.2 x target / core)`, at least 1 and at most `cpus` and [`QUEUE_CELLS`] (a
/// thread beyond either has nothing to run on or nothing to hash). A `core` of zero counts as one
/// byte per second.
///
/// Today's build nodes (no SHA-NI, about 0.29 GB/s per core) at the default 500 MB/s get 3
/// threads; a core with SHA-NI (about 1.5 GB/s) gets 1.
#[must_use]
pub fn hash_threads(target: BytesPerSec, core: BytesPerSec, cpus: NonZeroUsize) -> NonZeroUsize {
    let core = u128::from(core.0.max(1));
    // 1.2 x target / core, rounded up, in integers: (6 x target) / (5 x core).
    let wanted = (6 * u128::from(target.0)).div_ceil(5 * core);
    let cap = cpus.get().min(QUEUE_CELLS);
    let n = usize::try_from(wanted).unwrap_or(usize::MAX).clamp(1, cap);
    NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN)
}

/// `core_sha_rate` (4.7): the sha256 rate of the calling thread's core, measured by hashing one
/// 1 MiB cell with `ring` until `clock` has moved 100 ms or 256 MiB is hashed. A clock that has not
/// moved at all counts as 1 ms.
#[must_use]
pub fn measure_core_sha_rate(clock: &dyn Clock) -> BytesPerSec {
    let cell = vec![0u8; PROBE_BYTES];
    let start = clock.now_ms();
    let mut cells: u64 = 0;
    let mut elapsed = 0;
    while cells < PROBE_MAX_CELLS && elapsed < PROBE_MIN_MS {
        let digest = ring::digest::digest(&ring::digest::SHA256, &cell);
        std::hint::black_box(digest);
        cells += 1;
        elapsed = clock.now_ms().saturating_sub(start);
    }
    let bytes = u128::from(cells) * PROBE_BYTES as u128;
    let rate = bytes * 1000 / u128::from(elapsed.max(1));
    BytesPerSec(u64::try_from(rate).unwrap_or(u64::MAX))
}
