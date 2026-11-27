//! What the engine counts (the counters section 12 names for the fetch path, T9 registers them)
//! and the progress events tests wait on instead of time.

use crate::gate::Level;

/// The kind of a GET (4.2: GETs are counted by kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GetKind {
    /// A part of the plan: a ramp piece, a grid part, or a run of one.
    GridPart,
    /// A demand sub-range of 1 to `sub_range_max_cells` cells.
    DemandSub,
    /// A retry after a failure, or the refetch of a cell that failed verification.
    Retry,
    /// A hedge of a demand part.
    Hedge,
}

/// A timer of the engine, for [`Stats::armed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimerKind {
    /// The pre-first-byte timeout of a GET.
    FirstByte,
    /// The stall timeout after the first byte.
    Stall,
    /// The hedge trigger of a demand part.
    Hedge,
    /// The backoff before a retry.
    Backoff,
    /// The wait between `LOCATE`s of a fill whose miss is not definitive.
    Locate,
    /// The expiry of a `bad` cell.
    BadCell,
    /// How long a lost fill is remembered.
    LostTtl,
    /// A waiter looking again at a cell the evictor owns.
    EvictPoll,
}

const TIMER_KINDS: usize = 8;

impl TimerKind {
    fn index(self) -> usize {
        match self {
            Self::FirstByte => 0,
            Self::Stall => 1,
            Self::Hedge => 2,
            Self::Backoff => 3,
            Self::Locate => 4,
            Self::BadCell => 5,
            Self::LostTtl => 6,
            Self::EvictPoll => 7,
        }
    }
}

/// The engine's counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// GETs issued, by kind: grid parts, demand sub-ranges, retries, hedges.
    pub gets: [u64; 4],
    /// `LOCATE`s asked.
    pub locates: u64,
    /// Fills started (one per pack while it is being filled: single flight).
    pub fills: u64,
    /// Bytes of part bodies received.
    pub bytes_fetched: u64,
    /// Bytes of cells published.
    pub bytes_published: u64,
    /// Bytes received and not published: a hedge's or a retry's loser, a cell that failed
    /// verification, a part cut short.
    pub bytes_wasted: u64,
    /// Cells published.
    pub cells_published: u64,
    /// Cells whose bytes failed verification (the client's digest or the server's crc).
    pub cells_mismatched: u64,
    /// Cells marked `bad` (a second failure).
    pub cells_bad: u64,
    /// Copies excluded from a fill because they hold another generation
    /// (`store_copy_generation_mismatch_total`).
    pub copy_generation_mismatch: u64,
    /// Fills lost (`store_pack_lost_total`).
    pub packs_lost: u64,
    /// Fills that failed with EIO for their waiters and stay resumable.
    pub fills_failed: u64,
    /// Retries after a stall.
    pub stall_retries: u64,
    /// Timers armed now, by [`TimerKind`].
    pub timers: [u32; TIMER_KINDS],
    /// Bytes of cells handed to the entry's publish whose publish has not returned (4.8,
    /// `dirty_bytes`).
    pub dirty_bytes: u64,
    /// The most of [`Stats::dirty_bytes`] at once.
    pub dirty_bytes_peak: u64,
    /// Bytes of cells let past the dirty-byte cap to their publish, in all (a cell that was never
    /// published, a hedge's loser among them, is not).
    pub dirty_bytes_admitted: u64,
    /// Cells waiting now before their publish for the dirty-byte cap.
    pub dirty_waiting: u64,
    /// Cells that waited before their publish for the dirty-byte cap, in all (each is a sample of
    /// `writeback_stall_seconds`).
    pub dirty_stalls: u64,
    /// Cell buffers of the pool in use now (4.8).
    pub buffers: u64,
    /// The most cell buffers in use at once.
    pub buffers_peak: u64,
    /// Attempts waiting now for a cell buffer.
    pub buffers_waiting: u64,
}

impl Stats {
    /// GETs of `kind`.
    #[must_use]
    pub fn gets(&self, kind: GetKind) -> u64 {
        self.gets[kind_index(kind)]
    }

    /// Every GET.
    #[must_use]
    pub fn total_gets(&self) -> u64 {
        self.gets.iter().sum()
    }

    /// Timers of `kind` armed now.
    #[must_use]
    pub fn armed(&self, kind: TimerKind) -> u32 {
        self.timers[kind.index()]
    }

    /// Takes in the levels of the cell buffer pool and the dirty-byte cap.
    pub(crate) fn memory(&mut self, pool: &Level, dirty: &Level) {
        self.dirty_bytes = dirty.used;
        self.dirty_bytes_peak = dirty.peak;
        self.dirty_bytes_admitted = dirty.admitted;
        self.dirty_waiting = dirty.waiting;
        self.dirty_stalls = dirty.waits;
        self.buffers = pool.used;
        self.buffers_peak = pool.peak;
        self.buffers_waiting = pool.waiting;
    }

    pub(crate) fn count_get(&mut self, kind: GetKind) {
        self.gets[kind_index(kind)] += 1;
    }

    pub(crate) fn arm(&mut self, kind: TimerKind) {
        self.timers[kind.index()] += 1;
    }

    pub(crate) fn disarm(&mut self, kind: TimerKind) {
        self.timers[kind.index()] -= 1;
    }
}

fn kind_index(kind: GetKind) -> usize {
    match kind {
        GetKind::GridPart => 0,
        GetKind::DemandSub => 1,
        GetKind::Retry => 2,
        GetKind::Hedge => 3,
    }
}
