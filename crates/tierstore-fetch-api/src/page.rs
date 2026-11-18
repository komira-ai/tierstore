//! The state page and the counter regions (5.1).
//!
//! **State page.** `open-pack` returns a state memfd beside the pack's data fd. The server creates
//! it, maps it read-write, writes the header, the cell bytes and the group touch epochs, then
//! applies [`MOUNT_SEALS`] before passing it, so a mount can map it `PROT_READ` only: a mount can
//! never mark a cell `present` or alter another mount's data. Layout (little-endian):
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | magic `TSSP` |
//! | 4 | 2 | version (1) |
//! | 6 | 1 | cell shift (20) |
//! | 7 | 1 | cells per group (16) |
//! | 8 | 4 | cell count |
//! | 12 | 4 | group count |
//! | 16 | 8 | pack size |
//! | 24 | 8 | offset of the cell bytes (64) |
//! | 32 | 8 | offset of the touch epochs |
//! | 40 | 8 | length of the page |
//! | 48 | 16 | reserved, zero |
//! | 64 | cell count | one [`crate::cell::CellByte`] per cell |
//! | epochs offset | 4 x group count | `u32` touch epoch per 16-cell group, written by the server |
//!
//! A touch epoch is the seconds of the injected clock when the group was last touched, `now_ms /
//! 1000` truncated to a `u32` ([`epoch_of`]); an untouched group's epoch is 0. The evictor
//! reads the epochs to find the groups untouched longest (7).
//!
//! The epochs start at the next multiple of 8 after the cell bytes; the page length is a multiple
//! of 4096.
//!
//! **Counter regions.** Each mount creates one counter memfd for its whole life and sends it at
//! handshake; inside it the mount allocates a region per open pack, one `u16` reader counter per
//! 16-cell group, and names the region's offset in `open-pack`. A region starts at a multiple of
//! 8. A region offset beyond the server's current mapping makes the server remap that holder's
//! memfd before it acknowledges the `open-pack`.

use crate::ids::CellRun;

/// The magic of a state page.
pub const STATE_PAGE_MAGIC: u32 = u32::from_le_bytes(*b"TSSP");

/// The version of the state page this release writes and reads.
pub const STATE_PAGE_VERSION: u16 = 1;

/// The length of the state page header; the cell bytes follow it.
pub const STATE_PAGE_HEADER_LEN: u64 = 64;

/// The cells per reader-counter group (5.1).
pub const GROUP_CELLS: u32 = 16;

/// The client's cell: 1 MiB (section 2).
pub const CELL_SHIFT: u8 = 20;

/// The page size the state page's length is a multiple of.
pub const PAGE_BYTES: u64 = 4096;

/// `F_SEAL_SEAL` (Linux uapi).
pub const F_SEAL_SEAL: u32 = 0x0001;
/// `F_SEAL_SHRINK`.
pub const F_SEAL_SHRINK: u32 = 0x0002;
/// `F_SEAL_GROW`.
pub const F_SEAL_GROW: u32 = 0x0004;
/// `F_SEAL_FUTURE_WRITE`: no new writable mapping and no `write(2)`; the server's existing
/// writable mapping stays valid.
pub const F_SEAL_FUTURE_WRITE: u32 = 0x0010;

/// The seals the server applies to a state memfd before it passes it; a mount refuses a state
/// memfd whose `F_GET_SEALS` lacks any of them.
pub const MOUNT_SEALS: u32 = F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_FUTURE_WRITE;

/// Whether a state memfd sealed with `seals` (as `F_GET_SEALS` reports them) is read-only for
/// every mount.
#[must_use]
pub const fn sealed_for_mounts(seals: u32) -> bool {
    seals & MOUNT_SEALS == MOUNT_SEALS
}

/// The touch epoch of the instant `now_ms` (milliseconds of the injected clock): its seconds,
/// truncated to a `u32`.
#[must_use]
pub const fn epoch_of(now_ms: u64) -> u32 {
    // Truncation is the definition: the epoch wraps every 2^32 seconds.
    (now_ms / 1000) as u32
}

/// The 16-cell groups that cover `cells`.
#[must_use]
pub fn groups_of(cells: CellRun) -> Option<CellRun> {
    if cells.count == 0 {
        return None;
    }
    let last = cells.first.checked_add(cells.count - 1)?;
    let first = cells.first / GROUP_CELLS;
    Some(CellRun {
        first,
        count: last / GROUP_CELLS - first + 1,
    })
}

