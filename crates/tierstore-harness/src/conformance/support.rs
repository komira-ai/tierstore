//! Helpers shared by the cases: packs of whole cells, one-cell publishes and state reads.

use std::sync::Arc;

use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{ClaimedCell, Entry, EntryError, EntrySpec, EntryStore};
use tierstore_fetch_api::ids::{CellRun, PackId, PackKey, PackRoot};
use tierstore_fetch_api::page::CELL_SHIFT;

use super::EntryStoreFixture;
use super::check::{ensure_eq, expect_err, expect_ok};

/// The bytes of one cell.
pub(super) const MIB: u64 = 1 << CELL_SHIFT;

/// The pack named `name` (every case's packs share one root).
pub(super) fn pack(name: &str) -> PackId {
    PackId {
        key: PackKey(name.to_owned()),
        root: PackRoot::Digest([2; 32]),
    }
}

/// A run of cells.
pub(super) fn run(first: u32, count: u32) -> CellRun {
    CellRun { first, count }
}

/// The entry of `name`, `size` bytes, opened in `store`.
pub(super) fn open(store: &dyn EntryStore, name: &str, size: u64) -> Arc<dyn Entry> {
    let spec = EntrySpec {
        pack: pack(name),
        size,
    };
    expect_ok(store.open(&spec), format_args!("open {name}"))
}

/// The cell byte of `state`.
pub(super) fn cb(state: CellState, durable: bool) -> Option<CellByte> {
    Some(CellByte::new(state, durable))
}

/// The byte of `cell` as the state view loads it.
pub(super) fn at(e: &dyn Entry, cell: u32) -> Option<CellByte> {
    e.state().load(cell)
}

/// The state of `cell`.
pub(super) fn state_of(e: &dyn Entry, cell: u32) -> Option<CellState> {
    at(e, cell).and_then(CellByte::state)
}

/// The cells of the tokens, in order.
pub(super) fn cells(won: &[ClaimedCell]) -> Vec<u32> {
    won.iter().map(ClaimedCell::cell).collect()
}

/// The length of `cell`: a whole cell, or what is left of the pack for the last one.
pub(super) fn cell_len(e: &dyn Entry, cell: u32) -> usize {
    let start = u64::from(cell) << CELL_SHIFT;
    // A cell is at most 1 MiB, so it fits a usize.
    e.size().saturating_sub(start).min(MIB) as usize
}

/// The bytes a case publishes into `cell`: `fill`, `cell_len` times.
pub(super) fn bytes(e: &dyn Entry, cell: u32, fill: u8) -> Vec<u8> {
    vec![fill; cell_len(e, cell)]
}

/// Claims `cell` and publishes `fill` bytes into it.
pub(super) fn publish(e: &dyn Entry, cell: u32, fill: u8) {
    let won = e.claim(run(cell, 1));
    ensure_eq(
        cells(&won),
        vec![cell],
        format_args!("claim of cell {cell}"),
    );
    for token in won {
        expect_ok(
            e.publish(token, &bytes(e, cell, fill)),
            format_args!("publish of cell {cell}"),
        );
    }
}

/// Whether `got` is `want` (the errors are compared by their `Debug` text, a comparison with no
/// branch of its own).
pub(super) fn same(got: &EntryError, want: &EntryError) -> bool {
    format!("{got:?}") == format!("{want:?}")
}

/// Whether `e` refused a punch as a store that cannot punch does: [`EntryError::Io`] of kind
/// `Unsupported`.
pub(super) fn unsupported(e: &EntryError) -> bool {
    matches!(e, EntryError::Io { source, .. } if source.kind() == std::io::ErrorKind::Unsupported)
}

/// The punch of evicting `runs` that comes before the evictor's `Finish`: on a store that punches
/// it succeeds; on one that cannot ([`EntryStoreFixture::punches`] is `false`) it is refused as
/// [`unsupported`], and the `Finish` that follows is the state step alone.
pub(super) fn punch_as_able(
    f: &dyn EntryStoreFixture,
    e: &dyn Entry,
    runs: &[CellRun],
    what: &str,
) {
    if f.punches() {
        expect_ok(e.punch(runs), what);
    } else {
        expect_err(e.punch(runs), unsupported, what);
    }
}
