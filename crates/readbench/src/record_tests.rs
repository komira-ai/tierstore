use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::{
    ClientRun, DemandRead, DiskSample, MetricDelta, Mode, PlanRecord, Summary, nearest_rank,
    read_lines, write_line,
};

fn run(needed: u64, fetched: u64, gets: &[(&str, u64)]) -> ClientRun {
    ClientRun {
        run: "r".into(),
        mode: Mode::Engine,
        tree: "ab".into(),
        client: 0,
        clients: 1,
        node: "n1".into(),
        manifests: 1,
        files: 1,
        packs: 1,
        bytes_needed: needed,
        bytes_fetched: fetched,
        gets: gets.iter().map(|(k, n)| ((*k).to_owned(), *n)).collect(),
        tree_ready_us: 10,
        first_byte_us: Summary::of(&[1]),
        get_us: Summary::of(&[2]),
        get_errors: 0,
        config: BTreeMap::new(),
    }
}

fn demand(wait_us: u64) -> DemandRead {
    DemandRead {
        run: "r".into(),
        node: "n1".into(),
        load_clients: 2,
        len: 4,
        wait_us,
        outcome: "ready".into(),
    }
}

#[test]
fn a_summary_of_no_value_is_all_zero() {
    assert_eq!(Summary::of(&[]), Summary::default());
}

#[test]
fn a_summary_takes_nearest_rank_percentiles_whatever_the_order() {
    let values: Vec<u64> = (1..=100).rev().collect();
    assert_eq!(
        Summary::of(&values),
        Summary {
            n: 100,
            p50: 50,
            p99: 99,
            max: 100
        }
    );
    assert_eq!(
        Summary::of(&[7]),
        Summary {
            n: 1,
            p50: 7,
            p99: 7,
            max: 7
        }
    );
}

#[test]
fn nearest_rank_stays_inside_the_values() {
    let v = [1, 2, 3];
    assert_eq!(nearest_rank(&v, 0), 1);
    assert_eq!(nearest_rank(&v, 50), 2);
    assert_eq!(nearest_rank(&v, 100), 3);
    assert_eq!(nearest_rank(&v, 1000), 3);
}

#[test]
fn amplification_is_fetched_over_needed_in_thousandths() {
    assert_eq!(run(100, 250, &[]).amplification_milli(), 2500);
    assert_eq!(run(0, 250, &[]).amplification_milli(), 0);
}

#[test]
fn total_gets_sums_every_kind() {
    assert_eq!(run(1, 1, &[("grid_part", 3), ("hedge", 2)]).total_gets(), 5);
    assert_eq!(run(1, 1, &[]).total_gets(), 0);
}

#[test]
fn records_written_as_lines_are_read_back_skipping_blank_lines() {
    let dir = TestDir::under(std::env::temp_dir(), "readbench-record");
    let path = dir.path().join("runs.jsonl");
    let mut out = Vec::new();
    write_line(&mut out, &path, &demand(5)).unwrap();
    out.extend_from_slice(b"\n  \n");
    write_line(&mut out, &path, &demand(9)).unwrap();
    std::fs::write(&path, &out).unwrap();
    let back: Vec<DemandRead> = read_lines(&path).unwrap();
    assert_eq!(back, vec![demand(5), demand(9)]);
    let mut runs = Vec::new();
    write_line(&mut runs, &path, &run(1, 2, &[("retry", 1)])).unwrap();
    assert!(String::from_utf8(runs).unwrap().ends_with("}\n"));
}

#[test]
fn a_plan_record_is_one_line_of_the_named_fields_and_reads_back() {
    let record = PlanRecord {
        run: "r".into(),
        tree: "t".into(),
        variant: "design".into(),
        files: 1,
        packs: 2,
        bytes_needed: 3,
        bytes_planned: 4,
        packs_whole: 1,
        packs_runs: 1,
        ensures: 2,
    };
    let mut line = Vec::new();
    write_line(&mut line, Path::new("plans"), &record).unwrap();
    assert_eq!(
        String::from_utf8(line.clone()).unwrap(),
        "{\"run\":\"r\",\"tree\":\"t\",\"variant\":\"design\",\"files\":1,\"packs\":2,\
         \"bytes_needed\":3,\"bytes_planned\":4,\"packs_whole\":1,\"packs_runs\":1,\"ensures\":2}\n"
    );
    let dir = TestDir::under(std::env::temp_dir(), "readbench-record-plan");
    let path = dir.path().join("plans");
    std::fs::write(&path, &line).unwrap();
    assert_eq!(read_lines::<PlanRecord>(&path).unwrap(), [record]);
}

