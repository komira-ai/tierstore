//! Shared packs first (section 6.1): the needed ranges of packs whose record has `pack_flags`
//! bit 1 (`shared`) set rank before every other needed range, and each group keeps tree order. No
//! other bit of `pack_flags` moves a rank, and the gap and whole-pack rules are the same for a
//! shared pack as for any other.
//!
//! Catches:
//! - a planner that ignores the shared bit, or reads a bit other than bit 1 as shared;
//! - shared packs ranked among themselves by anything but tree order (pack index, key, size);
//! - the ranges of a shared pack split from one another, or ordered within the pack by anything
//!   but tree order;
//! - a shared pack fetched whole regardless of its coverage, or never downgraded by the backlog;
//! - a missing pack or a range outside its pack no longer refused, or refused as another error,
//!   once shared ranks are computed ahead of the ranges' checks;
//! - a profile input brought back into `PlanInput`.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod support;

use std::collections::BTreeMap;

use support::{Case, MIB, bytes, cells, list_flagged, list_mib, shape};
use tierstore_plan::plan::{Backlog, Fetch, PlanError, PlanInput};

#[test]
fn shared_packs_get_lower_ranks_than_tree_order_packs() {
    let mut list = list_mib(&[100, 100]);
    list.packs[1].pack_flags = 2;
    let plan = Case::new(&list, vec![cells(0, 10, 1), cells(1, 20, 1)]).plan();
    let rank_of = |pack: u32| {
        plan.ensures
            .iter()
            .find(|e| e.pack == pack)
            .map(|e| e.rank)
            .unwrap()
    };
    assert!(rank_of(1) < rank_of(0), "{:?}", shape(&plan));
}

#[test]
fn shared_packs_rank_before_tree_order() {
    let list = list_flagged(&[(100, 0), (100, 0), (100, 2)]);
    let plan = Case::new(
        &list,
        vec![cells(0, 10, 1), cells(1, 20, 1), cells(2, 30, 1)],
    )
    .plan();
    assert_eq!(
        shape(&plan),
        vec![
            (2, 0, vec![(30, 1)]),
            (0, 1, vec![(10, 1)]),
            (1, 2, vec![(20, 1)]),
        ]
    );
}

#[test]
fn with_no_shared_pack_the_ranks_are_tree_order() {
    // Bit 0 (fully covered) is set on pack 1 and says nothing of the order.
    let list = list_flagged(&[(100, 0), (100, 1), (100, 0)]);
    let plan = Case::new(
        &list,
        vec![cells(2, 30, 1), cells(0, 10, 1), cells(1, 20, 1)],
    )
    .plan();
    assert_eq!(
        shape(&plan),
        vec![
            (2, 0, vec![(30, 1)]),
            (0, 1, vec![(10, 1)]),
            (1, 2, vec![(20, 1)]),
        ]
    );
}

#[test]
fn unknown_flag_bits_do_not_change_the_order() {
    let needed = vec![cells(0, 10, 1), cells(1, 20, 1), cells(2, 30, 1)];
    let tree_order = vec![
        (0, 0, vec![(10, 1)]),
        (1, 1, vec![(20, 1)]),
        (2, 2, vec![(30, 1)]),
    ];
    // 0x80 alone, every bit but bit 1, and bit 0 on the last pack: none is shared.
    let list = list_flagged(&[(100, 0x80), (100, 0xfd), (100, 0x01)]);
    assert_eq!(shape(&Case::new(&list, needed.clone()).plan()), tree_order);
    // Bit 1 beside unknown bits is shared all the same.
    let list = list_flagged(&[(100, 0x80), (100, 0x82), (100, 0xff)]);
    assert_eq!(
        shape(&Case::new(&list, needed).plan()),
        vec![
            (1, 0, vec![(20, 1)]),
            (2, 1, vec![(30, 1)]),
            (0, 2, vec![(10, 1)]),
        ]
    );
}

#[test]
fn two_shared_packs_keep_tree_order_between_them() {
    // Pack 2 comes before pack 1 in the tree: the shared ones keep that order, and pack 0 follows.
    let list = list_flagged(&[(100, 0), (100, 2), (100, 2)]);
    let plan = Case::new(
        &list,
        vec![cells(2, 30, 1), cells(0, 10, 1), cells(1, 20, 1)],
    )
    .plan();
    assert_eq!(
        shape(&plan),
        vec![
            (2, 0, vec![(30, 1)]),
            (1, 1, vec![(20, 1)]),
            (0, 2, vec![(10, 1)]),
        ]
    );
}

