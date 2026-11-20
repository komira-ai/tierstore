//! The measurement document of the FUSE baseline holds the tables of its raw records: what
//! `fsbench report` makes of `data/2026-11-fuse-baseline.jsonl` is in
//! `docs/measurements/2026-11-fuse-baseline.md` word for word, so no figure of the document is
//! typed by hand, and a change to the report that would change the document fails here until
//! the document is made again. The same holds for the document of the variants V3 and V4,
//! `docs/measurements/2026-11-fuse-v3-v4.md`, and `data/2026-11-fuse-v3-v4.jsonl`.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::path::Path;

/// The document holds the report of its raw records, and the records are the series it
/// describes: four of tierstore fs and three of the null file system, of six runs each, none
/// of whose cycles failed.
#[test]
fn the_baseline_document_holds_the_report_of_its_raw_records() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let records =
        fsbench::report::read(&crate_dir.join("data/2026-11-fuse-baseline.jsonl")).unwrap();
    let doc =
        std::fs::read_to_string(crate_dir.join("../../docs/measurements/2026-11-fuse-baseline.md"))
            .unwrap();
    let report = fsbench::report::render(&records, 1);
    assert!(
        doc.contains(&report),
        "the document does not hold the report of its records; make it again with \
         `fsbench report --results crates/fsbench/data/2026-11-fuse-baseline.jsonl`:\n{report}"
    );
    let mut series: Vec<(String, String, usize)> = Vec::new();
    for record in &records {
        assert_eq!(
            (record.cycles.errors, record.cold.errors, record.warm.errors),
            (0, 0, 0)
        );
        let key = (record.variant.clone(), record.regime.clone());
        match series
            .iter_mut()
            .find(|(v, r, _)| (v, r) == (&key.0, &key.1))
        {
            Some(found) => found.2 += 1,
            None => series.push((key.0, key.1, 1)),
        }
    }
    let names: Vec<String> = series
        .iter()
        .map(|(v, r, n)| format!("{v} {r} {n}"))
        .collect();
    assert_eq!(
        names,
        [
            "v1 caches-on 6",
            "v2 caches-on 6",
            "null caches-off 6",
            "null caches-on 6",
            "null no-open 6",
            "v1-t64 caches-on 6",
            "v2-t64 caches-on 6",
        ]
    );
}

/// The document of the variants V3 and V4 (`tsfuse` on `/dev/fuse`: one reader or N cloned
/// readers, replies by `writev` or by splice) holds the report of its raw records, and the
/// records are its series: in each regime the same-day control on `fuser`, then V3 at 1, 2, 4,
/// 8 and 22 readers, then (in the two regimes of the design) V4 at the same reader counts and
/// V4 with `SPLICE_F_MOVE` at 8; six runs each, none of whose cycles or reads failed.
#[test]
fn the_v3_v4_document_holds_the_report_of_its_raw_records() {
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
