use tierstore_testkit::dir::TestDir;

use super::{cell, load_of, mount_of, number, read, render, short};
use crate::record::{RunRecord, example};

/// Run `run` of `variant`, with `per_s` cycles a second and `mb` MB/s cold.
fn record(variant: &str, run: u32, per_s: f64, mb: f64) -> RunRecord {
    let mut r = example();
    r.variant = variant.to_owned();
    r.run = run;
    r.cycles.per_s = per_s;
    r.cold.mb_per_s = mb;
    r
}

/// Numbers keep two decimals below 10, one below 100 and none from 100; a cell is the median
/// of its values (the mean of the middle two for an even count) with the least and the most,
/// and `-` when no run measured it.
#[test]
fn a_cell_is_the_median_with_the_least_and_the_most() {
    assert_eq!(
        (number(7.0), number(7.25), number(12.34), number(2769.4)),
        ("7.00".into(), "7.25".into(), "12.3".into(), "2769".into())
    );
    assert_eq!(cell(vec![3.0, 1.0, 2.0]), "2.00 (1.00-3.00)");
    assert_eq!(cell(vec![4.0, 1.0, 2.0, 3.0]), "2.50 (1.00-4.00)");
    assert_eq!(cell(vec![28_199.0]), "28199 (28199-28199)");
    assert_eq!(cell(Vec::new()), "-");
}

/// The report leaves out the first runs of each series, gives a row per variant and regime in
/// the order they first appear, and says how many runs it kept of how many; a series with no run
/// kept has `-` figures and no environment row.
#[test]
fn the_report_keeps_the_runs_after_the_first_and_groups_them() {
    let records = vec![
        record("v1", 0, 1.0, 1.0),
        record("v2", 0, 1.0, 1.0),
        record("v1", 1, 2000.0, 300.0),
        record("v1", 2, 3000.0, 500.0),
        record("v2", 1, 20_000.0, 900.0),
        record("v1", 3, 2500.0, 400.0),
        record("v3", 0, 5.0, 5.0),
    ];
    let text = render(&records, 1);
    let line = |start: &str| -> String {
        text.lines()
            .find(|l| l.starts_with(start))
            .unwrap_or_else(|| panic!("no line {start:?} in\n{text}"))
            .to_owned()
    };
    assert!(text.starts_with(
        "Median of the runs kept (least-most); the first 1 run(s) of each series left out.\n"
    ));
    assert!(line("| v1 | caches-off | 2500 (2000-3000) |").contains("| 0.00 (0.00-0.00) |"));
    assert!(
        line("| v1 | caches-off | 400 (300-500) | 400 (400-400) |")
            .ends_with("| 5.00 (5.00-5.00) | 5.00 (5.00-5.00) |")
    );
    assert_eq!(
        line("| v3 | caches-off | - |"),
        format!("| v3 | caches-off |{}", " - |".repeat(7))
    );
    assert!(line("| v1 | caches-off | 3 of 4 |").starts_with(
        "| v1 | caches-off | 3 of 4 | 0 | 0.10 (0.10-0.10) | abababababab | - | open=explicit |"
    ));
    assert!(
        line("| v3 | caches-off | 0 of 1 |")
            .starts_with("| v3 | caches-off | 0 of 1 | 0 | - | - | - |  |")
    );
    let order: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("| v"))
        .map(|l| &l[..1])
        .filter(|c| c.bytes().all(|b| b.is_ascii_digit()))
        .take(3)
        .collect();
    assert_eq!(order, ["1", "2", "3"]);
    assert_eq!(
        line("| v2 | caches-off | 7.0.0 |"),
        "| v2 | caches-off | 7.0.0 | 88 | schedutil x88 | enable_uring=N | rw - fuse.fsbench-null fsbench-null rw | max_background=1024 |"
    );
    assert!(!text.contains("| v3 | caches-off | 7.0.0 |"));
}

/// A run that measured no control group and no connection, on a machine whose kernel, CPUs and
/// mount were not read, shows `-` there; its errors are counted in its row.
#[test]
fn what_a_run_did_not_measure_is_a_dash() {
    let mut r = example();
    r.cpu.cgroup_system_ms_per_1k = None;
    r.samples.waiting_mean = None;
    r.samples.waiting_max = None;
    r.env = crate::host::Environment::default();
    r.cycles.errors = 4;
    r.warm.errors = 1;
    r.server.exe_sha256 = Some("0123456789abcdef".to_owned());
    let text = render(&[r], 0);
    assert!(text.contains("| 3.00 (3.00-3.00) | - | - |\n"), "{text}");
    assert!(
        text.contains(
            "| null | caches-off | 1 of 1 | 5 | - | abababababab | 0123456789ab | open=explicit |"
        ),
        "{text}"
    );
    assert!(
        text.contains("| null | caches-off | - | - |  |  | - |  |"),
        "{text}"
    );
}

/// A results file reads back as its records, blank lines passed over; a line that is not a
/// record is an error that names it, and a file that is not there is an error.
#[test]
fn a_results_file_reads_back_and_a_bad_line_is_named() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-report-read");
    let path = dir.path().join("results.jsonl");
    let one = serde_json::to_string(&example()).unwrap();
    std::fs::write(&path, format!("{one}\n\n{one}\n")).unwrap();
    assert_eq!(read(&path).unwrap(), [example(), example()]);
    std::fs::write(&path, format!("{one}\n{{\"format\":1}}\n")).unwrap();
    let error = read(&path).unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("{}:2: not a run record: ", path.display())),
        "{error}"
    );
    assert!(matches!(
        read(&dir.path().join("missing")),
        Err(crate::Error::Io { .. })
    ));
}

/// A digest is cut to 12 digits, a short one is kept whole, none is `-`; a mountinfo line shows
/// from its options on, and a line too short to have them shows whole.
#[test]
fn digests_are_cut_and_mount_lines_lose_their_places() {
    assert_eq!(
        (
            short(Some("0123456789abcdef")),
            short(Some("ab")),
            short(None)
        ),
        ("0123456789ab", "ab", "-")
    );
    assert_eq!(
        mount_of("1 2 0:5 / /a/b ro,nosuid - fuse.x x ro"),
        "ro,nosuid - fuse.x x ro"
    );
    assert_eq!(mount_of("1 2 0:5"), "1 2 0:5");
    assert_eq!(load_of("1.50 2 3 1/2 9"), Some(1.5));
    assert_eq!(load_of("busy"), None);
}
