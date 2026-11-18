//! The text of each kind of family: counters, gauges, histograms (fed from the injected clock)
//! and state sets, label escaping, series made on first use, and updates from several threads.
//!
//! Catches: a histogram whose buckets are not cumulative, that counts an observation at a bound
//! in the bucket above, or whose count differs from its `+Inf` bucket; a gauge value written in a
//! form the text format does not read (`inf`, `NaN` spelled otherwise); a state set with more
//! than one state at 1; a label value written unescaped; a series lost or double counted under
//! concurrent updates.

use std::thread;

use std::ops::Deref;
use std::sync::Arc;
use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_fetch_api::ids::Class;
use tierstore_format::ids::DriveId;
use tierstore_metrics::labels::{BudgetReason, DriveState, FuseOp, SizeClass};

use tierstore_metrics::{
    Cache, ColdStoreConfig, Drives, Fetch, FetchEngineConfig, Fuse, Kind, Metrics,
};

fn drive(n: u8) -> DriveId {
    let mut id = [0; 16];
    id[15] = n;
    DriveId(id)
}

/// A registry with the fetch engine, the node cache, the FUSE front and a cold store of one
/// drive, and the families each registered.
struct Rig {
    metrics: Metrics,
    fetch: Arc<Fetch>,
    cache: Arc<Cache>,
    fuse: Arc<Fuse>,
    drives: Arc<Drives>,
}

impl Deref for Rig {
    type Target = Metrics;

    fn deref(&self) -> &Metrics {
        &self.metrics
    }
}

fn metrics(version: &'static str) -> Rig {
    let metrics = Metrics::new(version);
    let fetch = metrics.fetch_engine(&FetchEngineConfig {
        drives: vec![drive(1)],
    });
    let cold = metrics.cold_store(&ColdStoreConfig {
        drives: vec![drive(1)],
        codes: Vec::new(),
    });
    Rig {
        fetch: fetch.fetch,
        cache: metrics.node_cache().cache,
        fuse: metrics.fuse_front().fuse,
        drives: cold.drives,
        metrics,
    }
}

/// The `# HELP`, `# TYPE` and sample lines of the family exported as `name`.
fn block(text: &str, name: &str) -> Vec<String> {
    let help = format!("# HELP {name} ");
    let kind = format!("# TYPE {name} ");
    text.lines()
        .filter(|l| {
            l.starts_with(&help)
                || l.starts_with(&kind)
                || l.strip_prefix(name).is_some_and(|rest| {
                    [" ", "{", "_bucket{", "_sum", "_count"]
                        .iter()
                        .any(|s| rest.starts_with(s))
                })
        })
        .map(str::to_owned)
        .collect()
}

/// Asserts the whole text of an unlabelled histogram: one observation at a bound counts in that
/// bound's bucket, one between bounds in the next, one above every bound only in `+Inf`; the
/// buckets are cumulative; the sum is the observations' and the count the `+Inf` bucket's.
#[test]
fn a_histogram_renders_cumulative_buckets_its_sum_and_its_count() {
    let m = metrics("v");
    m.cache.journal_records.observe((), 1.0);
    m.cache.journal_records.observe((), 3.0);
    m.cache.journal_records.observe((), 5000.0);
    let mut expected = vec![
        "# HELP tierstore_journal_records Records in one batch of the cache journal.".to_owned(),
        "# TYPE tierstore_journal_records histogram".to_owned(),
    ];
    let cumulative = [1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2];
    let bounds = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
    for (bound, count) in bounds.iter().zip(cumulative) {
        expected.push(format!(
            "tierstore_journal_records_bucket{{le=\"{bound}\"}} {count}"
        ));
    }
    expected.push("tierstore_journal_records_bucket{le=\"+Inf\"} 3".to_owned());
    expected.push("tierstore_journal_records_sum 5004".to_owned());
    expected.push("tierstore_journal_records_count 3".to_owned());
    assert_eq!(block(&m.render(), "tierstore_journal_records"), expected);
    let series = m.cache.journal_records.with(());
    assert_eq!(series.count(), 3);
    assert_eq!(series.sum(), 5004.0);
    assert_eq!(
        series.cumulative(),
        [1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3]
    );
}

