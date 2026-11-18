//! The part plan of one fill (4.4): which cells each part covers. Pure: sizes in, cell ranges
//! out.
//!
//! - A pack of at most `small_pack_max` bytes: the first-touch cells (at least
//!   `first_piece_min_cells`, at most `first_piece_max_cells`), then one part for the rest after
//!   them and one for the cells before them.
//! - A larger pack: the steady grid is anchored at offset 0 with pitch `P`. The ramp covers
//!   `[first-touch cell, next grid boundary)` in doubling pieces from `ramp_first_cells`, capped
//!   at `P/2`, the last trimmed to the boundary; every later part is a whole grid part (the last
//!   one trimmed at the pack's end), and the cells before the first touch are whole grid parts
//!   plus the head of the first-touch cell's grid part.
//! - Only for a legacy source with a recorded `upload_part_size`, parts are also cut at the cells
//!   of those boundaries (4.4; deleted with silo).

use std::collections::BTreeSet;

use tierstore_fetch_api::page::CELL_SHIFT;

use crate::config::EngineConfig;

/// The bytes of a cell.
pub const CELL_BYTES: u64 = 1 << CELL_SHIFT;

/// Cells `[first, end)` of a pack: one part of the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Piece {
    /// The first cell.
    pub first: u32,
    /// One past the last cell.
    pub end: u32,
}

impl Piece {
    /// The number of cells.
    #[must_use]
    pub fn cells(self) -> u32 {
        self.end - self.first
    }
}

/// What a plan is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanInput {
    /// The pack's size in bytes.
    pub pack_size: u64,
    /// The current part size `P` in bytes (rounded down to whole cells, at least one).
    pub part_size: u64,
    /// The first cell anyone asked for.
    pub first_touch: u32,
    /// The cells of that first request.
    pub first_touch_cells: u32,
}

/// The number of cells of a pack of `size` bytes, or `None` beyond `u32::MAX` cells.
#[must_use]
pub fn cell_count(size: u64) -> Option<u32> {
    u32::try_from(size.div_ceil(CELL_BYTES)).ok()
}

/// The size class of a pack for the budget's signals (4.4's rows): 0 up to 32 MiB, 1 up to
/// 512 MiB, 2 above.
#[must_use]
pub fn size_class(pack_size: u64) -> u8 {
    match pack_size {
        s if s <= 32 * CELL_BYTES => 0,
        s if s <= 512 * CELL_BYTES => 1,
        _ => 2,
    }
}

/// The plan: pieces sorted by first cell, disjoint, covering every cell of the pack, and cut at
/// every cell of `cuts` (the legacy upload parts, the extents of a chain's blocks).
#[must_use]
pub fn plan(config: &EngineConfig, input: PlanInput, cuts: &BTreeSet<u32>) -> Vec<Piece> {
    let n = cell_count(input.pack_size).unwrap_or(u32::MAX);
    if n == 0 {
        return Vec::new();
    }
    let ft = input.first_touch.min(n - 1);
    let mut pieces = if input.pack_size <= config.small_pack_max {
        small(config, n, ft, input.first_touch_cells)
    } else {
        let p = u32::try_from(input.part_size >> CELL_SHIFT)
            .unwrap_or(u32::MAX)
            .max(1);
        large(config, n, ft, p)
    };
    pieces = cut(&pieces, cuts);
    pieces.sort_unstable();
    pieces
}

fn push(pieces: &mut Vec<Piece>, first: u32, end: u32) {
    if first < end {
        pieces.push(Piece { first, end });
    }
}

fn small(config: &EngineConfig, n: u32, ft: u32, touched: u32) -> Vec<Piece> {
    let len = touched.clamp(config.first_piece_min_cells, config.first_piece_max_cells);
    let end = ft.saturating_add(len).min(n);
    let mut pieces = Vec::new();
    push(&mut pieces, 0, ft);
    push(&mut pieces, ft, end);
    push(&mut pieces, end, n);
    pieces
}

fn large(config: &EngineConfig, n: u32, ft: u32, p: u32) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let grid_start = ft / p * p;
    let boundary = grid_start.saturating_add(p).min(n);
    // Before the first touch: whole grid parts, then the head of the first touch's grid part.
    let mut at = 0;
    while at < grid_start {
        push(&mut pieces, at, at + p);
        at += p;
    }
    push(&mut pieces, grid_start, ft);
    // The ramp, relative to the grid.
    let cap = (p / 2).max(1);
    let mut size = config.ramp_first_cells.max(1);
    let mut at = ft;
    while at < boundary {
        let len = size.min(cap).min(boundary - at);
        push(&mut pieces, at, at + len);
        at += len;
        size = size.saturating_mul(2);
    }
    // Whole grid parts to the end.
    let mut at = boundary;
    while at < n {
        let end = at.saturating_add(p).min(n);
        push(&mut pieces, at, end);
        at = end;
    }
    pieces
}

/// The cells at which upload parts of `upload` bytes begin, inside a pack of `size` bytes (none
/// for 0).
#[must_use]
pub fn upload_cuts(size: u64, upload: u64) -> BTreeSet<u32> {
    let mut cuts = BTreeSet::new();
    if upload == 0 {
        return cuts;
    }
    let mut at = upload;
    while at < size {
        // `at < size`, and the size has fewer than 2^32 cells.
        cuts.insert((at >> CELL_SHIFT) as u32);
        at = at.saturating_add(upload);
    }
    cuts
}

fn cut(pieces: &[Piece], cuts: &BTreeSet<u32>) -> Vec<Piece> {
    let mut out = Vec::new();
    for piece in pieces {
        let mut first = piece.first;
        for &c in cuts.range(piece.first + 1..piece.end) {
            push(&mut out, first, c);
            first = c;
        }
        push(&mut out, first, piece.end);
    }
    out
}

/// The index of the piece that holds `cell` in a plan made by [`plan`], or `None` past the pack.
#[must_use]
pub fn piece_of(pieces: &[Piece], cell: u32) -> Option<usize> {
    let i = pieces.partition_point(|p| p.end <= cell);
    pieces.get(i).filter(|p| p.first <= cell).map(|_| i)
}
