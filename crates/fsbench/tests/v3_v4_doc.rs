//! The measurement document of V3 and V4, `docs/measurements/2026-11-fuse-v3-v4.md`, holds the
//! report of its raw records word for word: what `fsbench report` makes of
//! `data/2026-11-fuse-v3-v4.jsonl`. No figure of the document is typed by hand, and a change to
//! the report that would change the document fails here until the document is made again.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::path::Path;

/// The document holds the report of its records, and the records are its series in the order
/// they ran: in each regime the same-day control on `fuser`, then V3 (`tsfuse` on `/dev/fuse`,
/// replies by `writev`) at 1, 2, 4, 8 and 22 readers, then, in the two regimes of the design, V4
/// (replies by splice) at the same reader counts and V4 with `SPLICE_F_MOVE` at 8; six runs each,
/// none of whose cycles or reads failed, every one with the CPU of its whole-file phases.
#[test]
fn the_document_holds_the_report_of_the_v3_v4_records() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let records = fsbench::report::read(&crate_dir.join("data/2026-11-fuse-v3-v4.jsonl")).unwrap();
    let doc =
        std::fs::read_to_string(crate_dir.join("../../docs/measurements/2026-11-fuse-v3-v4.md"))
            .unwrap();
    let report = fsbench::report::render(&records, 1);
    assert!(
        doc.contains(&report),
        "the document does not hold the report of its records; make it again with \
         `fsbench report --results crates/fsbench/data/2026-11-fuse-v3-v4.jsonl`:\n{report}"
    );
    let mut series: Vec<(String, String, usize)> = Vec::new();
    for record in &records {
        assert_eq!(
            (record.cycles.errors, record.cold.errors, record.warm.errors),
            (0, 0, 0)
        );
        assert!(record.cold.cpu.is_some() && record.warm.cpu.is_some());
        match series
            .iter_mut()
            .find(|(v, r, _)| (v, r) == (&record.variant, &record.regime))
        {
            Some(found) => found.2 += 1,
            None => series.push((record.variant.clone(), record.regime.clone(), 1)),
        }
    }
    let mut expected = Vec::new();
    for regime in ["caches-off", "caches-on", "no-open"] {
        expected.push(format!("null {regime} 6"));
        let readers = [1, 2, 4, 8, 22];
        expected.extend(readers.map(|n| format!("v3-r{n} {regime} 6")));
        if regime != "caches-on" {
            expected.extend(readers.map(|n| format!("v4-r{n} {regime} 6")));
            expected.push(format!("v4m-r8 {regime} 6"));
        }
    }
    let names: Vec<String> = series
        .iter()
        .map(|(v, r, n)| format!("{v} {r} {n}"))
        .collect();
    assert_eq!(names, expected);
}
