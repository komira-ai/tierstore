//! `tierstore_request_to_start_seconds` of a cache on a clock the test moves: one sample per run,
//! from its first request to its first start, rendered as a Prometheus histogram.

use super::{BOUNDS_MS, MOST_RUNS, REQUEST_TO_START, seconds};
use crate::server::local_cache::testing::Rig;

/// The run of the tests, as the provider names it, and two of its sources.
const RUN: &str = "mount-provider/6167656e74/6a732e31";
const SOURCE_0: &str = "mount-provider/6167656e74/6a732e31/0";
const SOURCE_1: &str = "mount-provider/6167656e74/6a732e31/1";

/// The lines of the family in the cache's `/metricz` text.
fn family(rig: &Rig) -> Vec<String> {
    rig.cache
        .metrics()
        .lines()
        .filter(|line| line.contains(REQUEST_TO_START))
        .map(str::to_owned)
        .collect()
}

/// The text of the family with `below[i]` spans at or under the bound `i` (cumulative), `count`
/// spans in all and their sum `sum`, in seconds as rendered.
fn expected(below: [u64; BOUNDS_MS.len()], count: u64, sum: &str) -> Vec<String> {
    let mut lines = vec![format!("# TYPE {REQUEST_TO_START} histogram")];
    for (bound, below) in BOUNDS_MS.iter().zip(below) {
        let le = seconds(*bound);
        lines.push(format!("{REQUEST_TO_START}_bucket{{le=\"{le}\"}} {below}"));
    }
    lines.push(format!("{REQUEST_TO_START}_bucket{{le=\"+Inf\"}} {count}"));
    lines.push(format!("{REQUEST_TO_START}_sum {sum}"));
    lines.push(format!("{REQUEST_TO_START}_count {count}"));
    lines
}

/// The cumulative counts of one span that falls in the bucket `first` and every one after it.
fn one_from(first: usize) -> [u64; BOUNDS_MS.len()] {
    std::array::from_fn(|bucket| u64::from(bucket >= first))
}

/// Asserts that a cache that has sampled nothing renders the whole family at zero, with the
/// bounds of the buckets in seconds, from 5 ms to 900 s.
#[tokio::test]
async fn an_unsampled_cache_renders_every_bucket_at_zero() {
    let rig = Rig::new("tscache-starts-zero");
    assert_eq!(family(&rig), expected([0; BOUNDS_MS.len()], 0, "0"));
    let bounds: Vec<String> = BOUNDS_MS.iter().map(|ms| seconds(*ms)).collect();
    let want = [
        "0.005", "0.01", "0.025", "0.05", "0.1", "0.2", "0.5", "1", "2.5", "5", "10", "30", "60",
        "120", "300", "900",
    ];
    assert_eq!(bounds, want);
}

/// Asserts that a run is sampled once, from its first request to its first start: a second
/// request of the run (another source) does not move the stamp, the start of its first source is
/// the span (180 ms, in the bucket of 200 ms and every one above, not in that of 100 ms), and a
/// start of its other source after that adds nothing.
#[tokio::test]
async fn a_run_is_sampled_once_from_its_first_request_to_its_first_start() {
    let rig = Rig::new("tscache-starts-once");
    rig.clock.set(10_000);
    rig.cache.run_requested(RUN);
    rig.clock.advance(30);
    rig.cache.run_requested(RUN);
    rig.clock.advance(150);
    assert_eq!(rig.cache.run_started(SOURCE_0), Some(180));
    rig.clock.advance(1_000);
    assert_eq!(rig.cache.run_started(SOURCE_1), None);
    assert_eq!(rig.cache.run_started(SOURCE_0), None);
    assert_eq!(family(&rig), expected(one_from(5), 1, "0.18"));
    // The run stays known until a release, so that its repeated requests do not sample it again.
    assert_eq!(rig.cache.runs_kept(), 1);
    rig.cache.run_requested(RUN);
    assert_eq!(rig.cache.run_started(SOURCE_0), None);
}

