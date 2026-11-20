//! The result document of the FUSE benchmark, `docs/measurements/2026-11-fuse-results.md`, holds
//! its two generated tables word for word: what `fsbench summary` makes of the run records of
//! the baseline, V3/V4 and V5 measurements, and what `fsbench dstate-report` makes of the
//! real-build run's index and the nodes' sampler texts (`data/2026-11-real-build`). No figure of
//! those tables is typed by hand, and a change to either report or to the data that would change
//! the document fails here until the document is made again.

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
    std::fs::read_to_string(crate_dir().join("../../docs/measurements/2026-11-fuse-results.md"))
        .unwrap()
}

fn data(name: &str) -> PathBuf {
    crate_dir().join("data").join(name)
}

/// The three measurements of the run records, in the order of the design's variants: the
/// baseline (V1, V2 and the null control), V3 and V4, and V5 with its controls.
const MEASUREMENTS: [&str; 3] = [
    "2026-11-fuse-baseline",
    "2026-11-fuse-v3-v4",
    "2026-11-fuse-v5",
];

/// The document holds `fsbench summary` of the three measurements, the first run of each series
/// left out, as each measurement's own document leaves it out.
#[test]
fn the_document_holds_the_summary_of_every_measurement() {
    let files: Vec<(String, Vec<fsbench::record::RunRecord>)> = MEASUREMENTS
        .iter()
        .map(|name| {
            let path = data(&format!("{name}.jsonl"));
            ((*name).to_owned(), fsbench::report::read(&path).unwrap())
        })
        .collect();
    let table = fsbench::summary::render(&files, 1);
    assert!(
        doc().contains(&table),
        "the document does not hold the summary of the records; make it again with \
         `fsbench summary --results crates/fsbench/data/2026-11-fuse-baseline.jsonl --results \
         crates/fsbench/data/2026-11-fuse-v3-v4.jsonl --results \
         crates/fsbench/data/2026-11-fuse-v5.jsonl`:\n{table}"
    );
    // 7 series of the baseline, 30 of V3 and V4, 8 of V5 and its controls.
    assert_eq!(table.lines().count(), 2 + 7 + 30 + 8);
}

/// The document holds `fsbench dstate-report` of the real-build run: every run sampled every
/// node from before the build to after it, on the same kernel and with the mount of
/// `tierstore fs` listed, and the build ran.
#[test]
fn the_document_holds_the_report_of_the_real_build_run() {
    let runs = fsbench::dstate::load(&data("2026-11-real-build/runs.json")).unwrap();
    let report = fsbench::dstaterep::render(&runs);
    assert!(
        doc().contains(&report),
        "the document does not hold the report of the real-build run; make it again with \
         `fsbench dstate-report --index crates/fsbench/data/2026-11-real-build/runs.json`:\n\
         {report}"
    );
    assert!(!runs.is_empty());
    for (entry, nodes) in &runs {
        assert!(entry.build_end_ns > entry.build_start_ns, "{}", entry.run);
        assert!(entry.settings.contains_key("targets"), "{}", entry.run);
        assert!(entry.settings.contains_key("commands"), "{}", entry.run);
        assert!(!nodes.is_empty(), "{}", entry.run);
        for node in nodes {
            let label = format!("{} {}", entry.run, node.node);
            assert_eq!(node.connections.len(), 1, "{label}");
            let first = node.ticks.first().unwrap();
            let last = node.ticks.last().unwrap();
            assert!(first.t_ms < 0, "{label}: no tick before the build");
            assert!(
                u64::try_from(last.t_ms).unwrap() > node.build_ms,
                "{label}: no tick after the build"
            );
        }
    }
}
