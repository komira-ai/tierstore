//! The storm's records: what a line holds, how a results file is read, and the tables made of
//! it.

use std::collections::BTreeMap;

use tierstore_testkit::dir::TestDir;

use super::{FORMAT, PhaseRecord, ProbeRecord, Sampled, StormRecord, read, render};
use crate::hist::Histogram;
use crate::host::Environment;

/// A histogram of `values`, sealed.
fn hist(values: &[u64]) -> Histogram {
    let mut h = Histogram::default();
    for &v in values {
        h.add(v);
    }
    h.sealed()
}

fn probe(role: &str, cpu: usize, stormed: bool, values: &[u64]) -> ProbeRecord {
    ProbeRecord {
        role: role.to_owned(),
        cpu,
        stormed,
        errors: 0,
        stat: hist(values),
    }
}

fn phase(name: &str, probes: Vec<ProbeRecord>, reads: u64, held: &[u64]) -> PhaseRecord {
    PhaseRecord {
        name: name.to_owned(),
        storm_threads: if reads > 0 { 8 } else { 0 },
        stormed_cpus: u32::from(reads > 0),
        window_ns: 2_000_000_000,
        storm_reads: reads,
        storm_errors: 0,
        storm_failures: BTreeMap::new(),
        probes,
        held: Sampled::of(held),
        parked: Sampled::of(&[3, 5]),
        waiting: Some(Sampled::of(&[7])),
    }
}

/// A run of `label`: idle probes of 10 us on both CPUs; a one-CPU storm whose probe on the
/// storm's CPU takes `stormed_us` and on the other 10 us.
fn record(label: &str, run: u32, stormed_us: u64) -> StormRecord {
    let mut environment = Environment {
        kernel: Some("7.0.0-42-generic".to_owned()),
        ..Environment::default()
    };
    environment
        .fuse_parameters
        .insert("enable_uring".to_owned(), "Y".to_owned());
    StormRecord {
        format: FORMAT,
        label: label.to_owned(),
        run,
        transport: "io_uring".to_owned(),
        uring_registered_bytes: Some(57_798_400),
        uring_depth: Some(5),
        uring_queues: Some(88),
        readers: 1,
        park_us: 60_000,
        read: "buffered".to_owned(),
        per_cpu: 8,
        storm_cpu: 0,
        other_cpu: Some(87),
        settings: BTreeMap::new(),
        environment,
        phases: vec![
            phase(
                "idle",
                vec![
                    probe("storm-cpu", 0, false, &[10_000; 4]),
                    probe("other-cpu", 87, false, &[10_000; 4]),
                ],
                0,
                &[0, 0],
            ),
            phase(
                "one-cpu",
                vec![
                    probe("storm-cpu", 0, true, &[stormed_us * 1000; 4]),
                    probe("other-cpu", 87, false, &[10_000; 4]),
                ],
                100,
                &[5, 5, 2],
            ),
        ],
    }
}

/// Samples give their count, mean and largest; none give zeros.
#[test]
fn samples_give_their_mean_and_largest() {
    assert_eq!(
        Sampled::of(&[1, 2, 6]),
        Sampled {
            samples: 3,
            mean: 3.0,
            max: 6
        }
    );
    assert_eq!(Sampled::of(&[]), Sampled::default());
    assert_eq!(
        Sampled::of(&[u64::MAX, 1]).max,
        u64::MAX,
        "the sum saturates"
    );
}

/// A record is one line of JSON that reads back as itself, and a results file of such lines
/// (blank lines skipped) reads as its records; a line that is not one is an error naming it.
#[test]
fn records_read_back_from_a_results_file() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-stormrec-read");
    let path = dir.path().join("storm.jsonl");
    let records = vec![record("a", 0, 10), record("a", 1, 70_000)];
    let text: String = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n\n")
        .collect();
    assert!(!text.lines().next().unwrap().contains('\n'));
    std::fs::write(&path, &text).unwrap();
    assert_eq!(read(&path).unwrap(), records);
    std::fs::write(&path, format!("{text}{{\"format\":1}}\n")).unwrap();
    let error = read(&path).unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("{}:5: not a storm record: ", path.display())),
        "{error}"
    );
    let missing = dir.path().join("missing");
    assert!(
        read(&missing)
            .unwrap_err()
            .to_string()
            .starts_with(&format!("read {}: ", missing.display()))
    );
}

