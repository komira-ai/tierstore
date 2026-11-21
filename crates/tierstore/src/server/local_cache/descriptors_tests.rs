//! What the mounts report of their descriptors, as the cache counts and renders it: the report
//! read back as written and every other form refused; the gauge taking the last report and the
//! counter adding up; the mounts rendered apart and in order; the bound on the mounts.

use super::{Descriptors, MOST_MOUNTS};
use crate::server::local_cache::testing::Rig;

fn report(mount: &str, process: u64, held: u64, short: u64) -> Descriptors {
    Descriptors {
        mount: mount.to_owned(),
        process,
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
    let one = report("boot-1", 4242, 12, 3);
    assert_eq!(one.line(), "descriptors boot-1 4242 12 3");
    assert_eq!(Descriptors::parse("boot-1 4242 12 3"), Some(one));
    let most = report("m", u64::MAX, u64::MAX, 0);
    let rest = most.line().replacen("descriptors ", "", 1);
    assert_eq!(Descriptors::parse(&rest), Some(most));
    let too_long = format!("{} 7 1 1", "m".repeat(129));
    let big = format!("m 7 1 {}0", u64::MAX);
    for refused in [
        "",
        "m",
        "m 7",
        "m 7 1",
        "m 7 1 2 3",
        "m  7 1 2",
        " 7 1 2",
        "m 7 1 ",
        "m 7 +1 2",
        "m 7 1 -2",
        "m -7 1 2",
        "m 7 1x 2",
        "m 7 1 2x",
        "m x 1 2",
        "m 7 0x1 2",
        "m 7 1.0 2",
        "m 7 a b",
        "m\u{e9} 7 1 2",
        "m\t7 1 2",
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
fn the_gauge_takes_the_last_report_and_the_counter_what_the_total_grew_by() {
    let rig = Rig::new("descriptors-sum");
    rig.cache.descriptors_reported(report("a", 1, 10, 2));
    rig.cache.descriptors_reported(report("a", 1, 4, 2));
    rig.cache.descriptors_reported(report("a", 1, 7, 7));
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
    rig.cache.descriptors_reported(report("a", 1, 1, u64::MAX));
    rig.cache.descriptors_reported(report("a", 2, 1, 9));
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
    rig.cache.descriptors_reported(report("zed", 1, 3, 1));
    rig.cache.descriptors_reported(report("a\"b\\c", 1, 8, 0));
    rig.cache.descriptors_reported(report("zed", 1, 5, 3));
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
            .descriptors_reported(report(&format!("m{n:03}"), 1, 1, 1));
    }
    rig.cache.descriptors_reported(report("extra", 1, 9, 9));
    rig.cache.descriptors_reported(report("m000", 1, 2, 2));
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

/// Asserts how the counter follows the totals of the processes of one mount: a report that was
/// lost loses nothing the next one carries; the two processes of a hand-over, reporting in turns,
/// each add only what their own total grew by; a process not seen adds its whole total, as does
/// one whose total went down (a new process given the same id); and the totals of the two latest
/// processes only are kept, so one older than those counts as new.
#[test]
fn the_counter_adds_what_each_processs_total_grew_by() {
    let rig = Rig::new("descriptors-processes");
    let counter = |rig: &Rig| {
        families(rig)
            .into_iter()
            .find_map(|line| {
                line.strip_prefix("tierstore_fs_open_descriptor_failures_total{mount=\"a\"} ")
                    .map(str::to_owned)
            })
            .unwrap()
    };
    let steps: [(u64, u64, &str); 9] = [
        (10, 2, "2"),
        // The report of 5 was lost: the next carries it.
        (10, 9, "9"),
        // The successor of a hand-over, and the two in turns.
        (11, 1, "10"),
        (10, 12, "13"),
        (11, 4, "16"),
        (10, 12, "16"),
        // A new process given the id of one before: its total is new.
        (11, 2, "18"),
        // A third process: the first falls out of those kept.
        (12, 5, "23"),
        (10, 12, "35"),
    ];
    for (process, total, want) in steps {
        rig.cache
            .descriptors_reported(report("a", process, 1, total));
        assert_eq!(counter(&rig), want, "after {process} {total}");
    }
}
