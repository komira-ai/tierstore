//! Planner ranks (section 6 step 2, 4.5): with no shared pack, a needed cell's rank is its place
//! in tree order; the plan's ensures are in rank order. Shared packs first is in `shared.rs`.
//!
//! Catches:
//! - a cell shared by two files that takes the later file's rank;
//! - one rank for a whole run, so a cell an earlier range needs in its middle waits for its head;
//! - the tail of a large pack ensured before a small pack the job touches earlier;
//! - ensures out of rank order, or a plan that depends on anything but its input.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod support;

use support::{Case, MIB, bytes, cells, list_mib, pack, per_cell, shape};
use tierstore_plan::config::Medium;
use tierstore_plan::plan::{Fetch, PackPlan, Plan};

#[test]
fn without_a_shared_pack_ranks_follow_tree_order() {
    let list = list_mib(&[100, 100, 100]);
    let plan = Case::new(
        &list,
        vec![cells(2, 10, 1), cells(0, 50, 1), cells(1, 0, 1)],
    )
    .plan();
    assert_eq!(
        shape(&plan),
        vec![
            (2, 0, vec![(10, 1)]),
            (0, 1, vec![(50, 1)]),
            (1, 2, vec![(0, 1)]),
        ]
    );
}

#[test]
fn a_cell_two_ranges_share_takes_the_lower_rank() {
    let list = list_mib(&[100]);
    // Cells 1 and 2 at rank 0, then cells 0 and 1 at rank 1: cell 1 is the first range's.
    let plan = Case::new(
        &list,
        vec![bytes(0, MIB + MIB / 2, MIB), bytes(0, MIB / 2, MIB)],
    )
    .plan();
    assert_eq!(
        shape(&plan),
        vec![(0, 0, vec![(1, 2)]), (0, 1, vec![(0, 1)])]
    );
}

#[test]
fn a_cell_an_earlier_range_needs_inside_a_later_run_is_ensured_ahead_of_the_rest_of_the_run() {
    let list = list_mib(&[100]);
    // Cell 12 at rank 0, then cells 10 to 13 at rank 1: one rank per run would hold cell 12 back
    // until cell 10 is read.
    let plan = Case::new(&list, vec![cells(0, 12, 1), cells(0, 10, 4)]).plan();
    assert_eq!(
        shape(&plan),
        vec![(0, 0, vec![(12, 1)]), (0, 1, vec![(10, 2), (13, 1)])]
    );
}

#[test]
fn a_small_early_pack_is_ensured_before_the_tail_of_a_4_gib_pack() {
    let list = list_mib(&[4096, 8]);
    let plan = Case::new(
        &list,
        vec![cells(0, 0, 4), cells(1, 0, 2), cells(0, 4000, 4)],
    )
    .plan();
    // The 8 MiB pack is 25% needed, so whole (20% from 64 MiB down), at its rank, 1.
    assert_eq!(
        shape(&plan),
        vec![
            (0, 0, vec![(0, 4)]),
            (1, 1, vec![(0, 8)]),
            (0, 2, vec![(4000, 4)]),
        ]
    );
}

#[test]
fn the_last_short_cell_is_a_cell() {
    // 100.5 MiB: 101 cells, the last one half a MiB.
    let list = support::FakeList {
        packs: vec![pack("p0", 100 * MIB + MIB / 2)],
    };
    let plan = Case::new(&list, vec![bytes(0, 100 * MIB + MIB / 2 - 1, 1)]).plan();
    assert_eq!(shape(&plan), vec![(0, 0, vec![(100, 1)])]);
    assert_eq!(plan.packs[0].cell_count, 101);
}

#[test]
fn nothing_needed_is_an_empty_plan() {
    let list = list_mib(&[100]);
    assert_eq!(Case::new(&list, vec![]).plan(), Plan::default());
    // An empty range needs no cell.
    let case = Case::new(&list, vec![bytes(0, 5 * MIB, 0)]);
    assert_eq!(case.plan(), Plan::default());
}

#[test]
fn the_plan_reports_each_packs_counts() {
    let list = list_mib(&[100, 8]);
    let mut case = Case::new(
        &list,
        vec![cells(0, 10, 1), cells(0, 14, 1), cells(1, 0, 1)],
    );
    case.media.insert(1, Medium::Hot);
    let plan = case.plan();
    assert_eq!(
        plan.packs,
        vec![
            PackPlan {
                pack: 0,
                cell_count: 100,
                medium: Medium::Cold,
                gap_cells: 3,
                needed_cells: 2,
                merged_cells: 5,
                fetch_cells: 5,
                fetch: Fetch::Runs,
            },
            PackPlan {
                pack: 1,
                cell_count: 8,
                medium: Medium::Hot,
                gap_cells: 0,
                needed_cells: 1,
                merged_cells: 1,
                fetch_cells: 1,
                fetch: Fetch::Runs,
            },
        ]
    );
}

#[test]
fn the_same_input_gives_the_same_plan_and_tree_order_only_moves_ranks() {
    let list = list_mib(&[100, 100, 8]);
    let needed = vec![
        cells(0, 3, 2),
        cells(1, 40, 1),
        cells(2, 1, 3),
        cells(0, 60, 1),
    ];
    let mut case = Case::new(&list, needed.clone());
    let first = case.plan();
    assert_eq!(first, case.plan());

    let mut reversed = needed;
    reversed.reverse();
    case.needed = reversed;
    let second = case.plan();
    let cells_of = |plan: &Plan| per_cell(plan).into_keys().collect::<Vec<_>>();
    assert_eq!(cells_of(&first), cells_of(&second));
    assert_eq!(first.packs, second.packs);
    assert_ne!(shape(&first), shape(&second));
}

#[test]
fn the_input_debug_names_what_it_plans_from() {
    let list = list_mib(&[1]);
    let needed = [cells(0, 0, 1)];
    let media = std::collections::BTreeMap::new();
    let input = tierstore_plan::plan::PlanInput {
        list: &list,
        needed: &needed,
        media: &media,
        backlog: tierstore_plan::plan::Backlog::default(),
    };
    let text = format!("{input:?}");
    assert!(
        text.starts_with("PlanInput { needed: [Needed { pack: 0"),
        "{text}"
    );
    assert!(
        text.ends_with("backlog: Backlog { queued_bytes: 0, bytes_per_s: 0 }, .. }"),
        "{text}"
    );
    // The fake list's other methods are not what the planner reads.
    use tierstore_fetch_api::list::{ListL2, NodeIndex};
    assert_eq!(list.pack_count(), 1);
    assert_eq!(list.cell_digest(0, 0), None);
    assert_eq!(list.pack_root(0), None);
    assert_eq!(list.dir_lookup(NodeIndex(0), b"x"), None);
}
