//! The types of `entry`, `engine`, `verify` and the crate root.
//!
//! Proves `StateView::ready` (a run is ready only when every cell of it is present; a run that
//! leaves the pack is not ready; an empty run is ready), the claim token, the text of every
//! `EntryError`, the `Debug` of the engine's spec types, and the verifier's types.
//!
//! Catches:
//! - `ready` that accepts a non-present cell, a cell past the pack, or rejects an empty run;
//! - `ready` that panics on a run ending at `u32::MAX`;
//! - a claim token that names another cell than it was made for;
//! - an entry error that loses the pack, the cell or a number it reports, or an `Io` error that
//!   hides its source;
//! - a spec `Debug` that prints the mapped list;
//! - a verifier priority that sorts `recheck` before a fill.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;

use bytes::Bytes;
use tierstore_fetch_api::TREE_LOST_CAUSE;
use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::engine::{PackDigests, PackSpec, WaitOutcome};
use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, EntryError, EntrySpec, PunchReport, StateView,
};
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId, PackKey, PackRoot};
use tierstore_fetch_api::list::{CellDigest, ListL2, NodeIndex, PackRecord};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_fetch_api::verify::{CellJob, Verdict, VerifyPriority};

/// A state view over a fixed list of bytes: only `cell_count` and `load` are its own, so `ready`
/// is the trait's default method.
struct Cells(Vec<CellByte>);

impl StateView for Cells {
    fn cell_count(&self) -> u32 {
        u32::try_from(self.0.len()).expect("small")
    }

    fn load(&self, cell: u32) -> Option<CellByte> {
        self.0.get(cell as usize).copied()
    }
}

fn present(durable: bool) -> CellByte {
    CellByte::new(CellState::Present, durable)
}

fn run(first: u32, count: u32) -> CellRun {
    CellRun { first, count }
}

#[test]
fn a_run_is_ready_when_every_cell_of_it_is_present_durable_or_not() {
    let v = Cells(vec![present(false), present(true), present(false)]);
    assert_eq!(v.cell_count(), 3);
    assert!(v.ready(run(0, 3)));
    assert!(v.ready(run(1, 1)));
    assert!(v.ready(run(2, 1)));
}

#[test]
fn a_run_with_a_cell_in_any_other_state_is_not_ready() {
    for s in [
        CellState::Absent,
        CellState::Claimed,
        CellState::Recheck,
        CellState::Evicting,
        CellState::Bad,
    ] {
        for at in 0..3usize {
            let mut cells = vec![present(false); 3];
            cells[at] = CellByte::new(s, false);
            let v = Cells(cells);
            assert!(!v.ready(run(0, 3)), "{s:?} at {at}");
        }
    }
    // A cell outside the run does not matter.
    let v = Cells(vec![CellByte::new(CellState::Bad, false), present(false)]);
    assert!(v.ready(run(1, 1)));
    assert!(!v.ready(run(0, 2)));
}

#[test]
fn a_byte_with_a_reserved_bit_is_not_ready() {
    let v = Cells(vec![CellByte(0x0A)]);
    assert!(!v.ready(run(0, 1)));
}

#[test]
fn a_run_that_leaves_the_pack_is_not_ready_even_if_every_cell_inside_is() {
    let v = Cells(vec![present(false), present(false)]);
    assert!(!v.ready(run(0, 3)));
    assert!(!v.ready(run(2, 1)));
    assert!(!v.ready(run(5, 1)));
    assert!(!v.ready(run(u32::MAX, 1)));
    assert!(!v.ready(run(1, u32::MAX)));
}

#[test]
fn an_empty_run_is_ready_wherever_it_is() {
    let v = Cells(vec![]);
    assert_eq!(v.cell_count(), 0);
    assert!(v.ready(run(0, 0)));
    assert!(v.ready(run(7, 0)));
    assert!(v.ready(run(u32::MAX, 0)));
}

