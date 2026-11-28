//! What the core reports of its opens, as the cache counts and renders it: the families at zero
//! before any report; each process's totals added by what they grew by, across a hand-over
//! and a lost report; the gauge as the sum of the mounts' last reports; the bound on the mounts.

use super::{KINDS, Opens, Outcome, Source, Spans, kind};
use crate::server::local_cache::attaches::BUCKETS;
use crate::server::local_cache::descriptors::MOST_MOUNTS;
use crate::server::local_cache::testing::Rig;

/// The `le` of each bucket, as rendered.
const LE: [&str; BUCKETS] = [
    "0.001", "0.002", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5",
    "10", "30", "60", "300", "+Inf",
];

/// The labels of each kind, in the order of [`KINDS`].
const LABELS: [&str; 4] = [
    "outcome=\"ok\",source=\"cached\"",
    "outcome=\"ok\",source=\"filled\"",
    "outcome=\"error\",source=\"cached\"",
    "outcome=\"error\",source=\"filled\"",
];

/// Totals of one kind: `ms` milliseconds, and `count` opens in the bucket `bucket`.
fn spans(ms: u64, bucket: usize, count: u64) -> Spans {
    let mut buckets = [0; BUCKETS];
    buckets[bucket] = count;
    Spans { ms, buckets }
}

/// A report of `mount` by `process` with `waiting` opens in flight and the totals `spans`.
fn report(mount: &str, process: u64, waiting: u64, spans: [Spans; 4]) -> Opens {
    Opens {
        mount: mount.to_owned(),
        process,
        waiting,
        spans,
    }
}

/// A report whose only opens are `count` of kind `at` in the first bucket, of `ms` in all.
fn only(process: u64, at: usize, count: u64, ms: u64) -> Opens {
    let mut all = [Spans::default(); 4];
    all[at] = spans(ms, 0, count);
    report("m", process, 0, all)
}

/// The lines of the two families in the cache's `/metricz` text.
fn families(rig: &Rig) -> Vec<String> {
    rig.cache
        .metrics()
        .lines()
        .filter(|line| {
            line.contains("tierstore_fs_open_seconds")
                || line.contains("tierstore_fs_opens_waiting")
        })
        .map(str::to_owned)
        .collect()
}

/// The lines of the histogram of the kind of `labels` with every bucket at or above `from` holding
/// `count`, of `sum` seconds; every bucket below `from` empty.
fn histogram(labels: &str, from: usize, count: u64, sum: &str) -> Vec<String> {
    let mut lines: Vec<String> = LE
        .iter()
        .enumerate()
        .map(|(at, le)| {
            let n = if at >= from { count } else { 0 };
            format!("tierstore_fs_open_seconds_bucket{{{labels},le=\"{le}\"}} {n}")
        })
        .collect();
    lines.push(format!("tierstore_fs_open_seconds_sum{{{labels}}} {sum}"));
    lines.push(format!(
        "tierstore_fs_open_seconds_count{{{labels}}} {count}"
    ));
    lines
}

/// The count of the kind of `labels`, in the cache's text.
fn count(rig: &Rig, labels: &str) -> u64 {
    let prefix = format!("tierstore_fs_open_seconds_count{{{labels}}} ");
    families(rig)
        .iter()
        .find_map(|line| line.strip_prefix(&prefix).map(|n| n.parse().unwrap()))
        .unwrap()
}

/// Asserts that the kinds are the four pairs in order, each at the place [`kind`] gives it.
#[test]
fn every_pair_of_an_outcome_and_a_source_has_its_own_place() {
    for (at, (outcome, source)) in KINDS.into_iter().enumerate() {
        assert_eq!(kind(outcome, source), at);
    }
    assert_eq!(kind(Outcome::Error, Source::Filled), 3);
}

/// Asserts that before any report the two families have their `# TYPE` lines, every series of the
/// four kinds at zero, and the gauge at zero: the whole text, in order.
#[test]
fn the_families_are_at_zero_before_a_report() {
    let rig = Rig::new("opens-none");
    let mut expected = vec!["# TYPE tierstore_fs_open_seconds histogram".to_owned()];
    for labels in LABELS {
        expected.extend(histogram(labels, 0, 0, "0"));
    }
    expected.push("# TYPE tierstore_fs_opens_waiting gauge".to_owned());
    expected.push("tierstore_fs_opens_waiting 0".to_owned());
    assert_eq!(families(&rig), expected);
}

