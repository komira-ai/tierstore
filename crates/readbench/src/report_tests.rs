use std::collections::BTreeMap;

use super::{
    demand_table, disks_table, drive_classes_table, mean, metrics_table, number, parse_class,
    percentile, plan_table, render, runs_table, trees_table,
};
use crate::record::{ClientRun, DemandRead, DiskSample, MetricDelta, Mode, PlanRecord, Summary};

const TREES_HEADER: &str = "\
| tree | mode | clients | files | packs | manifest GETs | pack GETs (grid/sub/retry/hedge) | needed MiB | fetched MiB | amplification | ready p50 s | ready p99 s | first byte p50 ms | first byte p99 ms | errors |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
";
const DEMAND_HEADER: &str = "\
| run | load clients | read length | reads | wait p50 ms | wait p99 ms | wait max ms |
| --- | --- | --- | --- | --- | --- | --- |
";
const DISKS_HEADER: &str = "\
| run | node | device | intervals | r_await p50 ms | r_await p99 ms | aqu-sz p99 | util mean % | MiB/s mean |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
";
const METRICS_HEADER: &str = "\
| node | series | delta | per second |
| --- | --- | --- | --- |
";

const MIB: u64 = 1_048_576;

/// A client of `tree` with the figures the tests vary; the rest is the same for every client.
#[expect(
    clippy::too_many_arguments,
    reason = "a fixture: every figure the tables read"
)]
fn client(
    tree: &str,
    mode: Mode,
    clients: u32,
    fetched_mib: u64,
    gets: &[(&str, u64)],
    ready_us: u64,
    first_byte: (u64, u64, u64),
    errors: u64,
) -> ClientRun {
    ClientRun {
        run: "r".into(),
        mode,
        tree: tree.into(),
        client: 0,
        clients,
        node: "n1".into(),
        manifests: 10,
        files: 100,
        packs: 4,
        bytes_needed: 10 * MIB,
        bytes_fetched: fetched_mib * MIB,
        gets: gets.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
        tree_ready_us: ready_us,
        first_byte_us: Summary {
            n: first_byte.0,
            p50: first_byte.1,
            p99: first_byte.2,
            max: first_byte.2,
        },
        get_us: Summary::default(),
        get_errors: errors,
        config: BTreeMap::new(),
    }
}

fn engine_gets(grid: u64) -> Vec<(&'static str, u64)> {
    vec![
        ("grid_part", grid),
        ("demand_sub", 2),
        ("retry", 0),
        ("hedge", 1),
    ]
}

/// Three engine clients of one tree give one row with the median of each count, the
/// amplification of the median client, the tree-ready time at its median and 99th percentile, the
/// first-byte median of medians and largest 99th percentile, and the sum of the errors; the
/// tree is cut to 12 characters. The same tree fetched the shipped way is another row, showing
/// its chunk GETs, with 0.0 first-byte times when no client sent a GET; a tree that needed
/// nothing has an amplification of 0.00; rows are ordered by tree, mode and clients.
#[test]
fn the_trees_table_has_a_row_per_tree_mode_and_clients() {
    let tree_a = "a".repeat(64);
    let tree_b = "b".repeat(64);
    let runs = vec![
        client(&tree_b, Mode::Engine, 1, 0, &[], 1_000_000, (0, 0, 0), 0),
        client(
            &tree_a,
            Mode::Shipped,
            1,
            10,
            &[("chunk", 400)],
            8_000_000,
            (0, 0, 0),
            0,
        ),
        client(
            &tree_a,
            Mode::Engine,
            3,
            12,
            &engine_gets(20),
            2_000_000,
            (20, 5000, 40_000),
            0,
        ),
        client(
            &tree_a,
            Mode::Engine,
            3,
            11,
            &engine_gets(24),
            3_000_000,
            (20, 7000, 90_000),
            1,
        ),
        client(
            &tree_a,
            Mode::Engine,
            3,
            15,
            &engine_gets(22),
            10_000_000,
            (20, 6000, 60_000),
            2,
        ),
    ];
    let mut want = String::from(TREES_HEADER);
    want.push_str(
        "| aaaaaaaaaaaa | engine | 3 | 100 | 4 | 10 | 22/2/0/1 | 10.00 | 12.00 | 1.20 | 3.00 | 10.00 | 6.0 | 90.0 | 3 |\n",
    );
    want.push_str(
        "| aaaaaaaaaaaa | shipped | 1 | 100 | 4 | 10 | 400 chunk | 10.00 | 10.00 | 1.00 | 8.00 | 8.00 | 0.0 | 0.0 | 0 |\n",
    );
    want.push_str(
        "| bbbbbbbbbbbb | engine | 1 | 100 | 4 | 10 | 0/0/0/0 | 10.00 | 0.00 | 0.00 | 1.00 | 1.00 | 0.0 | 0.0 | 0 |\n",
    );
    assert_eq!(trees_table(&runs), want);
}

