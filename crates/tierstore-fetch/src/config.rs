//! The engine's constants (4.4 to 4.6, 4.8). Every value is the design's starting value; T0-F and
//! T10b replace them with measured ones. The same values serve both deployment profiles of
//! section 21: what differs between NOW and LATER (two or three hot replicas, `4+2` or `4+4`)
//! reaches the engine as data, in the locations the upstream answers, never as configuration.

/// The engine's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// Packs up to this size take no ramp: the first-touch cells, then one GET for the rest (4.4).
    pub small_pack_max: u64,
    /// The first piece of a small pack covers at least this many cells (4.4: at least 2 cells).
    pub first_piece_min_cells: u32,
    /// ... and at most this many (4.4: at most 4 MiB).
    pub first_piece_max_cells: u32,
    /// The first piece of the ramp, in cells (4.4: 4 MiB, then doubling).
    pub ramp_first_cells: u32,
    /// A demand covering at most this many cells of a part that has not started is fetched as a
    /// demand sub-range of exactly those cells (4.5: 1 MiB, 4 MiB with alignment).
    pub sub_range_max_cells: u32,
    /// The active-pack window: at most this many packs partly filled per class (4.5). `demand`
    /// is not held by it.
    pub window_packs: usize,
    /// Attempts of one part before the engine asks `LOCATE` again (4.6: up to 3).
    pub max_attempts: u32,
    /// The floor of the stall timeout after the first byte (4.6: 500 ms).
    pub stall_floor_ms: u64,
    /// The stall timeout is this many times the baseline network time of 1 MiB (4.6: 8).
    pub stall_factor: u64,
    /// The floor of the pre-first-byte timeout (4.6: 2 s).
    pub first_byte_floor_ms: u64,
    /// The pre-first-byte timeout is this many times the first-byte p99 baseline (4.6: 8).
    pub first_byte_factor: u64,
    /// The floor of the hedge trigger (4.6: 30 ms).
    pub hedge_floor_ms: u64,
    /// The hedge trigger is this many times the first-byte p50 baseline (4.6: 2).
    pub hedge_factor: u64,
    /// The largest cold range a degraded read hedges (4.6: 4 MiB).
    pub degraded_hedge_max: u64,
    /// Hedges in flight at once on this node (4.6: one hedge permit per client).
    pub hedge_permits: usize,
    /// The window of "at most one stall retry per drive per window" (4.3: at least 2 s).
    pub stall_retry_window_ms: u64,
    /// A device that answered `503` is treated as in a decrease state for this long: no hedge goes
    /// to it and it is the last choice for a retry (4.3: one controller window).
    pub busy_window_ms: u64,
    /// The base of the jittered backoff between retries.
    pub backoff_base_ms: u64,
    /// The cap of the backoff between retries and between `LOCATE`s of a waiting fill.
    pub backoff_max_ms: u64,
    /// The first wait between `LOCATE`s of a fill whose miss is not definitive.
    pub locate_backoff_ms: u64,
    /// `store.unavailable_wait`: how long a fill whose miss is not definitive waits before it is
    /// lost (4.6: 10 minutes, longer than a routine reboot).
    pub unavailable_wait_ms: u64,
    /// How long a cell that failed verification twice stays `bad` (4.6: 60 s).
    pub bad_cell_ms: u64,
    /// How long a lost fill answers `lost` before the engine forgets it.
    pub lost_ttl_ms: u64,
    /// How often a waiter looks again at a cell the evictor owns.
    pub evicting_poll_ms: u64,
    /// Parts of one class waiting for a budget permit at once, so one busy drive does not hold
    /// the parts of a class bound for the others.
    pub acquire_slots: usize,
    /// The seed of the backoff jitter.
    pub jitter_seed: u64,
    /// The pool of 1 MiB cell buffers parts are received into, in bytes: the most memory the
    /// cells in flight hold at once, from their first byte until their publish returns (4.8:
    /// `ceiling + 32 MiB`, [`buffer_bytes_for`]). The pool has at least one buffer.
    pub buffer_bytes: u64,
    /// The dirty-byte cap: cells handed to the entry's publish and not yet returned hold at most
    /// this many bytes; a cell waits before its publish while the cap is reached (4.8:
    /// `min(256 MiB, 2 x node ceiling)`, [`dirty_cap_for`]). A cell is always let in when nothing
    /// is being published, so a cap below one cell publishes one cell at a time.
    pub dirty_cap_bytes: u64,
}

