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

use storebench::report::{Arm, render};

/// The document as it is checked in.
fn document() -> String {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(
        crate_dir.join("../../docs/measurements/2026-11-storage-tier-t0-f-a.md"),
    )
    .unwrap()
}

/// Each arm's tables, as `storebench report --data crates/storebench/data --arm <arm>` prints
/// them, are in the document.
#[test]
fn the_document_holds_the_tables_of_every_arm() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    let doc = document();
    for (arm, name) in [(Arm::C6, "c6"), (Arm::C7, "c7"), (Arm::C10, "c10")] {
        let tables = render(&data, arm).unwrap();
        assert!(
            doc.contains(&tables),
            "the document does not hold the tables of {name}; make them again with \
             `storebench report --data crates/storebench/data --arm {name}`:\n{tables}"
        );
    }
}