/// No client gives a table of its header alone.
#[test]
fn a_table_of_nothing_is_its_header() {
    assert_eq!(trees_table(&[]), TREES_HEADER);
    assert_eq!(demand_table(&[]), DEMAND_HEADER);
    assert_eq!(disks_table(&[], &[]), DISKS_HEADER);
    assert_eq!(metrics_table(&[], &[]), METRICS_HEADER);
}

fn read(load_clients: u32, len: u64, wait_us: u64) -> DemandRead {
    read_of("r", load_clients, len, wait_us)
}

fn read_of(run: &str, load_clients: u32, len: u64, wait_us: u64) -> DemandRead {
    DemandRead {
        run: run.into(),
        node: "n1".into(),
        load_clients,
        len,
        wait_us,
        outcome: "ready".into(),
    }
}

/// Reads are grouped by run and by length, 1 MiB being in the short class and one byte more in
/// the long; each row has the run's load clients (the largest its reads name), the count and the
/// wait's median, 99th percentile and largest in milliseconds.
#[test]
fn the_demand_table_groups_by_run_and_length() {
    let reads = vec![
        read_of("b", 8, 1_048_577, 20_000_000),
        read_of("a", 0, 4096, 1000),
        read_of("b", 8, 1_048_576, 10_000),
        read_of("a", 0, 2_000_000, 50_000),
        read_of("a", 2, 4096, 3000),
        read_of("a", 0, 4096, 2000),
    ];
    let want = format!(
        "{DEMAND_HEADER}\
| a | 2 | <=1MiB | 3 | 2.0 | 3.0 | 3.0 |
| a | 0 | >1MiB | 1 | 50.0 | 50.0 | 50.0 |
| b | 8 | <=1MiB | 1 | 10.0 | 10.0 | 10.0 |
| b | 8 | >1MiB | 1 | 20000.0 | 20000.0 | 20000.0 |
"
    );
    assert_eq!(demand_table(&reads), want);
}

fn disk(node: &str, device: &str, r_await: f64, aqu: f64, util: f64, rkb_s: f64) -> DiskSample {
    DiskSample {
        run: "r1".into(),
        node: node.into(),
        device: device.into(),
        t_ms: 0,
        reads: 1,
        rkb_s,
        r_await_ms: r_await,
        aqu_sz: aqu,
        util_pct: util,
    }
}

/// A device's row is over every interval of its run and node: the wait's median and 99th
/// percentile, the queue's 99th percentile and the means of the utilisation and the read rate
/// (in MiB/s). Rows are ordered by run, node and device. Naming devices lists only those.
#[test]
fn the_disks_table_summarises_each_device_of_each_node() {
    let samples = vec![
        disk("n2", "sda", 5.5, 0.25, 12.5, 512.0),
        disk("n1", "sda", 10.0, 4.0, 50.0, 1024.0),
        disk("n1", "sda", 1.0, 0.5, 0.0, 0.0),
        disk("n1", "loop2", 0.0, 0.0, 0.0, 0.0),
        disk("n1", "sda", 3.0, 1.5, 100.0, 2048.0),
        disk("n1", "sda", 2.0, 1.0, 50.0, 1024.0),
    ];
    let all = format!(
        "{DISKS_HEADER}\
| r1 | n1 | loop2 | 1 | 0.00 | 0.00 | 0.00 | 0.0 | 0.00 |
| r1 | n1 | sda | 4 | 2.00 | 10.00 | 4.00 | 50.0 | 1.00 |
| r1 | n2 | sda | 1 | 5.50 | 5.50 | 0.25 | 12.5 | 0.50 |
"
    );
    assert_eq!(disks_table(&samples, &[]), all);
    let only = format!(
        "{DISKS_HEADER}\
| r1 | n1 | loop2 | 1 | 0.00 | 0.00 | 0.00 | 0.0 | 0.00 |
"
    );
    assert_eq!(disks_table(&samples, &["loop2".to_owned()]), only);
}

