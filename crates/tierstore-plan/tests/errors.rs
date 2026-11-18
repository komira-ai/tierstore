//! What the planner refuses: needed ranges the list cannot hold (`PlanError`), each with its exact
//! message. A profile never makes a plan fail (`ranks.rs`).
//!
//! Catches:
//! - a needed range of a missing pack, or past its pack, planned as some other cells;
//! - a range whose end overflows wrapping round to the pack's head;
//! - a record whose cells do not cover its bytes read past its cell table;
//! - an error message that loses the pack or the range.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod support;

use support::{Case, FakeList, MIB, bytes, cells, list_mib, pack};
use tierstore_plan::plan::PlanError;

#[test]
fn a_range_of_a_pack_the_list_does_not_have_is_refused_even_when_empty() {
    let list = list_mib(&[100]);
    for needed in [cells(5, 0, 1), bytes(5, 0, 0)] {
        let err = Case::new(&list, vec![cells(0, 0, 1), needed])
            .try_plan()
            .unwrap_err();
        assert_eq!(err, PlanError::NoSuchPack { pack: 5 });
        assert_eq!(
            err.to_string(),
            "needed range names pack 5, which the list does not have"
        );
    }
}

#[test]
fn a_range_past_the_pack_end_is_refused_and_one_ending_at_it_is_not() {
    let list = list_mib(&[100]);
    let err = Case::new(&list, vec![bytes(0, 100 * MIB - 1, 2)])
        .try_plan()
        .unwrap_err();
    assert_eq!(
        err,
        PlanError::OutsidePack {
            pack: 0,
            offset: 100 * MIB - 1,
            len: 2,
        }
    );
    assert_eq!(
        err.to_string(),
        "needed range 104857599+2 is outside pack 0"
    );
    let plan = Case::new(&list, vec![bytes(0, 100 * MIB - 1, 1)]).plan();
    assert_eq!(plan.packs[0].needed_cells, 1);
}

#[test]
fn a_range_whose_end_overflows_is_outside_the_pack() {
    let list = list_mib(&[100]);
    let err = Case::new(&list, vec![bytes(0, u64::MAX, 2)])
        .try_plan()
        .unwrap_err();
    assert_eq!(
        err,
        PlanError::OutsidePack {
            pack: 0,
            offset: u64::MAX,
            len: 2,
        }
    );
}

#[test]
fn a_record_whose_cells_cannot_hold_its_bytes_is_refused() {
    // A cell shift of 64: no cell grid.
    let mut wide = pack("p0", 10 * MIB);
    wide.cell_shift = 64;
    // Fewer cells than the size needs: cell 5 of a 10 MiB pack recorded with 2 cells.
    let mut short = pack("p0", 10 * MIB);
    short.cell_count = 2;
    // Byte cells over 2^32 bytes: the cell index does not fit a u32.
    let mut tiny = pack("p0", 1 << 33);
    tiny.cell_shift = 0;
    tiny.cell_count = u32::MAX;
    for (record, needed) in [
        (wide, bytes(0, 0, 1)),
        (short, cells(0, 5, 1)),
        (tiny, bytes(0, (1 << 33) - 1, 1)),
    ] {
        let list = FakeList {
            packs: vec![record],
        };
        let err = Case::new(&list, vec![needed]).try_plan().unwrap_err();
        assert_eq!(err, PlanError::BadPackRecord { pack: 0 });
        assert_eq!(
            err.to_string(),
            "pack 0's record does not cover its own bytes in cells"
        );
    }
}

#[test]
fn a_record_with_exactly_its_cells_is_planned() {
    let mut exact = pack("p0", 10 * MIB);
    exact.cell_count = 10;
    let list = FakeList { packs: vec![exact] };
    let plan = Case::new(&list, vec![cells(0, 9, 1)]).plan();
    assert_eq!(plan.packs[0].needed_cells, 1);
}
