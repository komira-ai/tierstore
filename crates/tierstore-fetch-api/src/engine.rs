//! `Engine`: the fetch engine of a build node (section 4). One engine serves demand reads, mount
//! Prepare and S3-front misses of large objects, on Linux and macOS. T3 implements it.

use std::sync::Arc;

use crate::ids::{ByteRange, CellRun, Class, JobId, PackId, Rank};
use crate::list::ListL2;

/// The digests of a pack's cells: where in which list.
#[derive(Clone)]
pub struct PackDigests {
    /// The mapped list.
    pub list: Arc<dyn ListL2>,
    /// The pack's index in it.
    pub pack: u32,
}

impl std::fmt::Debug for PackDigests {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackDigests")
            .field("pack", &self.pack)
            .finish_non_exhaustive()
    }
}

/// What the engine needs to fill a pack.
#[derive(Debug, Clone)]
pub struct PackSpec {
    /// The pack.
    pub id: PackId,
    /// Its size.
    pub size: u64,
    /// Its cell digests; `None` for a `digest=none` object (section 9).
    pub digests: Option<PackDigests>,
    /// The `upload_part_size` of the list (0 in the storage tier; 4.4).
    pub upload_part_size: u64,
}

/// What a `wait` ends with (5.4: `ok | eio(cell) | lost`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaitOutcome {
    /// Every cell of the range is present.
    Ready,
    /// The cell is `bad`, or failed on every copy: EIO for reads covering it.
    Eio {
        /// The first failed cell.
        cell: u32,
    },
    /// The fill is lost (4.6): EIO, and the tree's mount is marked lost.
    Lost,
}

/// The fetch engine.
///
/// Section 14 rows: "N readers on spans of one pack: one `PackFill`"; "Demand sub-range plus
/// overlapping grid part"; "Reverse-order part completion"; "A 1 GiB file span"; "Hedge and
/// original both deliver a cell"; "Ramp and grid"; "Stall before the first byte is not retried
/// below the pre-first-byte timeout"; "Two jobs both at rank 0 alternate by DRR"; "Corrupt bytes in
/// one cell"; "A copy answers 412"; "Pinned generation"; "Hedge only for `demand`"; "Pack lost
/// mid-job"; "Definitive miss ... one `LOCATE` per key per fill"; "S3-front object".
#[async_trait::async_trait]
pub trait Engine: Send + Sync {
    /// Asks for `runs` of the pack at `class` and `rank` for `job`: attaches to the pack's fill
    /// (single flight) and raises priority; never starts a second fill (4.1). Returns at once.
    fn ensure(&self, pack: &PackSpec, runs: &[CellRun], class: Class, job: JobId, rank: Rank);

    /// Waits until every cell covering `range` of the pack is present, or one fails, or the fill
    /// is lost. A `wait` on cells nobody ensured is a `demand` of them.
    async fn wait(&self, pack: &PackSpec, range: ByteRange, job: JobId) -> WaitOutcome;
}
