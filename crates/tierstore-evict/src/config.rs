//! [`EvictConfig`]: the node cache's budget and the constants of section 7 and 5.1.

use tierstore_fetch_api::page::CELL_SHIFT;

/// The bytes of one cell (1 MiB): the unit the cache charges (section 7: use is cells x cell
/// size, so the short last cell of a pack is charged as a whole cell).
pub const CELL_BYTES: u64 = 1 << CELL_SHIFT;

/// The budget of the node cache and the constants eviction and admission read.
///
/// Only the budget differs between nodes: on the NOW profile of section 21 it is what the
/// combined node gives the cache on its SATA boot SSD, on the LATER profile what a build node
/// gives it on its local SSD. The other fields are the constants of the design and the same on
/// both profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictConfig {
    /// The cache's byte budget.
    pub budget_bytes: u64,
    /// The share of the budget, in thousandths, that admitted jobs may hold (900: section 7's
    /// `0.9 x budget`).
    pub admit_permille: u64,
    /// How long the evictor waits for a group's readers to leave before it puts the group's
    /// cells back `present` (`evict_wait`, 2 s; 5.1 step 3), in milliseconds of the injected
    /// clock.
    pub evict_wait_ms: u64,
    /// How long after process start nothing is evicted, so the mounts rebind their holds (60 s;
    /// section 7), in milliseconds of the injected clock.
    pub grace_ms: u64,
}

impl EvictConfig {
    /// The share of the budget admitted jobs may hold (section 7).
    pub const ADMIT_PERMILLE: u64 = 900;
    /// `evict_wait` (5.1).
    pub const EVICT_WAIT_MS: u64 = 2_000;
    /// The grace window (section 7).
    pub const GRACE_MS: u64 = 60_000;

    /// A budget of `budget_bytes` with the design's constants.
    #[must_use]
    pub const fn new(budget_bytes: u64) -> Self {
        Self {
            budget_bytes,
            admit_permille: Self::ADMIT_PERMILLE,
            evict_wait_ms: Self::EVICT_WAIT_MS,
            grace_ms: Self::GRACE_MS,
        }
    }

    /// The bytes admitted jobs may hold: `budget x admit_permille / 1000`, rounded down.
    #[must_use]
    pub fn admit_limit(&self) -> u64 {
        let limit = u128::from(self.budget_bytes) * u128::from(self.admit_permille) / 1000;
        u64::try_from(limit).unwrap_or(u64::MAX)
    }
}
