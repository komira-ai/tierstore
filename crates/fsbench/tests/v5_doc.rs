//! The measurement document of V5 and the storm, `docs/measurements/2026-11-fuse-v5-storm.md`,
//! holds the tables of its raw records word for word: what `fsbench report` makes of
//! `data/2026-11-fuse-v5.jsonl`, and what `fsbench storm-report` makes of
//! `data/2026-11-fuse-storm.jsonl`. No figure of the document is typed by hand, and a change to
//! either report that would change the document fails here until the document is made again.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::path::{Path, PathBuf};

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn doc() -> String {
    std::fs::read_to_string(crate_dir().join("../../docs/measurements/2026-11-fuse-v5-storm.md"))
        .unwrap()
}

fn data(name: &str) -> PathBuf {
    crate_dir().join("data").join(name)
}

/// The V5 series and their same-day `/dev/fuse` controls, in the order they ran: in each of the
/// two regimes of the design, V5 at depth 5 and 16 and V3 with 8 and 22 readers, interleaved;
/// six runs each, none of whose cycles or reads failed; every V5 run served on io_uring.
#[test]
fn the_document_holds_the_report_of_the_v5_records() {
    let records = fsbench::report::read(&data("2026-11-fuse-v5.jsonl")).unwrap();
    let report = fsbench::report::render(&records, 1);
    assert!(
        doc().contains(&report),
        "the document does not hold the report of its records; make it again with \
         `fsbench report --results crates/fsbench/data/2026-11-fuse-v5.jsonl`:\n{report}"
    );
    let mut series: Vec<(String, String, usize)> = Vec::new();
    for record in &records {
        assert_eq!(
            (record.cycles.errors, record.cold.errors, record.warm.errors),
            (0, 0, 0)
        );
        let uring = record.variant.starts_with("v5");
        assert_eq!(
            record.settings.get("transport").map(String::as_str),
            Some(if uring { "uring" } else { "dev" })
        );
        assert_eq!(
            record
                .env
                .fuse_parameters
                .get("enable_uring")
                .map(String::as_str),
            Some("Y")
        );
        match series
            .iter_mut()
            .find(|(v, r, _)| (v, r) == (&record.variant, &record.regime))
        {
            Some(found) => found.2 += 1,
            None => series.push((record.variant.clone(), record.regime.clone(), 1)),
        }
    }
    let names: Vec<String> = series
        .iter()
        .map(|(v, r, n)| format!("{v} {r} {n}"))
        .collect();
    let mut expected = Vec::new();
    for regime in ["caches-off", "no-open"] {
        for variant in ["v5-d5", "v3-r8", "v5-d16", "v3-r22"] {
            expected.push(format!("{variant} {regime} 6"));
        }
    }
    assert_eq!(names, expected);
}

/// The storm's series, in the order they ran, three runs each: io_uring at depth 5 (parks of 60
/// and 10 ms, buffered and direct reads) and 16 (the storm of its depth, and the storm of depth
/// 5), and `/dev/fuse` with 8 readers under the same storms; every io_uring series served on
/// io_uring, and no storm thread and no probe failed.
#[test]
fn the_document_holds_the_report_of_the_storm_records() {
    let records = fsbench::stormrec::read(&data("2026-11-fuse-storm.jsonl")).unwrap();
    let report = fsbench::stormrec::render(&records, 0);
    assert!(
        doc().contains(&report),
        "the document does not hold the report of its records; make it again with \
         `fsbench storm-report --results crates/fsbench/data/2026-11-fuse-storm.jsonl`:\n{report}"
    );
    let mut labels: Vec<(String, usize)> = Vec::new();
    for record in &records {
        let uring = record.label.starts_with("uring");
        assert_eq!(record.transport == "io_uring", uring, "{}", record.label);
        assert_eq!(record.phases.len(), 3);
        for phase in &record.phases {
            assert_eq!(phase.storm_errors, 0, "{} {}", record.label, phase.name);
            assert!(phase.probes.iter().all(|p| p.errors == 0));
        }
        match labels.iter_mut().find(|(l, _)| l == &record.label) {
            Some(found) => found.1 += 1,
            None => labels.push((record.label.clone(), 1)),
        }
    }
    let names: Vec<String> = labels.iter().map(|(l, n)| format!("{l} {n}")).collect();
    assert_eq!(
        names,
        [
            "uring-d5-p60 3",
            "dev-r8-p60 3",
            "uring-d5-p10 3",
            "dev-r8-p10 3",
            "uring-d5-p60-direct 3",
            "uring-d16-p60 3",
            "uring-d16-p60-s8 3",
        ]
    );
}