/// The tables: the settings of each label, then a row per label, phase and probe with the
/// median and range of the runs kept: the rate and latency of the `stat`s, the stormed p99 over
/// the idle one, the storm's reads per second and the samples. The first `discard` runs of each
/// label are left out, and a label with none left has no settings row.
#[test]
fn the_tables_give_each_phase_and_probe_against_idle() {
    let mut dev = record("dev", 0, 10);
    dev.transport = "dev-fuse (NotAsked)".to_owned();
    dev.uring_registered_bytes = None;
    dev.uring_depth = None;
    dev.environment = Environment::default();
    dev.phases[1].waiting = None;
    let records = vec![
        record("uring", 0, 1),
        record("uring", 1, 60_000),
        record("uring", 2, 70_000),
        dev,
    ];
    let out = render(&records, 1);
    let expected = "\
| label | transport | depth | registered MiB | park ms | read | threads per CPU | readers | runs kept | kernel | enable_uring |
|---|---|---|---|---|---|---|---|---|---|---|
| uring | io_uring | 5 | 55.1 | 60.0 | buffered | 8 | 1 | 2 of 3 | 7.0.0-42-generic | Y |

| label | phase | probe | stormed | stats/s | stat p50 us | stat p99 us | stat max ms | p99 / idle p99 | stat errors | storm reads/s | storm errors | held mean | held max | parked max | waiting max |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| uring | idle | storm-cpu | no | 2.00 (2.00-2.00) | 10.0 (10.0-10.0) | 10.0 (10.0-10.0) | 0.01 (0.01-0.01) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 5.00 (5.00-5.00) | 7.00 (7.00-7.00) |
| uring | idle | other-cpu | no | 2.00 (2.00-2.00) | 10.0 (10.0-10.0) | 10.0 (10.0-10.0) | 0.01 (0.01-0.01) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 5.00 (5.00-5.00) | 7.00 (7.00-7.00) |
| uring | one-cpu | storm-cpu | yes | 2.00 (2.00-2.00) | 64865 (60000-69730) | 64865 (60000-69730) | 65.0 (60.0-70.0) | 6487 (6000-6973) | 0.00 (0.00-0.00) | 50.0 (50.0-50.0) | 0.00 (0.00-0.00) | 4.00 (4.00-4.00) | 5.00 (5.00-5.00) | 5.00 (5.00-5.00) | 7.00 (7.00-7.00) |
| uring | one-cpu | other-cpu | no | 2.00 (2.00-2.00) | 10.0 (10.0-10.0) | 10.0 (10.0-10.0) | 0.01 (0.01-0.01) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 50.0 (50.0-50.0) | 0.00 (0.00-0.00) | 4.00 (4.00-4.00) | 5.00 (5.00-5.00) | 5.00 (5.00-5.00) | 7.00 (7.00-7.00) |
";
    assert_eq!(out, expected);
    let all = render(&records, 0);
    assert!(
        all.contains(
            "| dev | dev-fuse (NotAsked) | - | - | 60.0 | buffered | 8 | 1 | 1 of 1 | - | - |"
        ),
        "{all}"
    );
    assert!(
        all.contains("| dev | one-cpu | storm-cpu | yes | 2.00 (2.00-2.00) | 10.0 (10.0-10.0) | 10.0 (10.0-10.0) | 0.01 (0.01-0.01) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 50.0 (50.0-50.0) | 0.00 (0.00-0.00) | 4.00 (4.00-4.00) | 5.00 (5.00-5.00) | 5.00 (5.00-5.00) | - |"),
        "a figure not measured is -: {all}"
    );
}

/// A figure with nothing to be read from is `-`: a phase of no time has no rates, and a probe
/// with no idle phase, or an idle probe with no `stat`, has no ratio.
#[test]
fn figures_with_nothing_to_read_are_dashes() {
    let mut r = record("x", 0, 10);
    r.phases[0].probes[0].stat = Histogram::default();
    r.phases[1].window_ns = 0;
    let out = render(std::slice::from_ref(&r), 0);
    let row = out
        .lines()
        .find(|l| l.starts_with("| x | one-cpu | storm-cpu |"))
        .unwrap();
    let cells: Vec<&str> = row.split(" | ").collect();
    assert_eq!(
        (cells[4], cells[8], cells[10]),
        ("-", "-", "-"),
        "stats/s, ratio, storm reads/s: {row}"
    );
    r.phases.remove(0);
    let out = render(std::slice::from_ref(&r), 0);
    let row = out
        .lines()
        .find(|l| l.starts_with("| x | one-cpu | other-cpu |"))
        .unwrap();
    assert_eq!(row.split(" | ").nth(8), Some("-"), "no idle phase: {row}");
}

/// The ratio to idle is `-` when the idle phase has no probe of that role, and when the idle
/// probe's p99 is zero.
#[test]
fn the_ratio_to_idle_needs_an_idle_probe_of_the_role_with_a_p99() {
    let ratio = |r: &StormRecord, role: &str| {
        let out = render(std::slice::from_ref(r), 0);
        let row = out
            .lines()
            .find(|l| l.starts_with(&format!("| x | one-cpu | {role} |")))
            .unwrap()
            .to_owned();
        row.split(" | ").nth(8).unwrap().to_owned()
    };
    let mut r = record("x", 0, 10);
    r.phases[0].probes.remove(1);
    assert_eq!(ratio(&r, "other-cpu"), "-");
    assert_eq!(ratio(&r, "storm-cpu"), "1.00 (1.00-1.00)");
    r.phases[0].probes[0].stat = hist(&[0, 0]);
    assert_eq!(ratio(&r, "storm-cpu"), "-");
}