#[test]
fn a_line_that_is_not_a_record_is_named_by_file_and_line() {
    let dir = TestDir::under(std::env::temp_dir(), "readbench-record-bad");
    let path = dir.path().join("demand.jsonl");
    std::fs::write(&path, "\n{\"nope\":1}\n").unwrap();
    let err = read_lines::<DemandRead>(&path).unwrap_err();
    assert!(
        err.to_string()
            .starts_with(&format!("{}:2: ", path.display())),
        "{err}"
    );
}

#[test]
fn a_missing_results_file_is_named() {
    let dir = TestDir::under(std::env::temp_dir(), "readbench-record-missing");
    let path = dir.path().join("none.jsonl");
    let err = read_lines::<DemandRead>(&path).unwrap_err();
    assert!(
        err.to_string()
            .starts_with(&format!("open {}: ", path.display())),
        "{err}"
    );
}

#[test]
fn a_file_that_is_not_text_fails_the_read_of_its_line() {
    let dir = TestDir::under(std::env::temp_dir(), "readbench-record-binary");
    let path = dir.path().join("binary.jsonl");
    std::fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
    let err = read_lines::<DemandRead>(&path).unwrap_err();
    assert!(
        err.to_string()
            .starts_with(&format!("read {}: ", path.display())),
        "{err}"
    );
}

struct Refuses;

impl Write for Refuses {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("disk full"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_write_that_fails_names_the_file() {
    let err = write_line(&mut Refuses, Path::new("out.jsonl"), &demand(1)).unwrap_err();
    assert_eq!(err.to_string(), "write out.jsonl: disk full");
}

/// A record that fails to serialize is reported as a parse failure naming the file.
#[test]
fn a_record_that_cannot_be_serialized_names_the_file() {
    struct Bad;
    impl serde::Serialize for Bad {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("no"))
        }
    }
    let mut out = Vec::new();
    let err = write_line(&mut out, Path::new("o.jsonl"), &Bad).unwrap_err();
    assert_eq!(err.to_string(), "o.jsonl: no");
    assert!(out.is_empty());
}

#[test]
fn modes_are_written_in_snake_case() {
    assert_eq!(
        serde_json::to_string(&Mode::Shipped).unwrap(),
        "\"shipped\""
    );
    assert_eq!(
        serde_json::from_str::<Mode>("\"engine\"").unwrap(),
        Mode::Engine
    );
}

/// Every kind of record survives a trip through a results file.
#[test]
fn every_kind_of_record_is_read_back_as_written() {
    let dir = TestDir::under(std::env::temp_dir(), "readbench-record-kinds");
    let path = dir.path().join("all.jsonl");
    let client = run(100, 250, &[("grid_part", 3)]);
    let disk = DiskSample {
        run: "r".into(),
        node: "n1".into(),
        device: "sda".into(),
        t_ms: 1000,
        reads: 5,
        rkb_s: 1.5,
        r_await_ms: 2.5,
        aqu_sz: 0.5,
        util_pct: 12.5,
    };
    let delta = MetricDelta {
        run: "r".into(),
        node: "n1".into(),
        name: "gets_total".into(),
        labels: "kind=\"x\"".into(),
        delta: 7.0,
        window_ms: 1000,
    };
    let mut out = Vec::new();
    write_line(&mut out, &path, &client).unwrap();
    write_line(&mut out, &path, &disk).unwrap();
    write_line(&mut out, &path, &delta).unwrap();
    // Each kind is read from a file of its own lines.
    for (name, line) in ["client", "disk", "delta"]
        .iter()
        .zip(out.split_inclusive(|b| *b == b'\n'))
    {
        std::fs::write(dir.path().join(name), line).unwrap();
    }
    assert_eq!(
        read_lines::<ClientRun>(&dir.path().join("client")).unwrap(),
        vec![client]
    );
    assert_eq!(
        read_lines::<DiskSample>(&dir.path().join("disk")).unwrap(),
        vec![disk]
    );
    assert_eq!(
        read_lines::<MetricDelta>(&dir.path().join("delta")).unwrap(),
        vec![delta]
    );
}
