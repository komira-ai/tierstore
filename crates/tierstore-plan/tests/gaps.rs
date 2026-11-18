//! The per-drive gap rule (section 6 step 3): a pack's needed runs merge across at most
//! `gap_cells` absent cells of the device the pack is read from, and the gap's cells are ensured
//! with the read they continue.
//!
//! Catches:
//! - a cold drive paying a positioning to skip fewer cells than it streams in that time;
//! - a gap one cell wider than the rule read through;
//! - a hot copy read through a gap, which only adds bytes;
//! - one gap rule for every pack, whatever its device;
//! - gap cells ensured at a rank other than the read they continue, or left out of the plan.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod support;

use support::{Case, cells, list_mib, per_cell, shape};
use tierstore_plan::config::{DriveModel, Medium, PlanConfig};
use tierstore_plan::plan::Fetch;

#[test]
fn a_cold_drive_reads_through_three_absent_cells_but_not_four() {
    let list = list_mib(&[100]);
    let plan = Case::new(
        &list,
        vec![cells(0, 10, 1), cells(0, 14, 1), cells(0, 19, 1)],
    )
    .plan();
    // Cells 11..14 are a gap of 3 and are read with cell 10; 15..19 is a gap of 4.
    assert_eq!(
        shape(&plan),
        vec![
            (0, 0, vec![(10, 4)]),
            (0, 1, vec![(14, 1)]),
            (0, 2, vec![(19, 1)]),
        ]
    );
    assert_eq!(plan.packs[0].needed_cells, 3);
    assert_eq!(plan.packs[0].merged_cells, 6);
    assert_eq!(plan.packs[0].fetch_cells, 6);
    assert_eq!(plan.packs[0].fetch, Fetch::Runs);
}

#[test]
fn a_hot_copy_reads_through_no_gap() {
    let list = list_mib(&[100]);
    let mut case = Case::new(
        &list,
        vec![cells(0, 10, 1), cells(0, 12, 1), cells(0, 13, 1)],
    );
    case.media.insert(0, Medium::Hot);
    let plan = case.plan();
    // Cell 11 is a gap of one and is not read; 12 and 13 touch, at their own ranks.
    assert_eq!(
        shape(&plan),
        vec![
            (0, 0, vec![(10, 1)]),
            (0, 1, vec![(12, 1)]),
            (0, 2, vec![(13, 1)]),
        ]
    );
    assert_eq!(plan.packs[0].gap_cells, 0);
    assert_eq!(plan.packs[0].fetch_cells, 3);
}

#[test]
fn gap_cells_take_the_rank_of_the_run_before_them() {
    let list = list_mib(&[100]);
    // Cell 14 is needed first, cell 10 second; the gap 11..14 continues the read of cell 10.
    let plan = Case::new(&list, vec![cells(0, 14, 1), cells(0, 10, 1)]).plan();
    assert_eq!(
        shape(&plan),
        vec![(0, 0, vec![(14, 1)]), (0, 1, vec![(10, 4)])]
    );
}

#[test]
fn every_gap_cell_is_ensured_exactly_once() {
    let list = list_mib(&[100]);
    let plan = Case::new(
        &list,
        vec![
            cells(0, 0, 1),
            cells(0, 4, 1),
            cells(0, 8, 2),
            cells(0, 13, 1),
        ],
    )
    .plan();
    let got: Vec<u32> = per_cell(&plan).into_keys().map(|(_, c)| c).collect();
    assert_eq!(got, (0..14).collect::<Vec<_>>());
}

#[test]
fn the_now_profile_reads_through_two_cells_on_its_smr_drives() {
    let list = list_mib(&[100]);
    let mut case = Case::new(
        &list,
        vec![cells(0, 10, 1), cells(0, 13, 1), cells(0, 17, 1)],
    );
    case.config = PlanConfig {
        cold: DriveModel {
            positioning_us: 15_574,
            media_mb_per_s: 119,
        },
        ..PlanConfig::default()
    };
    // 11..13 is a gap of 2, read; 14..17 a gap of 3, not read.
    assert_eq!(
        shape(&case.plan()),
        vec![
            (0, 0, vec![(10, 3)]),
            (0, 1, vec![(13, 1)]),
            (0, 2, vec![(17, 1)]),
        ]
    );
}

#[test]
fn each_pack_takes_the_gap_rule_of_its_own_device() {
    let list = list_mib(&[100, 100, 100]);
    let mut case = Case::new(
        &list,
        vec![
            cells(0, 10, 1),
            cells(0, 13, 1),
            cells(1, 10, 1),
            cells(1, 13, 1),
            cells(2, 10, 1),
            cells(2, 13, 1),
        ],
    );
    // Pack 0 is on a hot copy, pack 1 on a drive, pack 2's location is not known: cold.
    case.media.insert(0, Medium::Hot);
    case.media.insert(1, Medium::Cold);
    let plan = case.plan();
    let fetched: Vec<(u32, Medium, u32)> = plan
        .packs
        .iter()
        .map(|p| (p.pack, p.medium, p.fetch_cells))
        .collect();
    assert_eq!(
        fetched,
        vec![
            (0, Medium::Hot, 2),
            (1, Medium::Cold, 4),
            (2, Medium::Cold, 4)
        ]
    );
}

#[test]
fn a_gap_never_crosses_into_another_pack() {
    let list = list_mib(&[100, 100]);
    let plan = Case::new(&list, vec![cells(0, 99, 1), cells(1, 0, 1)]).plan();
    assert_eq!(
        shape(&plan),
        vec![(0, 0, vec![(99, 1)]), (1, 1, vec![(0, 1)])]
    );
}