#[test]
fn a_claim_token_names_the_cell_it_was_won_for() {
    let c = ClaimedCell::won(41);
    assert_eq!(c.cell(), 41);
    assert_eq!(format!("{c:?}"), "ClaimedCell { cell: 41 }");
    assert_eq!(ClaimedCell::won(1), ClaimedCell::won(1));
    assert_ne!(ClaimedCell::won(1), ClaimedCell::won(2));
}

fn pack() -> PackId {
    PackId {
        key: PackKey("k".into()),
        root: PackRoot::Digest([9; 32]),
    }
}

#[test]
fn every_entry_error_names_the_pack_the_cell_and_the_numbers() {
    let p = pack();
    let id = format!("{p:?}");
    assert_eq!(
        EntryError::CellOutOfRange {
            pack: p.clone(),
            cell: 9,
            cells: 4
        }
        .to_string(),
        format!("{id}: cell 9 outside a pack of 4 cells")
    );
    assert_eq!(
        EntryError::CellLength {
            pack: p.clone(),
            cell: 3,
            expected: 1_048_576,
            got: 10
        }
        .to_string(),
        format!("{id}: cell 3 needs 1048576 bytes, 10 given")
    );
    assert_eq!(
        EntryError::NotClaimed {
            pack: p.clone(),
            cell: 2
        }
        .to_string(),
        format!("{id}: cell 2 is not claimed")
    );
    assert_eq!(
        EntryError::SizeMismatch {
            pack: p.clone(),
            size: 5,
            existing: 6
        }
        .to_string(),
        format!("{id}: opened with size 5, the entry has 6")
    );
    assert_eq!(
        EntryError::TooLarge { pack: p, size: 77 }.to_string(),
        format!("{id}: a pack of 77 bytes is too large")
    );
}

#[test]
fn an_io_entry_error_says_what_was_done_and_keeps_its_source() {
    let p = pack();
    let e = EntryError::Io {
        pack: p.clone(),
        op: "pwrite",
        source: std::io::Error::other("disk gone"),
    };
    assert_eq!(e.to_string(), format!("{p:?}: pwrite: disk gone"));
    let source = std::error::Error::source(&e).expect("the io error");
    assert_eq!(source.to_string(), "disk gone");
    let plain = EntryError::NotClaimed { pack: p, cell: 0 };
    assert!(std::error::Error::source(&plain).is_none());
}

#[test]
fn a_commit_report_defaults_to_no_work_and_a_spec_compares_by_value() {
    assert_eq!(
        CommitReport::default(),
        CommitReport {
            data_syncs: 0,
            cells_durable: 0,
            records: 0
        }
    );
    let spec = EntrySpec {
        pack: pack(),
        size: 10,
    };
    let mut other = spec.clone();
    assert_eq!(spec, other);
    other.size = 11;
    assert_ne!(spec, other);
}

/// A list of one pack, enough to hold in a spec.
struct OnePack;

impl ListL2 for OnePack {
    fn pack_count(&self) -> u32 {
        1
    }
    fn pack_record(&self, _pack: u32) -> Option<PackRecord> {
        None
    }
    fn cell_digest(&self, _pack: u32, _cell: u32) -> Option<CellDigest> {
        None
    }
    fn pack_root(&self, _pack: u32) -> Option<PackRoot> {
        None
    }
    fn dir_lookup(&self, _dir: NodeIndex, _name: &[u8]) -> Option<NodeIndex> {
        None
    }
}

#[test]
fn pack_digests_debug_shows_the_pack_index_and_not_the_list() {
    let d = PackDigests {
        list: Arc::new(OnePack),
        pack: 3,
    };
    assert_eq!(format!("{d:?}"), "PackDigests { pack: 3, .. }");
    let copy = d.clone();
    assert_eq!(copy.pack, 3);
    assert_eq!(copy.list.pack_count(), 1);
}

