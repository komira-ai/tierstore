//! The planner's constants (section 6 steps 3 and 4). Every value is configuration: one schema
//! serves both deployment profiles of section 21, and nothing here branches on which one runs.
//! The starting values are marked [I] (section 15); T0-F, T0-S and T10b replace them.

use tierstore_fetch_api::upstream::DeviceRef;

/// The kind of device a pack is read from, which decides its gap rule (section 6 step 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Medium {
    /// A hot copy: the NVMe of a storage node (LATER) or the SATA SSD partition of a combined node
    /// (NOW). A seek costs nothing, so no gap is read through.
    Hot,
    /// A cold data shard on a hard drive (SAS on LATER, SMR on NOW), or the legacy upstream, whose
    /// erasure set is on hard drives too.
    Cold,
}

impl Medium {
    /// The medium of the device a location names.
    #[must_use]
    pub const fn of(device: DeviceRef) -> Self {
        match device {
            DeviceRef::Hot(_) => Self::Hot,
            DeviceRef::Drive(..) | DeviceRef::Legacy => Self::Cold,
        }
    }
}

/// What reading through a gap costs on one kind of drive: one positioning against the bytes the
/// drive streams in that time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DriveModel {
    /// The time of one positioning (seek plus rotation), in microseconds.
    pub positioning_us: u32,
    /// The media rate, in MB/s (10^6 bytes per second).
    pub media_mb_per_s: u32,
}

impl DriveModel {
    /// `gap_cells = ceil(positioning x media rate / cell bytes)` for cells of `1 << cell_shift`
    /// bytes: the most absent cells a read goes through rather than paying another positioning
    /// (section 6 step 3). Microseconds times MB/s is bytes. A gap wider than `u32::MAX` cells is
    /// `u32::MAX`.
    #[must_use]
    pub fn gap_cells(self, cell_shift: u8) -> u32 {
        let bytes = u128::from(self.positioning_us) * u128::from(self.media_mb_per_s);
        let cell = 1u128
            .checked_shl(u32::from(cell_shift))
            .unwrap_or(u128::MAX);
        u32::try_from(bytes.div_ceil(cell)).unwrap_or(u32::MAX)
    }
}

/// One MiB.
const MIB: u64 = 1 << 20;

/// The planner's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlanConfig {
    /// The gap rule of a pack read from a hot copy.
    pub hot: DriveModel,
    /// The gap rule of a pack read from a cold drive, or whose device is not known yet.
    pub cold: DriveModel,
    /// `cov` (merged cells over the pack's cells) from which a pack is fetched whole, in percent.
    pub whole_cov_percent: u8,
    /// The largest pack, in bytes, fetched whole from the lower coverage below.
    pub small_pack_bytes: u64,
    /// `cov` from which a pack of at most `small_pack_bytes` is fetched whole, in percent.
    pub small_whole_cov_percent: u8,
    /// A would-be-whole pack is fetched as its runs while the node's queued backlog exceeds this
    /// many milliseconds of budget throughput.
    pub backlog_ms: u64,
}

impl PlanConfig {
    /// The gap rule of `medium` for cells of `1 << cell_shift` bytes.
    #[must_use]
    pub fn gap_cells(&self, medium: Medium, cell_shift: u8) -> u32 {
        match medium {
            Medium::Hot => self.hot.gap_cells(cell_shift),
            Medium::Cold => self.cold.gap_cells(cell_shift),
        }
    }
}

impl Default for PlanConfig {
    /// The starting constants of section 6: a hot device with no positioning cost [I], a cold
    /// hard drive of 12 ms and 190 MB/s [I] (3 cells of 1 MiB), whole from 50%, or from 20% up to
    /// 64 MiB, and a 2 s backlog.
    fn default() -> Self {
        Self {
            hot: DriveModel {
                positioning_us: 0,
                media_mb_per_s: 3_000,
            },
            cold: DriveModel {
                positioning_us: 12_000,
                media_mb_per_s: 190,
            },
            whole_cov_percent: 50,
            small_pack_bytes: 64 * MIB,
            small_whole_cov_percent: 20,
            backlog_ms: 2_000,
        }
    }
}
