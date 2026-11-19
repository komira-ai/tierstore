//! The measurement document of T0-F-a holds the tables of its summaries: what `storebench report`
//! makes of each arm's summary in `data/` is in
//! `docs/measurements/2026-11-storage-tier-t0-f-a.md` word for word, so no figure of the
//! document is typed by hand, and a change to a table that would change the document fails here
//! until the document is made again.

#![expect(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::path::Path;

use storebench::report::{Arm, missing_arms};

/// The document as it is checked in.
fn document() -> String {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(
        crate_dir.join("../../docs/measurements/2026-11-storage-tier-t0-f-a.md"),
    )
    .unwrap()
}

/// Each arm's tables, as `storebench report --data crates/storebench/data --arm <arm>` prints
/// them, are in the document (`missing_arms` is the check).
#[test]
fn the_document_holds_the_tables_of_every_arm() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    let missing = missing_arms(&document(), &data).unwrap();
    assert!(
        missing.is_empty(),
        "the document does not hold the tables of {missing:?}; make them again with \
         `storebench report --data crates/storebench/data --arm <arm>`"
    );
}

/// The check fails on a document that differs from the tables: a figure changed in one row of
/// C-6 makes C-6 missing and only C-6; an empty document misses every arm.
#[test]
fn a_document_with_a_changed_figure_does_not_hold_the_tables() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    let doc = document();
    let changed = doc.replacen("| pack | 866,682 |", "| pack | 866,683 |", 1);
    assert_ne!(changed, doc, "the row to change is in the document");
    assert_eq!(missing_arms(&changed, &data).unwrap(), [Arm::C6]);
    assert_eq!(
        missing_arms("", &data).unwrap(),
        [Arm::C6, Arm::C7, Arm::C10]
    );
}
