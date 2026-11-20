//! The results document of the farm validation, `docs/measurements/2026-11-t10b.md`, holds its
//! tables word for word: what `readbench report` makes of the records in
//! `data/2026-11-t10b/`. No figure of those tables is typed by hand, and a change to the report
//! or to the records that would change the document fails here until the document is made again.
//! The records themselves are checked for what the document says of them: every run finished
//! with no failed GET, every demand read came back ready, and the two kinds of run are told
//! apart by their configuration.

#![cfg(target_os = "linux")]
#![expect(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use readbench::record::{ClientRun, DemandRead, DiskSample, MetricDelta, PlanRecord, read_lines};
use readbench::report;

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn doc() -> String {
    std::fs::read_to_string(crate_dir().join("../../docs/measurements/2026-11-t10b.md")).unwrap()
}

fn data(name: &str) -> PathBuf {
    crate_dir().join("data/2026-11-t10b").join(name)
}

fn runs() -> Vec<ClientRun> {
    read_lines(&data("runs.jsonl")).unwrap()
}

/// The drive classes of the document: the two SMR drives and the root SSD of each node.
fn classes() -> Vec<(String, Vec<(String, String)>)> {
    [
        "hdd=n2:sdb,n2:sdc,n3:sdb,n3:sdc,n4:sda,n4:sdc,n5:sda,n5:sdc,n6:sda,n6:sdc",
        "ssd=n2:sda,n3:sda,n4:sdb,n5:sdb,n6:sdb",
    ]
    .iter()
    .map(|arg| report::parse_class(arg).unwrap())
    .collect()
}

fn assert_holds(what: &str, table: &str) {
    assert!(
        doc().contains(table),
        "the document does not hold the {what} table of the records; make it again with \
         `readbench report` over crates/readbench/data/2026-11-t10b:\n{table}"
    );
}

#[test]
fn the_document_holds_the_table_of_every_run() {
    let runs = runs();
    assert_holds("runs", &report::runs_table(&runs));
    let names: BTreeSet<&str> = runs.iter().map(|r| r.run.as_str()).collect();
    assert_eq!(names.len(), 71);
}

/// The trees table is of the verified runs: a cold run's client has a tree of its own, which a
/// row per tree would only repeat from the runs table.
#[test]
fn the_document_holds_the_trees_of_the_verified_runs() {
    let verified: Vec<ClientRun> = runs()
        .into_iter()
        .filter(|r| r.config.get("verified").map(String::as_str) != Some("false"))
        .collect();
    let table = report::trees_table(&verified);
    assert_holds("trees", &table);
    let trees: BTreeSet<&str> = verified.iter().map(|r| r.tree.as_str()).collect();
    assert_eq!(trees.len(), 5);
}

#[test]
fn the_document_holds_the_demand_reads() {
    let reads: Vec<DemandRead> = read_lines(&data("demand.jsonl")).unwrap();
    assert_holds("demand", &report::demand_table(&reads));
    assert!(reads.iter().all(|r| r.outcome == "ready"));
}

#[test]
fn the_document_holds_the_drives_by_class() {
    let samples: Vec<DiskSample> = read_lines(&data("disks.jsonl")).unwrap();
    assert_holds("drives", &report::drive_classes_table(&samples, &classes()));
}

#[test]
fn the_document_holds_the_nodes_metrics() {
    let deltas: Vec<MetricDelta> = read_lines(&data("metrics.jsonl")).unwrap();
    assert_holds("metrics", &report::metrics_table(&deltas, &[]));
    // The document says the metric gained no sample on any node.
    assert!(
        deltas
            .iter()
            .all(|d| d.name != "tierstore_request_to_start_seconds_count")
    );
}

#[test]
fn the_document_holds_the_plans() {
    let plans: Vec<PlanRecord> = read_lines(&data("plans.jsonl")).unwrap();
    assert_holds("plans", &report::plan_table(&plans));
    let trees: BTreeSet<&str> = plans.iter().map(|p| p.tree.as_str()).collect();
    assert_eq!(trees.len(), 300);
}

/// Every client of every run finished: a failed client wrote no record, and a record's failed
/// GETs are counted; the document says no run had one.
#[test]
fn every_run_finished_with_no_failed_get() {
    let runs = runs();
    assert!(runs.iter().all(|r| r.get_errors == 0));
    let cold = runs
        .iter()
        .filter(|r| r.config.get("verified").map(String::as_str) == Some("false"))
        .count();
    assert_eq!(cold, 16 + 31 * 5);
}