/// Asserts that a span equal to a bound is in that bucket, that one past the last bound (900 s)
/// is only in `+Inf`, and that the sum is of every span, with the milliseconds kept.
#[tokio::test]
async fn a_span_on_a_bound_is_in_its_bucket_and_one_past_the_last_only_in_the_count() {
    let rig = Rig::new("tscache-starts-bounds");
    let runs = ["mount-provider/61/31", "mount-provider/61/32"];
    for run in runs {
        rig.cache.run_requested(run);
    }
    rig.clock.advance(100);
    assert_eq!(rig.cache.run_started(&format!("{}/0", runs[0])), Some(100));
    rig.clock.advance(900_001 - 100);
    assert_eq!(
        rig.cache.run_started(&format!("{}/3", runs[1])),
        Some(900_001)
    );
    assert_eq!(family(&rig), expected(one_from(4), 2, "900.101"));
}

/// Asserts that a start of a run that was never requested here, a path that is no source's (no
/// directory, or a placement of the trees API), and a run forgotten by a release add nothing.
#[tokio::test]
async fn a_start_of_a_run_not_requested_or_forgotten_adds_nothing() {
    let rig = Rig::new("tscache-starts-unknown");
    rig.cache.run_requested(RUN);
    rig.clock.advance(50);
    assert_eq!(
        rig.cache
            .run_started("mount-provider/6167656e74/6a732e32/0"),
        None
    );
    assert_eq!(rig.cache.run_started("jobs"), None);
    assert_eq!(rig.cache.run_started("jobs/1f3a/root"), None);
    rig.cache.run_forgotten(RUN);
    assert_eq!(rig.cache.runs_kept(), 0);
    assert_eq!(rig.cache.run_started(SOURCE_0), None);
    // Forgetting a run that is not kept is nothing.
    rig.cache.run_forgotten(RUN);
    assert_eq!(family(&rig), expected([0; BOUNDS_MS.len()], 0, "0"));
}

/// Asserts that a span over a step of the clock back is 0: in the first bucket, not a wrap.
#[tokio::test]
async fn a_clock_stepped_back_gives_a_span_of_zero() {
    let rig = Rig::new("tscache-starts-back");
    rig.clock.set(5_000);
    rig.cache.run_requested(RUN);
    rig.clock.set(4_000);
    assert_eq!(rig.cache.run_started(SOURCE_0), Some(0));
    assert_eq!(family(&rig), expected(one_from(0), 1, "0"));
}

/// Asserts that at most [`MOST_RUNS`] runs are kept: the request of one more forgets the run
/// requested longest ago, whose start then adds nothing, and keeps the others.
#[tokio::test]
async fn a_request_beyond_the_most_runs_forgets_the_run_requested_longest_ago() {
    let rig = Rig::new("tscache-starts-most");
    // The oldest is requested last in the order of the keys, so it is found by its time.
    let run = |n: usize| format!("mount-provider/61/{:05}", MOST_RUNS - n);
    for n in 0..MOST_RUNS {
        rig.cache.run_requested(&run(n));
        rig.clock.advance(1);
    }
    assert_eq!(rig.cache.runs_kept(), MOST_RUNS);
    rig.cache.run_requested(&run(MOST_RUNS));
    assert_eq!(rig.cache.runs_kept(), MOST_RUNS);
    assert_eq!(rig.cache.run_started(&format!("{}/0", run(0))), None);
    assert!(rig.cache.run_started(&format!("{}/0", run(1))).is_some());
    assert!(
        rig.cache
            .run_started(&format!("{}/0", run(MOST_RUNS)))
            .is_some()
    );
}

/// Asserts the text of milliseconds in seconds: no trailing zero, no point for whole seconds.
#[test]
fn milliseconds_are_written_as_seconds_with_no_trailing_zero() {
    let cases = [
        (0, "0"),
        (1, "0.001"),
        (10, "0.01"),
        (1_234, "1.234"),
        (2_500, "2.5"),
        (60_000, "60"),
    ];
    for (ms, text) in cases {
        assert_eq!(seconds(ms), text, "{ms} ms");
    }
}