/// Asserts that a duration measured on the manual clock lands in its bucket of a labelled
/// histogram, under its labels only, with the labels before `le`.
#[test]
fn a_duration_from_the_injected_clock_lands_in_its_bucket_under_its_labels() {
    let m = metrics("v");
    let clock = ManualClock::new();
    let start = clock.now_ms();
    clock.advance(250);
    let labels = (Class::Prepare, SizeClass::Large);
    m.fetch
        .part_first_byte_seconds
        .observe_ms(labels, clock.now_ms() - start);
    let text = m.render();
    let lines = block(&text, "tierstore_part_first_byte_seconds");
    let of = |pairs: &str| -> Vec<String> {
        lines
            .iter()
            .filter(|l| l.contains(pairs))
            .cloned()
            .collect()
    };
    let prepare_large = of("class=\"prepare\",size_class=\"large\"");
    assert!(prepare_large.contains(
        &"tierstore_part_first_byte_seconds_bucket{class=\"prepare\",size_class=\"large\",le=\"0.1\"} 0"
            .to_owned()
    ));
    assert!(prepare_large.contains(
        &"tierstore_part_first_byte_seconds_bucket{class=\"prepare\",size_class=\"large\",le=\"0.25\"} 1"
            .to_owned()
    ));
    assert!(
        prepare_large.contains(
            &"tierstore_part_first_byte_seconds_sum{class=\"prepare\",size_class=\"large\"} 0.25"
                .to_owned()
        )
    );
    assert!(
        prepare_large.contains(
            &"tierstore_part_first_byte_seconds_count{class=\"prepare\",size_class=\"large\"} 1"
                .to_owned()
        )
    );
    // Three classes times three size classes, each 18 bounds, +Inf, sum and count.
    assert_eq!(lines.len(), 2 + 9 * 21);
    let others = lines
        .iter()
        .filter(|l| l.ends_with(" 1") || l.ends_with(" 0.25"));
    assert!(
        others
            .clone()
            .all(|l| l.contains("class=\"prepare\",size_class=\"large\""))
    );
    // The ten bounds from 0.25 up, +Inf, the sum and the count.
    assert_eq!(others.count(), 10 + 3);
}

/// Asserts that a gauge writes integers, fractions, NaN and both infinities as the text format
/// reads them, and that add, inc and dec move it.
#[test]
fn a_gauge_writes_each_value_as_the_text_format_reads_it() {
    let m = metrics("v");
    let value = |m: &Rig| block(&m.render(), "tierstore_amplification")[2].clone();
    for (set, text) in [
        (3.0, "3"),
        (1.5, "1.5"),
        (-0.25, "-0.25"),
        (f64::NAN, "NaN"),
        (f64::INFINITY, "+Inf"),
        (f64::NEG_INFINITY, "-Inf"),
    ] {
        m.fetch.amplification.set((), set);
        assert_eq!(value(&m), format!("tierstore_amplification {text}"));
    }
    m.fetch.reads_waiting.inc(());
    m.fetch.reads_waiting.inc(());
    m.fetch.reads_waiting.dec(());
    m.fetch.reads_waiting.add((), 2.5);
    assert_eq!(m.fetch.reads_waiting.with(()).get(), 3.5);
    assert_eq!(
        block(&m.render(), "tierstore_reads_waiting")[2],
        "tierstore_reads_waiting 3.5"
    );
}

/// Asserts that a counter renders its count with and without labels, through the family's
/// shorthand and through a series kept by the caller.
#[test]
fn a_counter_renders_its_count_with_and_without_labels() {
    let m = metrics("v");
    m.fetch.retries_total.inc(());
    m.fetch.retries_total.add((), 4);
    let read = m.fuse.fuse_requests_total.with(FuseOp::Read);
    read.inc();
    read.add(2);
    let text = m.render();
    assert_eq!(
        block(&text, "tierstore_retries_total"),
        [
            "# HELP tierstore_retries_total Parts retried after a stall, a transport failure or a mismatch.",
            "# TYPE tierstore_retries_total counter",
            "tierstore_retries_total 5",
        ]
    );
    let requests = block(&text, "tierstore_fuse_requests_total");
    assert!(requests.contains(&"tierstore_fuse_requests_total{op=\"read\"} 3".to_owned()));
    assert!(requests.contains(&"tierstore_fuse_requests_total{op=\"write\"} 0".to_owned()));
    assert_eq!(requests.len(), 2 + 25);
    assert_eq!(m.fuse.fuse_requests_total.with(FuseOp::Read).get(), 3);
}

