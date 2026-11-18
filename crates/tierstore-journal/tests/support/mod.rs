//! Shared helpers of the journal's tests: a pack of four cells (the last one short), the bytes
//! each residency of a cell holds, boot ids, and the publish and evict steps an engine and an
//! evictor take on an [`FsEntry`].

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the helpers"
)]

use sha2::{Digest, Sha256};
use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{Entry, EntryError, EntrySpec, EvictStep};
use tierstore_fetch_api::ids::{CellRun, PackId, PackKey, PackRoot};
use tierstore_journal::fake::{FsEntry, MemFs};
use tierstore_journal::{BootId, Recovery};

/// One cell.
pub const MIB: u64 = 1 << 20;

/// The pack's size: three whole cells and a last one of 1000 bytes.
pub const SIZE: u64 = 3 * MIB + 1000;

/// Its cells.
pub const CELLS: u32 = 4;

/// The boot the tests run "now".
pub const BOOT_A: BootId = BootId([0xa1; 16]);

/// Another boot.
pub const BOOT_B: BootId = BootId([0xb2; 16]);

/// The pack of the tests.
pub fn pack() -> PackId {
    PackId {
        key: PackKey("packs/0123/abcd.pack".to_owned()),
        root: PackRoot::Digest([7; 32]),
    }
}

/// The entry spec of the tests' pack.
pub fn spec() -> EntrySpec {
    EntrySpec {
        pack: pack(),
        size: SIZE,
    }
}

/// The length of `cell`.
pub fn cell_len(cell: u32) -> usize {
    let start = u64::from(cell) * MIB;
    ((start + MIB).min(SIZE) - start) as usize
}

/// The bytes of residency `residency` of `cell`: different for every cell and residency, never
/// all zeros (a punched cell reads as zeros, so a resurrected one is caught).
pub fn cell_bytes(cell: u32, residency: u32) -> Vec<u8> {
    (0..cell_len(cell))
        .map(|i| (cell as usize * 31 + residency as usize * 7 + i % 251 + 1) as u8)
        .collect()
}

/// The sha256 of `bytes`: the cell digest of the list (3.1), what a recheck re-hashes with.
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Opens the tests' entry on `fs` in `boot`.
pub fn open(fs: &MemFs, boot: Option<BootId>) -> Result<(FsEntry, Recovery), EntryError> {
    FsEntry::open(fs.clone(), &spec(), boot)
}

/// Claims and publishes residency `residency` of each of `cells`, as a fill does after the
/// bytes verified.
pub fn publish(entry: &FsEntry, cells: &[u32], residency: u32) -> Result<(), EntryError> {
    for &c in cells {
        let claimed = entry.claim(CellRun { first: c, count: 1 });
        for token in claimed {
            entry.publish(token, &cell_bytes(c, residency))?;
        }
    }
    Ok(())
}

/// Evicts `runs` as the evictor of 5.1 does once the readers are clear: `present -> evicting` on
/// every cell, the journaled punch, then `evicting -> absent`.
pub fn evict(entry: &FsEntry, runs: &[CellRun]) -> Result<(), EntryError> {
    for c in runs.iter().flat_map(|r| r.cells()) {
        assert!(entry.evict_cell_cas(c, EvictStep::Begin), "begin evict {c}");
    }
    entry.punch(runs)?;
    for c in runs.iter().flat_map(|r| r.cells()) {
        assert!(
            entry.evict_cell_cas(c, EvictStep::Finish),
            "finish evict {c}"
        );
    }
    Ok(())
}

/// The state of `cell`.
pub fn state(entry: &FsEntry, cell: u32) -> Option<CellState> {
    entry.state().load(cell).and_then(CellByte::state)
}

/// The states of every cell.
pub fn states(entry: &FsEntry) -> Vec<CellState> {
    (0..CELLS).filter_map(|c| state(entry, c)).collect()
}

/// The byte of each state the recovery restored, as states.
pub fn restored(r: &Recovery) -> Vec<CellState> {
    r.cells.iter().filter_map(|b| b.state()).collect()
}