/// The layout of one pack's state page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatePageLayout {
    /// The pack's size in bytes.
    pub pack_size: u64,
    /// `ceil(pack_size / 1 MiB)`.
    pub cell_count: u32,
}

impl StatePageLayout {
    /// The layout of a pack of `pack_size` bytes, or `None` when it has more than `u32::MAX`
    /// cells.
    #[must_use]
    pub fn for_pack(pack_size: u64) -> Option<Self> {
        let cells = pack_size.div_ceil(1 << CELL_SHIFT);
        Some(Self {
            pack_size,
            cell_count: u32::try_from(cells).ok()?,
        })
    }

    /// The number of 16-cell groups.
    #[must_use]
    pub fn group_count(&self) -> u32 {
        self.cell_count.div_ceil(GROUP_CELLS)
    }

    /// The offset of cell `0`'s byte.
    #[must_use]
    pub const fn cells_offset(&self) -> u64 {
        STATE_PAGE_HEADER_LEN
    }

    /// The offset of group `0`'s touch epoch.
    #[must_use]
    pub fn epochs_offset(&self) -> u64 {
        (STATE_PAGE_HEADER_LEN + u64::from(self.cell_count)).next_multiple_of(8)
    }

    /// The page's length.
    #[must_use]
    pub fn len(&self) -> u64 {
        (self.epochs_offset() + 4 * u64::from(self.group_count())).next_multiple_of(PAGE_BYTES)
    }

    /// Whether the page has no cell (a pack of zero bytes).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cell_count == 0
    }

    /// The header's 64 bytes.
    #[must_use]
    pub fn header(&self) -> [u8; STATE_PAGE_HEADER_LEN as usize] {
        let mut h = [0u8; STATE_PAGE_HEADER_LEN as usize];
        h[0..4].copy_from_slice(&STATE_PAGE_MAGIC.to_le_bytes());
        h[4..6].copy_from_slice(&STATE_PAGE_VERSION.to_le_bytes());
        h[6] = CELL_SHIFT;
        // GROUP_CELLS is 16.
        h[7] = GROUP_CELLS as u8;
        h[8..12].copy_from_slice(&self.cell_count.to_le_bytes());
        h[12..16].copy_from_slice(&self.group_count().to_le_bytes());
        h[16..24].copy_from_slice(&self.pack_size.to_le_bytes());
        h[24..32].copy_from_slice(&self.cells_offset().to_le_bytes());
        h[32..40].copy_from_slice(&self.epochs_offset().to_le_bytes());
        h[40..48].copy_from_slice(&self.len().to_le_bytes());
        h
    }

    /// The layout a header describes, if it is a header of this version whose fields agree with
    /// the layout of its pack size.
    #[must_use]
    pub fn from_header(h: &[u8]) -> Option<Self> {
        let h: &[u8; STATE_PAGE_HEADER_LEN as usize] = h.first_chunk()?;
        let u64_at = |at: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&h[at..at + 8]);
            u64::from_le_bytes(b)
        };
        let layout = Self::for_pack(u64_at(16))?;
        (layout.header() == *h).then_some(layout)
    }
}

/// A pack's region in a mount's counter memfd: one `u16` per group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CounterRegion {
    /// The region's offset in the memfd, a multiple of 8.
    pub offset: u64,
    /// The groups it counts.
    pub groups: u32,
}

impl CounterRegion {
    /// The bytes the region takes, a multiple of 8.
    #[must_use]
    pub fn len(&self) -> u64 {
        (2 * u64::from(self.groups)).next_multiple_of(8)
    }

    /// Whether the region counts no group.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups == 0
    }

    /// The end of the region in the memfd: the mapping length the server needs to read it.
    #[must_use]
    pub fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.len())
    }

    /// The offset of `group`'s counter, or `None` outside the region or when it overflows.
    #[must_use]
    pub fn counter_offset(&self, group: u32) -> Option<u64> {
        if group < self.groups {
            self.offset.checked_add(2 * u64::from(group))
        } else {
            None
        }
    }

    /// Whether the region is well formed: aligned and not overflowing.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.offset.is_multiple_of(8) && self.end().is_some()
    }
}
