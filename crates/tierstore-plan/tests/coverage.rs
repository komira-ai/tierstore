//! The whole-pack rule, budget-aware (section 6 step 4): `cov`, the merged cells over the pack's
//! cells, of 50% fetches the whole pack, and of 20% for a pack of 64 MiB or less; a backlog of
//! more than 2 s of budget throughput downgrades a would-be-whole pack to its runs.
//!
//! Catches:
//! - an off-by-one at either coverage threshold or at the 64 MiB size bound;
//! - coverage computed before merging, so gap cells never count;
//! - a whole pack whose fill cells are missing, ensured twice, or ensured at a rank that puts a
//!   pack's unneeded head before another pack's needed cells;
//! - a backlog that never downgrades, downgrades at exactly 2 s, or downgrades a pack that was
//!   only runs anyway.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod support;

use support::{Case, MIB, cells, list_mib, pack, shape};
use tierstore_plan::config::Medium;
use tierstore_plan::plan::{Backlog, Fetch};

fn fetch_of(case: &Case<'_>) -> (Fetch, u32) {
    let plan = case.plan();
    (plan.packs[0].fetch, plan.packs[0].fetch_cells)
}

#[test]
fn a_large_pack_half_needed_is_fetched_whole_and_one_cell_less_is_not() {
    let list = list_mib(&[100]);
    let half = Case::new(&list, vec![cells(0, 0, 50)]);
    assert_eq!(fetch_of(&half), (Fetch::Whole, 100));
    assert_eq!(shape(&half.plan()), vec![(0, 0, vec![(0, 100)])]);
    let under = Case::new(&list, vec![cells(0, 0, 49)]);
    assert_eq!(fetch_of(&under), (Fetch::Runs, 49));
}

#[test]
fn coverage_counts_the_gap_cells_the_drive_reads_through() {
    let list = list_mib(&[100]);
    // 47 cells needed; on a cold drive the gap 45..48 is read too, so 50 cells are merged.
    let needed = vec![cells(0, 0, 45), cells(0, 48, 2)];
    let cold = Case::new(&list, needed.clone());
    assert_eq!(fetch_of(&cold), (Fetch::Whole, 100));
    let mut hot = Case::new(&list, needed);
    hot.media.insert(0, Medium::Hot);
    assert_eq!(fetch_of(&hot), (Fetch::Runs, 47));
}

#[test]
fn a_small_pack_is_fetched_whole_from_a_fifth() {
    let list = list_mib(&[10]);
    let fifth = Case::new(&list, vec![cells(0, 0, 2)]);
    assert_eq!(fetch_of(&fifth), (Fetch::Whole, 10));
    let tenth = Case::new(&list, vec![cells(0, 0, 1)]);
    assert_eq!(fetch_of(&tenth), (Fetch::Runs, 1));
}

#[test]
fn a_pack_of_exactly_64_mib_is_small_and_one_byte_more_is_not() {
    let list = list_mib(&[64]);
    // 13 of 64 cells is 20.3%, 12 is 18.75%.
    assert_eq!(
        fetch_of(&Case::new(&list, vec![cells(0, 0, 13)])),
        (Fetch::Whole, 64)
    );
    assert_eq!(
        fetch_of(&Case::new(&list, vec![cells(0, 0, 12)])),
        (Fetch::Runs, 12)
    );
    // 64 MiB + 1 byte is 65 cells: 13 of them is exactly 20%, but the pack is not small.
    let big = support::FakeList {
        packs: vec![pack("p0", 64 * MIB + 1)],
    };
    assert_eq!(
        fetch_of(&Case::new(&big, vec![cells(0, 0, 13)])),
        (Fetch::Runs, 13)
    );
}

#[test]
fn a_whole_pack_head_takes_the_first_runs_rank_and_the_rest_the_run_before() {
    let list = list_mib(&[10]);
    // Cells 4 (rank 0) and 7 (rank 1); the gap 5..7 reads with cell 4.
    let plan = Case::new(&list, vec![cells(0, 4, 1), cells(0, 7, 1)]).plan();
    assert_eq!(plan.packs[0].fetch, Fetch::Whole);
    assert_eq!(
        shape(&plan),
        vec![(0, 0, vec![(0, 7)]), (0, 1, vec![(7, 3)])]
    );
}

#[test]
fn a_whole_pack_keeps_a_later_cells_lower_rank() {
    let list = list_mib(&[10]);
    let mut case = Case::new(&list, vec![cells(0, 6, 1), cells(0, 1, 1)]);
    case.media.insert(0, Medium::Hot);
    // Cell 6 is rank 0, cell 1 rank 1: the head 0..6 goes with cell 1, the tail with cell 6.
    assert_eq!(
        shape(&case.plan()),
        vec![(0, 0, vec![(6, 4)]), (0, 1, vec![(0, 6)])]
    );
}

#[test]
fn a_pack_needed_entirely_is_whole() {
    let list = list_mib(&[300]);
    assert_eq!(
        fetch_of(&Case::new(&list, vec![cells(0, 0, 300)])),
        (Fetch::Whole, 300)
    );
}

#[test]
fn a_backlog_over_two_seconds_downgrades_a_whole_pack_to_its_runs() {
    let list = list_mib(&[100]);
    let needed = vec![cells(0, 0, 10), cells(0, 20, 50)];
    let mut case = Case::new(&list, needed);

    case.backlog = Backlog {
        queued_bytes: 200,
        bytes_per_s: 100,
    };
    assert_eq!(fetch_of(&case), (Fetch::Whole, 100), "exactly 2 s");

    case.backlog.queued_bytes = 201;
    let plan = case.plan();
    assert_eq!(plan.packs[0].fetch, Fetch::RunsUnderBacklog);
    assert_eq!(plan.packs[0].merged_cells, 60);
    assert_eq!(
        shape(&plan),
        vec![(0, 0, vec![(0, 10)]), (0, 1, vec![(20, 50)])]
    );
}

#[test]
fn bytes_queued_with_no_throughput_downgrade_and_no_bytes_queued_never_do() {
    let list = list_mib(&[100]);
    let mut case = Case::new(&list, vec![cells(0, 0, 60)]);
    case.backlog = Backlog {
        queued_bytes: 1,
        bytes_per_s: 0,
    };
    assert_eq!(fetch_of(&case), (Fetch::RunsUnderBacklog, 60));
    case.backlog.queued_bytes = 0;
    assert_eq!(fetch_of(&case), (Fetch::Whole, 100));
}

#[test]
fn a_backlog_leaves_a_pack_of_runs_as_runs() {
    let list = list_mib(&[100]);
    let mut case = Case::new(&list, vec![cells(0, 0, 10)]);
    case.backlog = Backlog {
        queued_bytes: u64::MAX,
        bytes_per_s: 1,
    };
    assert_eq!(fetch_of(&case), (Fetch::Runs, 10));
}

#[test]
fn the_backlog_bound_is_compared_without_overflow() {
    let full = Backlog {
        queued_bytes: u64::MAX,
        bytes_per_s: u64::MAX,
    };
    assert!(!full.exceeds(1_000));
    assert!(full.exceeds(999));
    assert!(!Backlog::default().exceeds(0));
}
