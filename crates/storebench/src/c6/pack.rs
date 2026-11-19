//! First-fit-decreasing pack assembly of the objects above `large_min` (design 10.5): how much of
//! a block the records leave unused for each block size and window.
//!
//! The coordinator takes a window of the oldest `destage.pack_window` large files, orders them by
//! size (largest first) and places each in the first block that still has room for its record
//! (the object rounded up to 4 KiB), a new block when none has. A block holds
//! `block_bytes - block_reserve`. An object larger than that is a chain: it takes
//! `ceil(size / (block_bytes - block_reserve))` blocks of its own and is not placed.

use serde::{Deserialize, Serialize};

use super::{Count, ratio};

/// The bytes of a block that records may not use (`cold.block_reserve`, the trailer).
pub const BLOCK_RESERVE: u64 = 1 << 20;

/// Records are aligned to this many bytes inside a block.
pub const RECORD_ALIGN: u64 = 4096;

/// The size above which an object is a whole file and not a segment record (`hot.large_min`).
pub const LARGE_MIN: u64 = 4 << 20;

/// The window sizes tried (`destage.pack_window`).
pub const WINDOWS: [usize; 3] = [16, 64, 256];

/// The block sizes tried (`cold.block_bytes`).
pub const BLOCKS: [u64; 4] = [256 << 20, 512 << 20, 1 << 30, 2 << 30];

/// The objects counted as "400 to 700 MiB" packs, both bounds inclusive.
pub const MID_RANGE: (u64, u64) = (400 << 20, 700 << 20);

/// The outcome for one block size and window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fit {
    /// The block size.
    pub block_bytes: u64,
    /// The window of objects placed together.
    pub window: usize,
    /// Blocks used: those the first-fit placement filled and the chains' blocks.
    pub blocks: u64,
    /// Of `blocks`, the blocks of chains.
    pub chain_blocks: u64,
    /// Objects that are chains.
    pub chains: u64,
    /// The records' bytes (each object rounded up to 4 KiB), chains included.
    pub data_bytes: u64,
    /// `data_bytes / (blocks * block_bytes)`: how full the blocks are.
    pub mean_fill: f64,
    /// The same without the chains: how full the placed blocks are.
    pub packed_fill: f64,
}

/// What the packing says about a listing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Packing {
    /// The size above which an object is packed (`LARGE_MIN`).
    pub large_min: u64,
    /// The objects above it.
    pub objects: Count,
    /// Of those, the objects of 400 to 700 MiB (`MID_RANGE`).
    pub mid: Count,
    /// One fit per block size and window, block sizes in `BLOCKS` order, windows inside.
    pub fits: Vec<Fit>,
}

/// `size` rounded up to a whole number of 4 KiB.
#[must_use]
pub fn align(size: u64) -> u64 {
    size.div_ceil(RECORD_ALIGN) * RECORD_ALIGN
}

/// How many blocks of `capacity` bytes first-fit decreasing needs for `sizes` (each at most
/// `capacity`): the sizes largest first, each into the first block with room, else a new one.
#[must_use]
pub fn first_fit_decreasing(sizes: &[u64], capacity: u64) -> u64 {
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    let mut free: Vec<u64> = Vec::new();
    for size in sorted {
        match free.iter_mut().find(|room| **room >= size) {
            Some(room) => *room -= size,
            None => free.push(capacity.saturating_sub(size)),
        }
    }
    free.len() as u64
}

/// The packing of `sizes` (objects above `LARGE_MIN`, in arrival order) in consecutive windows
/// of `window` objects into blocks of `block_bytes`.
#[must_use]
pub fn fit(sizes: &[u64], window: usize, block_bytes: u64) -> Fit {
    let capacity = block_bytes - BLOCK_RESERVE;
    let (mut placed_blocks, mut chain_blocks, mut chains) = (0, 0, 0);
    let (mut data, mut chain_data) = (0, 0);
    for group in sizes.chunks(window.max(1)) {
        let mut records = Vec::new();
        for &size in group {
            let record = align(size);
            data += record;
            if size > capacity {
                chains += 1;
                chain_blocks += size.div_ceil(capacity);
                chain_data += record;
            } else {
                records.push(record);
            }
        }
        placed_blocks += first_fit_decreasing(&records, capacity);
    }
    Fit {
        block_bytes,
        window,
        blocks: placed_blocks + chain_blocks,
        chain_blocks,
        chains,
        data_bytes: data,
        mean_fill: ratio(data, (placed_blocks + chain_blocks) * block_bytes),
        packed_fill: ratio(data - chain_data, placed_blocks * block_bytes),
    }
}

/// Every block size and window over `sizes`, the sizes of the objects above `LARGE_MIN` in
/// arrival order.
#[must_use]
pub fn packing(sizes: &[u64]) -> Packing {
    let mut objects = Count::default();
    let mut mid = Count::default();
    for &size in sizes {
        objects.add(size);
        if (MID_RANGE.0..=MID_RANGE.1).contains(&size) {
            mid.add(size);
        }
    }
    let mut fits = Vec::new();
    for block_bytes in BLOCKS {
        for window in WINDOWS {
            fits.push(fit(sizes, window, block_bytes));
        }
    }
    Packing {
        large_min: LARGE_MIN,
        objects,
        mid,
        fits,
    }
}

#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;