const MIB: u64 = 1 << 20;

/// The node ceiling the defaults are made for, in bytes: the client ceiling of the table of 4.3
/// (6 parts) at the steady part size of the largest packs (64 MiB, 4.4), so the defaults are a
/// dirty cap of 256 MiB and a pool of 416 MiB. The ceiling is the design's starting value (T0-S
/// replaces it with the measured knee); a host that knows its own ceiling sets the bounds with
/// [`EngineConfig::with_ceiling`]. The cap's formula is the one the farm's cache-device arm kept
/// (`docs/measurements/2026-11-t0.md`, arm B: writeback at about 130 MB/s, so 256 MiB is about
/// 2 s of writeback).
pub const DEFAULT_CEILING_BYTES: u64 = 6 * 64 * MIB;

/// The headroom of the cell buffer pool above the node ceiling (4.8: 32 MiB).
pub const BUFFER_HEADROOM_BYTES: u64 = 32 * MIB;

/// The largest dirty-byte cap, whatever the ceiling (4.8: 256 MiB).
pub const DIRTY_CAP_MAX_BYTES: u64 = 256 * MIB;

/// The cell buffer pool of a node ceiling of `ceiling` bytes: `ceiling + 32 MiB` (4.8).
#[must_use]
pub const fn buffer_bytes_for(ceiling: u64) -> u64 {
    ceiling.saturating_add(BUFFER_HEADROOM_BYTES)
}

/// The dirty-byte cap of a node ceiling of `ceiling` bytes: `min(256 MiB, 2 x ceiling)` (4.8).
#[must_use]
pub const fn dirty_cap_for(ceiling: u64) -> u64 {
    let twice = ceiling.saturating_mul(2);
    if twice < DIRTY_CAP_MAX_BYTES {
        twice
    } else {
        DIRTY_CAP_MAX_BYTES
    }
}

impl EngineConfig {
    /// This configuration with the cell buffer pool and the dirty-byte cap of a node ceiling of
    /// `ceiling` bytes (4.8).
    #[must_use]
    pub const fn with_ceiling(mut self, ceiling: u64) -> Self {
        self.buffer_bytes = buffer_bytes_for(ceiling);
        self.dirty_cap_bytes = dirty_cap_for(ceiling);
        self
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            small_pack_max: 32 * MIB,
            first_piece_min_cells: 2,
            first_piece_max_cells: 4,
            ramp_first_cells: 4,
            sub_range_max_cells: 4,
            window_packs: 3,
            max_attempts: 3,
            stall_floor_ms: 500,
            stall_factor: 8,
            first_byte_floor_ms: 2_000,
            first_byte_factor: 8,
            hedge_floor_ms: 30,
            hedge_factor: 2,
            degraded_hedge_max: 4 * MIB,
            hedge_permits: 1,
            stall_retry_window_ms: 2_000,
            busy_window_ms: 2_000,
            backoff_base_ms: 100,
            backoff_max_ms: 30_000,
            locate_backoff_ms: 1_000,
            unavailable_wait_ms: 600_000,
            bad_cell_ms: 60_000,
            lost_ttl_ms: 60_000,
            evicting_poll_ms: 50,
            acquire_slots: 8,
            jitter_seed: 0x9e37_79b9_7f4a_7c15,
            buffer_bytes: buffer_bytes_for(DEFAULT_CEILING_BYTES),
            dirty_cap_bytes: dirty_cap_for(DEFAULT_CEILING_BYTES),
        }
    }
}
