//! Pack identity, classes, cell runs and byte ranges (`ids`).
//!
//! Proves the `x-ts-class` header names round trip and nothing else parses, that a cell run
//! enumerates its cells up to and including `u32::MAX` without overflowing, and that a byte range
//! maps to the cells that cover it at every boundary (empty, exact cell, one byte over, end
//! overflow, cell number beyond `u32`, shift of 64 or more).
//!
//! Catches:
//! - a class header that does not round trip, or an unknown or differently cased value that parses;
//! - a cell run that panics or wraps at the end of the `u32` cell space;
//! - a byte range that is off by one at a cell boundary, or whose empty or overflowing form
//!   yields cells;
//! - a shift of 64 or more that panics instead of yielding none;
//! - the id types losing their order or hash, which the maps of the engine and the store rely on.

use std::collections::{BTreeSet, HashSet};

use tierstore_fetch_api::ids::{
    ByteRange, CellRun, Class, HolderId, JobId, PackId, PackKey, PackRoot, Rank, TreeRef,
};
use tierstore_format::ids::Generation;

const CELL: u64 = 1 << 20;

#[test]
fn every_class_header_round_trips_and_names_the_documented_value() {
    let all = [
        (Class::Demand, "demand"),
        (Class::Prepare, "prepare"),
        (Class::Background, "background"),
    ];
    for (class, text) in all {
        assert_eq!(class.header(), text);
        assert_eq!(Class::from_header(text), Some(class));
    }
}

#[test]
fn an_unknown_or_miscased_class_header_is_refused() {
    for bad in [
        "",
        "Demand",
        "PREPARE",
        "background ",
        "interactive",
        "demand\0",
    ] {
        assert_eq!(Class::from_header(bad), None, "{bad:?}");
    }
}

#[test]
fn classes_order_demand_before_prepare_before_background() {
    assert!(Class::Demand < Class::Prepare);
    assert!(Class::Prepare < Class::Background);
}

#[test]
fn a_cell_run_lists_its_cells_in_ascending_order() {
    let run = CellRun { first: 5, count: 3 };
    assert_eq!(run.cells().collect::<Vec<_>>(), vec![5, 6, 7]);
}

#[test]
fn an_empty_cell_run_has_no_cells_even_at_the_end_of_the_space() {
    assert_eq!(CellRun { first: 0, count: 0 }.cells().count(), 0);
    assert_eq!(
        CellRun {
            first: u32::MAX,
            count: 0
        }
        .cells()
        .count(),
        0
    );
}

#[test]
fn a_cell_run_ending_at_u32_max_includes_it_and_does_not_overflow() {
    let run = CellRun {
        first: u32::MAX - 1,
        count: 2,
    };
    assert_eq!(
        run.cells().collect::<Vec<_>>(),
        vec![u32::MAX - 1, u32::MAX]
    );
    let last = CellRun {
        first: u32::MAX,
        count: 1,
    };
    assert_eq!(last.cells().collect::<Vec<_>>(), vec![u32::MAX]);
}

#[test]
fn a_cell_run_that_leaves_the_cell_space_stops_at_u32_max() {
    let run = CellRun {
        first: u32::MAX - 1,
        count: 10,
    };
    assert_eq!(
        run.cells().collect::<Vec<_>>(),
        vec![u32::MAX - 1, u32::MAX]
    );
}

#[test]
fn the_end_of_a_byte_range_is_exclusive_and_overflow_is_none() {
    assert_eq!(ByteRange { offset: 10, len: 5 }.end(), Some(15));
    assert_eq!(ByteRange { offset: 0, len: 0 }.end(), Some(0));
    assert_eq!(
        ByteRange {
            offset: u64::MAX - 1,
            len: 1
        }
        .end(),
        Some(u64::MAX)
    );
    assert_eq!(
        ByteRange {
            offset: u64::MAX,
            len: 1
        }
        .end(),
        None
    );
}

fn cells(offset: u64, len: u64) -> Option<(u32, u32)> {
    ByteRange { offset, len }
        .cells(20)
        .map(|r| (r.first, r.count))
}

#[test]
fn an_empty_byte_range_covers_no_cell() {
    assert_eq!(cells(0, 0), None);
    assert_eq!(cells(CELL, 0), None);
    assert_eq!(cells(u64::MAX, 0), None);
}