/// Asserts the whole text after one report of each kind in its own bucket: the buckets cumulated
/// from the bucket the open fell in, the sum in seconds, the count, and the gauge at the report's
/// opens in flight.
#[test]
fn a_report_shows_each_kind_in_its_buckets() {
    let rig = Rig::new("opens-one");
    let all = [
        spans(1, 0, 1),
        spans(30, 5, 1),
        spans(7_000, 12, 1),
        spans(300_001, 16, 1),
    ];
    rig.cache.opens_reported(report("m", 1, 3, all));
    let mut expected = vec!["# TYPE tierstore_fs_open_seconds histogram".to_owned()];
    expected.extend(histogram(LABELS[0], 0, 1, "0.001"));
    expected.extend(histogram(LABELS[1], 5, 1, "0.03"));
    expected.extend(histogram(LABELS[2], 12, 1, "7"));
    // Above the last bound: in `+Inf` alone.
    expected.extend(histogram(LABELS[3], 16, 1, "300.001"));
    expected.push("# TYPE tierstore_fs_opens_waiting gauge".to_owned());
    expected.push("tierstore_fs_opens_waiting 3".to_owned());
    assert_eq!(families(&rig), expected);
}

/// Asserts how the counts follow the totals of the processes of one mount: a report that was lost
/// loses nothing the next one carries; the two processes of a hand-over, reporting in turns, each
/// add only what their own totals grew by, so neither is counted twice; a process not seen adds its
/// whole totals, as does one whose totals went down (a new process given the same id), whether its
/// milliseconds or a bucket went down; and the totals of the two latest processes only are kept,
/// so one older than those counts as new.
#[test]
fn the_counts_add_what_each_processs_totals_grew_by() {
    let rig = Rig::new("opens-processes");
    let ok = |rig: &Rig| count(rig, LABELS[0]);
    let steps: [(Opens, u64); 10] = [
        (only(10, 0, 2, 2), 2),
        // The report of 5 was lost: the next carries it.
        (only(10, 0, 9, 9), 9),
        // The successor of a hand-over, and the two in turns.
        (only(11, 0, 1, 1), 10),
        (only(10, 0, 12, 12), 13),
        (only(11, 0, 4, 4), 16),
        (only(10, 0, 12, 12), 16),
        // A new process given the id of one before, its milliseconds lower: its totals are new.
        (only(11, 0, 2, 2), 18),
        // The same, its milliseconds higher but a count lower.
        (only(11, 0, 1, 50), 19),
        // A third process: the first falls out of those kept.
        (only(12, 0, 5, 5), 24),
        (only(10, 0, 12, 12), 36),
    ];
    for (report, want) in steps {
        let process = report.process;
        rig.cache.opens_reported(report);
        assert_eq!(ok(&rig), want, "after {process}");
    }
    let sum = format!("tierstore_fs_open_seconds_sum{{{}}} 0.085", LABELS[0]);
    assert!(families(&rig).contains(&sum), "{sum}");
    assert_eq!(count(&rig, LABELS[3]), 0);
}

/// Asserts that the gauge is the sum over the mounts of each one's last report, whichever process
/// sent it, and falls to zero when every mount last reported none waiting.
#[test]
fn the_gauge_is_the_sum_of_the_last_report_of_each_mount() {
    let rig = Rig::new("opens-gauge");
    let gauge = |rig: &Rig| families(rig).last().cloned().unwrap();
    let none = [Spans::default(); 4];
    rig.cache.opens_reported(report("a", 1, 5, none));
    rig.cache.opens_reported(report("b", 1, 3, none));
    assert_eq!(gauge(&rig), "tierstore_fs_opens_waiting 8");
    rig.cache.opens_reported(report("a", 2, 1, none));
    assert_eq!(gauge(&rig), "tierstore_fs_opens_waiting 4");
    rig.cache.opens_reported(report("a", 1, 0, none));
    rig.cache.opens_reported(report("b", 1, 0, none));
    assert_eq!(gauge(&rig), "tierstore_fs_opens_waiting 0");
}

/// Asserts that a mount beyond [`MOST_MOUNTS`] is dropped, counted nowhere, while the mounts kept
/// go on counting.
#[test]
fn a_mount_beyond_the_bound_is_dropped() {
    let rig = Rig::new("opens-bound");
    for n in 0..MOST_MOUNTS {
        let mut one = only(1, 1, 1, 1);
        one.mount = format!("m{n:03}");
        rig.cache.opens_reported(one);
    }
    let mut extra = only(1, 1, 9, 9);
    extra.mount = "extra".to_owned();
    extra.waiting = 100;
    rig.cache.opens_reported(extra);
    assert_eq!(count(&rig, LABELS[1]), MOST_MOUNTS as u64);
    let mut again = only(1, 1, 3, 3);
    again.mount = "m000".to_owned();
    rig.cache.opens_reported(again);
    assert_eq!(count(&rig, LABELS[1]), MOST_MOUNTS as u64 + 2);
    assert_eq!(
        families(&rig).last().unwrap(),
        "tierstore_fs_opens_waiting 0"
    );
}