fn delta(node: &str, name: &str, labels: &str, delta: f64, window_ms: u64) -> MetricDelta {
    MetricDelta {
        run: "r".into(),
        node: node.into(),
        name: name.into(),
        labels: labels.into(),
        delta,
        window_ms,
    }
}

/// A series' row sums its changes and windows over the runs and gives the change per second;
/// whole numbers print without decimals, others with three; a window of 0 gives 0.00 a second.
/// Naming series lists only those.
#[test]
fn the_metrics_table_sums_each_series_of_each_node() {
    let deltas = vec![
        delta("n2", "a_total", "{op=\"get\"}", 4.0, 0),
        delta("n1", "b_total", "", 0.5, 1000),
        delta("n1", "a_total", "{op=\"get\"}", 10.0, 2000),
        delta("n1", "a_total", "{op=\"get\"}", 5.0, 3000),
    ];
    let all = format!(
        "{METRICS_HEADER}\
| n1 | a_total{{op=\"get\"}} | 15 | 3.00 |
| n1 | b_total | 0.500 | 0.50 |
| n2 | a_total{{op=\"get\"}} | 4 | 0.00 |
"
    );
    assert_eq!(metrics_table(&deltas, &[]), all);
    let only = format!(
        "{METRICS_HEADER}\
| n1 | b_total | 0.500 | 0.50 |
"
    );
    assert_eq!(metrics_table(&deltas, &["b_total".to_owned()]), only);
}

/// With no records the report is the four headings, each with its table's header.
#[test]
fn the_report_of_nothing_is_four_empty_tables() {
    let want = format!(
        "### Trees\n\n{TREES_HEADER}\n### Demand reads\n\n{DEMAND_HEADER}\n### Drives\n\n{DISKS_HEADER}\n### Node metrics\n\n{METRICS_HEADER}\n"
    );
    assert_eq!(render(&[], &[], &[], &[]), want);
}

/// With records the report holds each table, whole, under its heading.
#[test]
fn the_report_has_each_table_under_its_heading() {
    let runs = vec![client(
        &"c".repeat(64),
        Mode::Engine,
        1,
        10,
        &engine_gets(5),
        1_500_000,
        (5, 2000, 3000),
        0,
    )];
    let reads = vec![read(0, 10, 1000)];
    let samples = vec![disk("n1", "sda", 1.0, 1.0, 1.0, 1024.0)];
    let deltas = vec![delta("n1", "x_total", "", 2.0, 1000)];
    let want = format!(
        "### Trees\n\n{}\n### Demand reads\n\n{}\n### Drives\n\n{}\n### Node metrics\n\n{}\n",
        trees_table(&runs),
        demand_table(&reads),
        disks_table(&samples, &[]),
        metrics_table(&deltas, &[]),
    );
    assert_eq!(render(&runs, &reads, &samples, &deltas), want);
    assert!(want.contains(
        "| cccccccccccc | engine | 1 | 100 | 4 | 10 | 5/2/0/1 | 10.00 | 10.00 | 1.00 | 1.50 | 1.50 | 2.0 | 3.0 | 0 |\n"
    ));
    assert!(want.contains("| n1 | x_total | 2 | 2.00 |\n"));
}

/// Percentiles are by nearest rank, an empty set gives 0, and numbers print whole or with three
/// decimals.
#[test]
fn percentiles_means_and_numbers_print_as_the_tables_need() {
    let v = [4.0, 1.0, 3.0, 2.0];
    assert_eq!(percentile(&v, 50), 2.0);
    assert_eq!(percentile(&v, 99), 4.0);
    assert_eq!(percentile(&v, 0), 1.0);
    assert_eq!(percentile(&[], 99), 0.0);
    assert_eq!(mean(&v), 2.5);
    assert_eq!(mean(&[]), 0.0);
    assert_eq!(number(3.0), "3");
    assert_eq!(number(2.5), "2.500");
}

const PLANS_HEADER: &str = "\
| variant | trees | aggregate amplification | tree p50 | tree p90 | tree p99 | tree max | trees above 1.50 | packs whole |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
";

fn plan(variant: &str, needed: u64, planned: u64, packs: u64, whole: u64) -> PlanRecord {
    PlanRecord {
        run: "r".into(),
        tree: "t".into(),
        variant: variant.into(),
        files: 1,
        packs,
        bytes_needed: needed,
        bytes_planned: planned,
        packs_whole: whole,
        packs_runs: packs - whole,
        ensures: packs,
    }
}

#[test]
fn the_plan_table_has_a_row_per_variant_with_the_amplification_of_its_trees() {
    let records = [
        plan("a", 100, 410, 2, 1),
        plan("b", 0, 0, 0, 0),
        plan("a", 100, 100, 2, 2),
        plan("a", 100, 200, 2, 0),
        plan("a", 100, 150, 2, 1),
    ];
    assert_eq!(
        plan_table(&records),
        format!(
            "{PLANS_HEADER}\
| a | 4 | 2.15 | 1.50 | 4.10 | 4.10 | 4.10 | 2 (50.0%) | 50.0% |
| b | 1 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0 (0.0%) | 0.0% |
"
        )
    );
}

#[test]
fn the_plan_table_of_no_plans_is_its_header() {
    assert_eq!(plan_table(&[]), PLANS_HEADER);
}

const RUNS_HEADER: &str = "\
| run | mode | verified | drive streams | clients | trees | pack GETs per tree p50 | manifest GETs per tree p50 | ready p50 s | ready p99 s | needed GiB | fetched GiB | amplification | first byte p50 ms | first byte p99 ms | errors |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
";
const CLASSES_HEADER: &str = "\
| run | class | devices | intervals | read MiB/s | r_await p50 ms | r_await p99 ms | aqu-sz p99 | util mean % | util p99 % |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
";

fn in_run(run: &str, config: &[(&str, &str)], mut c: ClientRun) -> ClientRun {
    c.run = run.into();
    c.config = config
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    c
}

/// A run's row has its mode and the `verified` and `drive_streams` of its configuration, the most
/// clients, the distinct trees, the medians of the pack GETs (all kinds) and manifest GETs, the
/// ready time's median and 99th percentile, the summed GiB, the amplification of the sums, the
/// first-byte median of medians and largest 99th percentile and the summed errors. A shipped run
/// has no drive streams and a run without the key has `-`; rows are ordered by run.
#[test]
fn the_runs_table_has_a_row_per_run() {
    let ta = "a".repeat(64);
    let tb = "b".repeat(64);
    let mut big = client(
        &ta,
        Mode::Engine,
        4,
        3072,
        &engine_gets(7),
        2_000_000,
        (3, 5000, 40_000),
        1,
    );
    big.bytes_needed = 2048 * MIB;
    big.bytes_fetched = 3072 * MIB;
    let mut big2 = big.clone();
    big2.tree = tb.clone();
    big2.clients = 2;
    big2.tree_ready_us = 9_000_000;
    big2.first_byte_us = Summary {
        n: 3,
        p50: 7000,
        p99: 90_000,
        max: 90_000,
    };
    big2.gets = engine_gets(3)
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    big2.manifests = 30;
    let engine = [
        in_run(
            "z-eng",
            &[("verified", "true"), ("drive_streams", "2")],
            big,
        ),
        in_run(
            "z-eng",
            &[("verified", "true"), ("drive_streams", "2")],
            big2,
        ),
    ];
    let shipped = in_run(
        "a-ship",
        &[("drive_streams", "9")],
        client(
            &ta,
            Mode::Shipped,
            1,
            5,
            &[("chunk", 40)],
            1_000_000,
            (0, 0, 0),
            0,
        ),
    );
    let bare = in_run(
        "m-bare",
        &[],
        client(&tb, Mode::Engine, 1, 0, &[], 0, (0, 0, 0), 0),
    );
    let mut runs: Vec<ClientRun> = engine.to_vec();
    runs.insert(0, bare);
    runs.push(shipped);
    let want = format!(
        "{RUNS_HEADER}\
| a-ship | shipped | - | - | 1 | 1 | 40 | 10 | 1.00 | 1.00 | 0.01 | 0.00 | 0.50 | 0.0 | 0.0 | 0 |
| m-bare | engine | - | - | 1 | 1 | 0 | 10 | 0.00 | 0.00 | 0.01 | 0.00 | 0.00 | 0.0 | 0.0 | 0 |
| z-eng | engine | true | 2 | 4 | 2 | 6 | 10 | 2.00 | 9.00 | 4.00 | 6.00 | 1.50 | 5.0 | 90.0 | 2 |
"
    );
    assert_eq!(runs_table(&runs), want);
    assert_eq!(runs_table(&[]), RUNS_HEADER);
}

#[expect(
    clippy::too_many_arguments,
    reason = "a fixture: every figure the tables read"
)]
fn dsample(
    run: &str,
    node: &str,
    dev: &str,
    reads: u64,
    r_await: f64,
    aqu: f64,
    util: f64,
    rkb_s: f64,
) -> DiskSample {
    DiskSample {
        run: run.into(),
        node: node.into(),
        device: dev.into(),
        t_ms: 0,
        reads,
        rkb_s,
        r_await_ms: r_await,
        aqu_sz: aqu,
        util_pct: util,
    }
}