/// Asserts that a state set renders every state at 0 before one is set, then exactly the set
/// state at 1, and moves when set again.
#[test]
fn a_state_set_has_exactly_its_current_state_at_one() {
    let m = metrics("v");
    let ones = |m: &Rig| -> Vec<String> {
        block(&m.render(), "tierstore_budget_reason")
            .into_iter()
            .filter(|l| l.ends_with(" 1"))
            .collect()
    };
    let lines = block(&m.render(), "tierstore_budget_reason");
    assert_eq!(lines.len(), 2 + 11);
    assert!(ones(&m).is_empty());
    assert_eq!(m.fetch.budget_reason.with(()).get(), None);
    m.fetch.budget_reason.set((), BudgetReason::Cooldown);
    assert_eq!(ones(&m), ["tierstore_budget_reason{reason=\"cooldown\"} 1"]);
    m.fetch.budget_reason.set((), BudgetReason::Drive503);
    assert_eq!(
        ones(&m),
        ["tierstore_budget_reason{reason=\"drive_503\"} 1"]
    );
    assert_eq!(
        m.fetch.budget_reason.with(()).get(),
        Some(BudgetReason::Drive503)
    );

    m.drives.drive_state.set(drive(1), DriveState::Stalled);
    let states = block(&m.render(), "tierstore_drive_state");
    let id = drive(1).to_string();
    assert_eq!(states.len(), 2 + 8);
    assert_eq!(
        states
            .iter()
            .filter(|l| l.ends_with(" 1"))
            .collect::<Vec<_>>(),
        [&format!(
            "tierstore_drive_state{{drive=\"{id}\",state=\"stalled\"}} 1"
        )]
    );
    let info = m
        .families()
        .into_iter()
        .find(|f| f.design == "drive_state")
        .unwrap();
    assert_eq!(info.labels, ["drive", "state"]);
    assert_eq!(info.kind, Kind::Gauge);
}

/// Asserts that a label value with a quote, a backslash and a newline is written escaped.
#[test]
fn a_label_value_is_escaped() {
    let m = metrics("1.0\"x\\y\nz");
    assert_eq!(
        block(&m.render(), "tierstore_build_info")[2],
        "tierstore_build_info{version=\"1.0\\\"x\\\\y\\nz\"} 1"
    );
}

/// Asserts that a drive outside the configuration has no series until it is used, then has one
/// that renders in label order after the configured drive.
#[test]
fn a_series_is_made_on_first_use_and_renders_in_label_order() {
    let m = metrics("v");
    assert!(m.fetch.parts_per_drive.get(drive(9)).is_none());
    assert_eq!(m.fetch.parts_per_drive.labels(), [drive(1)]);
    m.fetch.parts_per_drive.set(drive(9), 2.0);
    m.fetch.parts_per_drive.set(drive(9), 3.0);
    assert_eq!(m.fetch.parts_per_drive.labels(), [drive(1), drive(9)]);
    assert_eq!(m.fetch.parts_per_drive.get(drive(9)).unwrap().get(), 3.0);
    assert_eq!(
        block(&m.render(), "tierstore_parts_per_drive")[2..],
        [
            format!("tierstore_parts_per_drive{{drive=\"{}\"}} 0", drive(1)),
            format!("tierstore_parts_per_drive{{drive=\"{}\"}} 3", drive(9)),
        ]
    );
}

/// Asserts that counts and observations from eight threads, some making the same new series at
/// once, all land: none lost, none counted twice.
#[test]
fn updates_from_many_threads_all_land() {
    let m = metrics("v");
    thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..1000 {
                    m.fuse.fuse_requests_total.inc(FuseOp::Read);
                    m.fuse.fuse_request_seconds.observe(FuseOp::Read, 0.001);
                    m.fetch.parts_per_drive.inc(drive(7));
                }
            });
        }
    });
    assert_eq!(m.fuse.fuse_requests_total.with(FuseOp::Read).get(), 8000);
    assert_eq!(m.fuse.fuse_request_seconds.with(FuseOp::Read).count(), 8000);
    assert_eq!(m.fetch.parts_per_drive.with(drive(7)).get(), 8000.0);
}

/// Asserts that a NaN observation is not counted and leaves the sum as it was, and that an
/// infinite one counts only in `+Inf`.
#[test]
fn a_nan_observation_is_not_counted_and_an_infinite_one_lands_in_inf() {
    let m = metrics("v");
    let series = m.cache.journal_records.with(());
    series.observe(2.0);
    series.observe(f64::NAN);
    assert_eq!((series.count(), series.sum()), (1, 2.0));
    series.observe(f64::INFINITY);
    let cumulative = series.cumulative();
    assert_eq!(cumulative[cumulative.len() - 2..], [1, 2]);
    assert_eq!(series.sum(), f64::INFINITY);
    assert!(
        block(&m.render(), "tierstore_journal_records")
            .contains(&"tierstore_journal_records_sum +Inf".to_owned())
    );
}
