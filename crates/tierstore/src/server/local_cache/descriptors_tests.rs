//! What the mounts report of their descriptors, as the cache counts and renders it: the report
//! read back as written and every other form refused; the gauge taking the last report and the
//! counter adding up; the mounts rendered apart and in order; the bound on the mounts.

use super::{Descriptors, MOST_MOUNTS};
use crate::server::local_cache::testing::Rig;

fn report(mount: &str, held: u64, short: u64) -> Descriptors {
    Descriptors {
        mount: mount.to_owned(),
        held,
        short,
    }
}

/// The lines of the two families in the cache's `/metricz` text.
fn families(rig: &Rig) -> Vec<String> {
    rig.cache
        .metrics()
        .lines()
        .filter(|line| {
            line.contains("tierstore_fs_cache_descriptors")
                || line.contains("tierstore_fs_open_descriptor_failures_total")
        })
        .map(str::to_owned)
        .collect()
}

/// Asserts that a report reads back as written, and that a missing or extra word, an empty word,
/// a number with a sign, a space or a non-digit, a number over 64 bits and a bad mount name are
/// refused.
#[test]
fn a_descriptors_report_reads_back_as_written_and_any_other_form_is_refused() {
    let one = report("boot-1", 12, 3);
    assert_eq!(one.line(), "descriptors boot-1 12 3");
    assert_eq!(Descriptors::parse("boot-1 12 3"), Some(one));
    let most = report("m", u64::MAX, 0);
    let rest = most.line().replacen("descriptors ", "", 1);
    assert_eq!(Descriptors::parse(&rest), Some(most));
    let too_long = format!("{} 1 1", "m".repeat(129));
    let big = format!("m 1 {}0", u64::MAX);
    for refused in [
        "",
        "m",
        "m 1",
        "m 1 2 3",
        "m  1 2",
        " 1 2",
        "m 1 ",
        "m +1 2",
        "m 1 -2",
        "m 1x 2",
        "m 1 2x",
        "m 0x1 2",
        "m 1.0 2",
        "m a b",
        "m\u{e9} 1 2",
        "m\t1 2",
        &too_long,
        &big,
    ] {
        assert_eq!(Descriptors::parse(refused), None, "{refused:?}");
    }
}

/// Asserts that before any report the families have their `# TYPE` lines and no series.
#[test]
fn the_families_have_no_series_before_a_report() {
    let rig = Rig::new("descriptors-none");
    assert_eq!(
        families(&rig),
        [
            "# TYPE tierstore_fs_cache_descriptors gauge",
            "# TYPE tierstore_fs_open_descriptor_failures_total counter",
        ]
    );
}

/// Asserts that the gauge shows the last `held` of a mount, and that the counter adds the `short`
/// of every report of it, whatever the `held` did in between.
#[test]
fn the_gauge_takes_the_last_report_and_the_counter_sums_the_short() {
    let rig = Rig::new("descriptors-sum");
    rig.cache.descriptors_reported(report("a", 10, 2));
    rig.cache.descriptors_reported(report("a", 4, 0));
    rig.cache.descriptors_reported(report("a", 7, 5));
    assert_eq!(
        families(&rig),
        [
            "# TYPE tierstore_fs_cache_descriptors gauge",
            "tierstore_fs_cache_descriptors{mount=\"a\"} 7",
            "# TYPE tierstore_fs_open_descriptor_failures_total counter",
            "tierstore_fs_open_descriptor_failures_total{mount=\"a\"} 7",
        ]
    );
}

/// Asserts that the counter stops at the largest number instead of wrapping.
#[test]
fn the_counter_saturates() {
    let rig = Rig::new("descriptors-saturate");
    rig.cache.descriptors_reported(report("a", 1, u64::MAX));
    rig.cache.descriptors_reported(report("a", 1, 9));
    let line = format!(
        "tierstore_fs_open_descriptor_failures_total{{mount=\"a\"}} {}",
        u64::MAX
    );
    assert!(families(&rig).contains(&line), "{line}");
}

/// Asserts the whole text for two mounts reported out of order: each its own series, in the order
/// of the ids, a label with a quote or a backslash escaped, the gauges before the counters.
#[test]
fn two_mounts_render_apart_in_the_order_of_their_ids() {
    let rig = Rig::new("descriptors-two");
    rig.cache.descriptors_reported(report("zed", 3, 1));
    rig.cache.descriptors_reported(report("a\"b\\c", 8, 0));
    rig.cache.descriptors_reported(report("zed", 5, 2));
    let expected = "# TYPE tierstore_fs_cache_descriptors gauge\n\
        tierstore_fs_cache_descriptors{mount=\"a\\\"b\\\\c\"} 8\n\
        tierstore_fs_cache_descriptors{mount=\"zed\"} 5\n\
        # TYPE tierstore_fs_open_descriptor_failures_total counter\n\
        tierstore_fs_open_descriptor_failures_total{mount=\"a\\\"b\\\\c\"} 0\n\
        tierstore_fs_open_descriptor_failures_total{mount=\"zed\"} 3\n";
    let metrics = rig.cache.metrics();
    assert!(metrics.contains(expected), "{metrics}");
}

/// Asserts that a mount beyond [`MOST_MOUNTS`] is dropped, while the mounts kept go on counting.
#[test]
fn a_mount_beyond_the_bound_is_dropped() {
    let rig = Rig::new("descriptors-bound");
    for n in 0..MOST_MOUNTS {
        rig.cache
            .descriptors_reported(report(&format!("m{n:03}"), 1, 1));
    }
    rig.cache.descriptors_reported(report("extra", 9, 9));
    rig.cache.descriptors_reported(report("m000", 2, 1));
    let lines = families(&rig);
    assert_eq!(lines.len(), 2 + 2 * MOST_MOUNTS);
    let text = lines.join("\n");
    assert!(!text.contains("extra"), "{text}");
    assert!(
        text.contains("tierstore_fs_cache_descriptors{mount=\"m000\"} 2"),
        "{text}"
    );
    assert!(
        text.contains("tierstore_fs_open_descriptor_failures_total{mount=\"m000\"} 2"),
        "{text}"
    );
}