#[test]
fn a_pack_spec_debug_shows_its_id_size_digests_and_part_size() {
    let with = PackSpec {
        id: pack(),
        size: 5,
        digests: Some(PackDigests {
            list: Arc::new(OnePack),
            pack: 3,
        }),
        upload_part_size: 8,
    };
    let text = format!("{with:?}");
    assert!(text.starts_with("PackSpec { id: PackId {"), "{text}");
    assert!(text.contains("size: 5"), "{text}");
    assert!(
        text.contains("digests: Some(PackDigests { pack: 3, .. })"),
        "{text}"
    );
    assert!(text.ends_with("upload_part_size: 8 }"), "{text}");
    let none = PackSpec {
        digests: None,
        ..with.clone()
    };
    assert!(format!("{none:?}").contains("digests: None"));
}

#[test]
fn wait_outcomes_compare_by_the_cell_they_name() {
    assert_eq!(WaitOutcome::Eio { cell: 1 }, WaitOutcome::Eio { cell: 1 });
    assert_ne!(WaitOutcome::Eio { cell: 1 }, WaitOutcome::Eio { cell: 2 });
    assert_ne!(WaitOutcome::Ready, WaitOutcome::Lost);
}

#[test]
fn a_fill_cell_sorts_before_a_recheck_cell_in_the_hash_pool() {
    assert!(VerifyPriority::Fill < VerifyPriority::Recheck);
    let job = CellJob {
        cell: 4,
        expected: [1; 32],
        bytes: Bytes::from_static(b"abc"),
        priority: VerifyPriority::Fill,
    };
    let mut other = job.clone();
    assert_eq!(job, other);
    other.priority = VerifyPriority::Recheck;
    assert_ne!(job, other);
    assert_ne!(Verdict::Match, Verdict::Mismatch);
}

#[test]
fn the_cause_reported_for_a_lost_tree_is_tree_lost() {
    assert_eq!(TREE_LOST_CAUSE, "tree-lost");
}

#[test]
fn the_new_entry_errors_name_the_pack_the_cell_and_the_region() {
    let p = pack();
    let id = format!("{p:?}");
    assert_eq!(
        EntryError::NotEvicting {
            pack: p.clone(),
            cell: 4
        }
        .to_string(),
        format!("{id}: cell 4 is not evicting")
    );
    assert_eq!(
        EntryError::NotReadable {
            pack: p.clone(),
            cell: 5,
            state: CellByte(0x84)
        }
        .to_string(),
        format!("{id}: cell 5 is CellByte(132), not recheck or present")
    );
    assert_eq!(
        EntryError::UnknownPack { pack: p.clone() }.to_string(),
        format!("{id}: no such entry")
    );
    assert_eq!(
        EntryError::UnknownHolder {
            holder: HolderId(7)
        }
        .to_string(),
        "holder HolderId(7) is not registered"
    );
    assert_eq!(
        EntryError::BadRegion {
            pack: p,
            region: CounterRegion {
                offset: 4,
                groups: 3
            }
        }
        .to_string(),
        format!("{id}: bad counter region CounterRegion {{ offset: 4, groups: 3 }}")
    );
    let map = EntryError::CountersMap {
        len: 99,
        source: std::io::Error::other("no memory"),
    };
    assert_eq!(
        map.to_string(),
        "mapping 99 bytes of a holder's counters: no memory"
    );
    assert!(std::error::Error::source(&map).is_some());
}

#[test]
fn a_punch_report_defaults_to_no_work_and_counts_default_to_zero() {
    assert_eq!(
        PunchReport::default(),
        PunchReport {
            journal_syncs: 0,
            records: 0,
            bytes_punched: 0
        }
    );
    assert_eq!(CellCounts::default().used(), 0);
    let counts = CellCounts {
        absent: 9,
        claimed: 1,
        present: 2,
        recheck: 3,
        evicting: 4,
        bad: 5,
    };
    // Only present, claimed and recheck cells are use (7).
    assert_eq!(counts.used(), 6);
}