#[test]
fn the_ranges_of_one_shared_pack_keep_tree_order_ahead_of_the_rest() {
    let list = list_flagged(&[(100, 0), (100, 2)]);
    let plan = Case::new(
        &list,
        vec![
            cells(0, 50, 1),
            cells(1, 70, 1),
            cells(0, 5, 1),
            cells(1, 40, 1),
        ],
    )
    .plan();
    assert_eq!(
        shape(&plan),
        vec![
            (1, 0, vec![(70, 1)]),
            (1, 1, vec![(40, 1)]),
            (0, 2, vec![(50, 1)]),
            (0, 3, vec![(5, 1)]),
        ]
    );
}

#[test]
fn a_shared_pack_that_fails_the_whole_pack_rule_is_fetched_as_merged_runs() {
    // 100 MiB, so the 50% rule applies: cells 10 and 12, one cold gap cell apart, merge into
    // three cells, 3% of the pack.
    let list = list_flagged(&[(100, 0), (100, 2)]);
    let plan = Case::new(
        &list,
        vec![cells(0, 0, 1), cells(1, 10, 1), cells(1, 12, 1)],
    )
    .plan();
    assert_eq!(
        shape(&plan),
        vec![
            (1, 0, vec![(10, 2)]),
            (1, 1, vec![(12, 1)]),
            (0, 2, vec![(0, 1)])
        ]
    );
    assert_eq!(plan.packs[1].fetch, Fetch::Runs);
    assert_eq!(plan.packs[1].merged_cells, 3);
}

#[test]
fn a_shared_pack_at_the_whole_pack_rule_is_fetched_whole_at_its_shared_rank() {
    // 8 MiB, 2 cells needed: 25%, whole from 20% at 64 MiB and below.
    let list = list_flagged(&[(100, 0), (8, 2)]);
    let mut case = Case::new(&list, vec![cells(0, 0, 1), cells(1, 3, 2)]);
    let plan = case.plan();
    assert_eq!(
        shape(&plan),
        vec![(1, 0, vec![(0, 8)]), (0, 1, vec![(0, 1)])]
    );
    assert_eq!(plan.packs[1].fetch, Fetch::Whole);
    // A backlog past its bound downgrades it to its runs, at the same rank.
    case.backlog = Backlog {
        queued_bytes: 1,
        bytes_per_s: 0,
    };
    let plan = case.plan();
    assert_eq!(
        shape(&plan),
        vec![(1, 0, vec![(3, 2)]), (0, 1, vec![(0, 1)])]
    );
    assert_eq!(plan.packs[1].fetch, Fetch::RunsUnderBacklog);
}

#[test]
fn a_needed_range_naming_a_missing_pack_is_still_no_such_pack() {
    let list = list_flagged(&[(100, 2)]);
    for needed in [cells(5, 0, 1), bytes(5, 0, 0)] {
        let err = Case::new(&list, vec![cells(0, 0, 1), needed])
            .try_plan()
            .unwrap_err();
        assert_eq!(err, PlanError::NoSuchPack { pack: 5 });
    }
}

#[test]
fn the_first_bad_range_in_tree_order_is_the_error() {
    // A range past shared pack 0 comes before a range of a missing pack: the planner refuses the
    // first, as it did before shared packs were ranked first.
    let list = list_flagged(&[(100, 2)]);
    let err = Case::new(&list, vec![bytes(0, 100 * MIB, 1), cells(5, 0, 1)])
        .try_plan()
        .unwrap_err();
    assert_eq!(
        err,
        PlanError::OutsidePack {
            pack: 0,
            offset: 100 * MIB,
            len: 1,
        }
    );
}

#[test]
fn an_empty_range_of_a_shared_pack_needs_nothing_and_keeps_its_place() {
    // The empty range of shared pack 1 needs no cell; like every range it holds its place in the
    // order (rank 0), so pack 0's range follows it at rank 1.
    let list = list_flagged(&[(100, 0), (100, 2)]);
    let plan = Case::new(&list, vec![cells(0, 10, 1), bytes(1, 0, 0)]).plan();
    assert_eq!(shape(&plan), vec![(0, 1, vec![(10, 1)])]);
    assert_eq!(plan.packs.len(), 1);
}

#[test]
fn plan_input_has_no_profile_field() {
    // A struct literal names every field, so this stops compiling if `PlanInput` gains one: the
    // planner's input is the list, the needed ranges, the media and the backlog, and nothing
    // recorded by an earlier run.
    let list = list_flagged(&[(100, 2)]);
    let needed = [cells(0, 1, 1)];
    let media = BTreeMap::new();
    let input = PlanInput {
        list: &list,
        needed: &needed,
        media: &media,
        backlog: Backlog::default(),
    };
    let plan = tierstore_plan::plan::plan(&input, &Default::default()).unwrap();
    assert_eq!(shape(&plan), vec![(0, 0, vec![(1, 1)])]);
}