#[test]
fn a_byte_range_maps_to_the_cells_that_cover_it_at_every_boundary() {
    assert_eq!(cells(0, 1), Some((0, 1)));
    assert_eq!(cells(0, CELL), Some((0, 1)));
    assert_eq!(cells(0, CELL + 1), Some((0, 2)));
    assert_eq!(cells(CELL - 1, 1), Some((0, 1)));
    assert_eq!(cells(CELL - 1, 2), Some((0, 2)));
    assert_eq!(cells(CELL, CELL), Some((1, 1)));
    assert_eq!(cells(CELL, 1), Some((1, 1)));
    assert_eq!(cells(CELL + 5, 3 * CELL), Some((1, 4)));
    assert_eq!(cells(2 * CELL, 3 * CELL), Some((2, 3)));
}

#[test]
fn a_byte_range_whose_end_overflows_covers_no_cell() {
    assert_eq!(cells(u64::MAX, 1), None);
    assert_eq!(cells(1, u64::MAX), None);
}

#[test]
fn a_byte_range_whose_cell_numbers_do_not_fit_a_u32_covers_no_cell() {
    let last_cell_start = u64::from(u32::MAX) << 20;
    assert_eq!(cells(last_cell_start, CELL), Some((u32::MAX, 1)));
    assert_eq!(cells(last_cell_start, CELL + 1), None);
    assert_eq!(cells(last_cell_start + CELL, 1), None);
    assert_eq!(cells(u64::MAX - 1, 1), None);
}

#[test]
fn the_cell_size_is_the_shift() {
    let r = ByteRange { offset: 5, len: 3 };
    assert_eq!(r.cells(0), Some(CellRun { first: 5, count: 3 }));
    assert_eq!(r.cells(1), Some(CellRun { first: 2, count: 2 }));
    assert_eq!(r.cells(3), Some(CellRun { first: 0, count: 1 }));
    assert_eq!(r.cells(63), Some(CellRun { first: 0, count: 1 }));
}

#[test]
fn a_shift_of_64_or_more_has_no_cells_instead_of_panicking() {
    let r = ByteRange { offset: 5, len: 3 };
    assert_eq!(r.cells(64), None);
    assert_eq!(r.cells(200), None);
    assert_eq!(r.cells(255), None);
}

#[test]
fn pack_ids_order_and_hash_by_key_then_root() {
    let a = PackId {
        key: PackKey("a".into()),
        root: PackRoot::Digest([2; 32]),
    };
    let b = PackId {
        key: PackKey("a".into()),
        root: PackRoot::Etag(Generation([0; 16])),
    };
    let c = PackId {
        key: PackKey("b".into()),
        root: PackRoot::Digest([0; 32]),
    };
    // Digest sorts before Etag (declaration order), and the key sorts first.
    let sorted: Vec<_> = [c.clone(), b.clone(), a.clone()]
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(sorted, vec![a.clone(), b.clone(), c.clone()]);
    let set: HashSet<_> = [a.clone(), b, c, a.clone()].into_iter().collect();
    assert_eq!(set.len(), 3);
    assert_eq!(a.clone(), a);
}

#[test]
fn the_party_ids_order_by_value_and_default_to_zero() {
    assert!(HolderId(1) < HolderId(2));
    assert!(JobId(1) < JobId(2));
    assert!(Rank(0) < Rank(1));
    assert_eq!(JobId::default(), JobId(0));
    assert_eq!(Rank::default(), Rank(0));
    let set: HashSet<_> = [HolderId(1), HolderId(1), HolderId(2)]
        .into_iter()
        .collect();
    assert_eq!(set.len(), 2);
    assert!(TreeRef("a".into()) < TreeRef("b".into()));
}

#[test]
fn cell_runs_and_byte_ranges_order_by_first_then_count() {
    assert!(CellRun { first: 1, count: 9 } < CellRun { first: 2, count: 0 });
    assert!(CellRun { first: 1, count: 1 } < CellRun { first: 1, count: 2 });
    assert!(ByteRange { offset: 1, len: 9 } < ByteRange { offset: 2, len: 0 });
    let set: HashSet<_> = [CellRun { first: 1, count: 1 }; 3].into_iter().collect();
    assert_eq!(set.len(), 1);
    let set: HashSet<_> = [ByteRange { offset: 1, len: 1 }; 3].into_iter().collect();
    assert_eq!(set.len(), 1);
}