fn classes() -> Vec<(String, Vec<(String, String)>)> {
    vec![
        (
            "hdd".to_owned(),
            vec![
                ("n1".to_owned(), "sda".to_owned()),
                ("n2".to_owned(), "sda".to_owned()),
            ],
        ),
        (
            "ssd".to_owned(),
            vec![("n1".to_owned(), "nvme0n1".to_owned())],
        ),
        ("idle".to_owned(), vec![("n3".to_owned(), "sdz".to_owned())]),
    ]
}

/// A row per run and class: devices with samples, intervals, the sum of each device's mean read
/// rate, the wait over the intervals with reads only (`-` for a class with none), the queue's 99th
/// percentile, the mean and 99th percentile of the utilisation; a drive in no class is ignored; a
/// class with no sample in a run still has its row.
#[test]
fn the_drive_classes_table_has_a_row_per_run_and_class() {
    let samples = vec![
        dsample("r2", "n1", "sda", 5, 8.0, 1.0, 10.0, 2048.0),
        dsample("r1", "n1", "sda", 1, 4.0, 2.0, 20.0, 1024.0),
        dsample("r1", "n1", "sda", 0, 0.0, 0.5, 0.0, 3072.0),
        dsample("r1", "n2", "sda", 3, 12.0, 4.0, 100.0, 1024.0),
        dsample("r1", "n1", "nvme0n1", 0, 0.0, 0.0, 0.0, 0.0),
        dsample("r1", "n1", "loop2", 9, 99.0, 9.0, 99.0, 9999.0),
        dsample("r3", "n9", "sdq", 1, 1.0, 1.0, 1.0, 1.0),
    ];
    let want = format!(
        "{CLASSES_HEADER}\
| r1 | hdd | 2 | 3 | 3.0 | 4.0 | 12.0 | 4.00 | 40.0 | 100.0 |
| r1 | ssd | 1 | 1 | 0.0 | - | - | 0.00 | 0.0 | 0.0 |
| r1 | idle | 0 | 0 | 0.0 | - | - | 0.00 | 0.0 | 0.0 |
| r2 | hdd | 1 | 1 | 2.0 | 8.0 | 8.0 | 1.00 | 10.0 | 10.0 |
| r2 | ssd | 0 | 0 | 0.0 | - | - | 0.00 | 0.0 | 0.0 |
| r2 | idle | 0 | 0 | 0.0 | - | - | 0.00 | 0.0 | 0.0 |
"
    );
    assert_eq!(drive_classes_table(&samples, &classes()), want);
    assert_eq!(drive_classes_table(&[], &classes()), CLASSES_HEADER);
    assert_eq!(drive_classes_table(&samples, &[]), CLASSES_HEADER);
}

/// `NAME=node:dev,...` gives the name and its pairs; a missing `=`, an empty name, an empty list
/// and a device without a node or a device name are usage errors naming the argument.
#[test]
fn a_class_argument_is_a_name_and_node_device_pairs() {
    assert_eq!(
        parse_class("hdd=n1:sda,n2:sdb").unwrap(),
        (
            "hdd".to_owned(),
            vec![
                ("n1".to_owned(), "sda".to_owned()),
                ("n2".to_owned(), "sdb".to_owned())
            ]
        )
    );
    for (arg, why) in [
        ("hdd", "expected NAME=node:dev,node:dev,..."),
        ("=n1:sda", "expected NAME=node:dev,node:dev,..."),
        ("hdd=", "a device is not <node>:<device>"),
        ("hdd=n1", "a device is not <node>:<device>"),
        ("hdd=:sda", "a device is not <node>:<device>"),
        ("hdd=n1:", "a device is not <node>:<device>"),
        ("hdd=n1:sda,x", "a device is not <node>:<device>"),
    ] {
        assert_eq!(
            parse_class(arg).unwrap_err().to_string(),
            format!("--class {arg:?}: {why}")
        );
    }
}
