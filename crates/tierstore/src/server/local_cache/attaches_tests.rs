//! What the mounts report of their attaches, as the cache counts and renders it: the words of the
//! two reports, read back as written and every other form refused; the bucket of a span at each
//! bound; the families at zero; and the reports added up, rendered line for line.

use super::{Attach, BOUNDS_MS, BUCKETS, Fetches, Outcome, bucket_of};
use crate::server::local_cache::testing::Rig;

/// The lines of the three families in the cache's `/metricz` text.
fn families(rig: &Rig) -> Vec<String> {
    rig.cache
        .metrics()
        .lines()
        .filter(|line| line.contains("tierstore_fs_"))
        .map(str::to_owned)
        .collect()
}

/// The bounds of the buckets as rendered, `+Inf` last.
const LE: [&str; BUCKETS] = [
    "0.001", "0.002", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10",
    "30", "60", "300", "+Inf",
];

/// The lines of the histogram `name` with `label` (`outcome="ok",` or nothing) whose cumulative
/// bucket counts are `below`, and whose sum is `sum` as rendered.
fn histogram(name: &str, label: &str, below: [u64; BUCKETS], sum: &str) -> Vec<String> {
    let mut lines: Vec<String> = LE
        .iter()
        .zip(below)
        .map(|(le, n)| format!("{name}_bucket{{{label}le=\"{le}\"}} {n}"))
        .collect();
    let alone = label.trim_end_matches(',');
    let braces = if alone.is_empty() {
        String::new()
    } else {
        format!("{{{alone}}}")
    };
    lines.push(format!("{name}_sum{braces} {sum}"));
    lines.push(format!("{name}_count{braces} {}", below[BUCKETS - 1]));
    lines
}

/// The text of the three families: the attaches by outcome (`ok`, `absent`, `failed`), the
/// manifests (`cache`, `fill`, `error`) and the fills, each histogram given as its cumulative
/// counts and its sum.
fn expected(
    attaches: [([u64; BUCKETS], &str); 3],
    fetches: [u64; 3],
    fills: ([u64; BUCKETS], &str),
) -> Vec<String> {
    let attach = "tierstore_fs_tree_attach_seconds";
    let mut lines = vec![format!("# TYPE {attach} histogram")];
    for (outcome, (below, sum)) in ["ok", "absent", "failed"].iter().zip(attaches) {
        let label = format!("outcome=\"{outcome}\",");
        lines.extend(histogram(attach, &label, below, sum));
    }
    lines.push("# TYPE tierstore_fs_manifest_fetch_total counter".to_owned());
    for (result, n) in ["cache", "fill", "error"].iter().zip(fetches) {
        lines.push(format!("tierstore_fs_manifest_fetch_total{{result=\"{result}\"}} {n}"));
    }
    let fill = "tierstore_fs_manifest_fill_seconds";
    lines.push(format!("# TYPE {fill} histogram"));
    lines.extend(histogram(fill, "", fills.0, fills.1));
    lines
}

/// The cumulative counts of spans of which `per[i]` fell in the bucket `i`.
fn cumulated(per: [u64; BUCKETS]) -> [u64; BUCKETS] {
    let mut below = 0;
    per.map(|n| {
        below += n;
        below
    })
}

/// Asserts that each report reads back as it is written, an outcome of each kind and counts in
/// every bucket, and that every other form is refused: no words, an unknown outcome, a missing or
/// extra word, a number with a sign or not decimal, one bucket too few or too many.
#[test]
fn a_report_reads_back_as_written_and_any_other_form_is_refused() {
    for (outcome, word) in [
        (Outcome::Ok, "ok"),
        (Outcome::Absent, "absent"),
        (Outcome::Failed, "failed"),
    ] {
        let attach = Attach { outcome, ms: 245 };
        assert_eq!(attach.line(), format!("attached {word} 245"));
        let rest = attach.line().replacen("attached ", "", 1);
        assert_eq!(Attach::parse(&rest), Some(attach));
    }
    for refused in [
        "", "ok", "ok ", "done 1", "ok +1", "ok -1", "ok 1 2", "ok 1x", "OK 1",
    ] {
        assert_eq!(Attach::parse(refused), None, "{refused:?}");
    }
    let fetches = Fetches {
        cache: 38,
        filled: 2,
        failed: 1,
        fill_ms: 61,
        fills: std::array::from_fn(|bucket| bucket as u64),
    };
    let line = fetches.line();
    assert_eq!(
        line,
        "fetched 38 2 1 61 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16"
    );
    let rest = line.replacen("fetched ", "", 1);
    assert_eq!(Fetches::parse(&rest), Some(fetches));
    assert_eq!(fetches.manifests(), 41);
    let short = rest.rsplit_once(' ').unwrap().0.to_owned();
    for refused in [
        String::new(),
        "1 2 3".to_owned(),
        short,
        format!("{rest} 17"),
        rest.replacen("38", "+38", 1),
        rest.replacen("38", "3.8", 1),
        rest.replacen(' ', "  ", 1),
    ] {
        assert_eq!(Fetches::parse(&refused), None, "{refused:?}");
    }
}

/// Asserts that a span falls in the bucket of the first bound at or over it: 0 and 1 ms in the
/// first, a bound in its own bucket and one more in the next, and over the last bound in the one
/// above it.
#[test]
fn a_span_falls_in_the_bucket_of_the_first_bound_at_or_over_it() {
    assert_eq!(bucket_of(0), 0);
    assert_eq!(bucket_of(1), 0);
    assert_eq!(bucket_of(2), 1);
    assert_eq!(bucket_of(3), 2);
    assert_eq!(bucket_of(100), 6);
    assert_eq!(bucket_of(101), 7);
    assert_eq!(bucket_of(300_000), BOUNDS_MS.len() - 1);
    assert_eq!(bucket_of(300_001), BOUNDS_MS.len());
    assert_eq!(bucket_of(u64::MAX), BOUNDS_MS.len());
}

/// Asserts that a cache that has had no report renders the three families whole, at zero, with
/// the bounds of the buckets in seconds from 1 ms to 300 s.
#[tokio::test]
async fn a_cache_with_no_report_renders_every_series_at_zero() {
    let rig = Rig::new("tscache-attaches-zero");
    let zero = ([0; BUCKETS], "0");
    assert_eq!(families(&rig), expected([zero; 3], [0; 3], zero));
}

/// Asserts that the reports add up: attaches of 0, 3 and 245 ms that were attached, one of 1 ms
/// that was absent and one of 400000 ms (over the last bound) that failed; and two reads, one all
/// from the cache and one with fills, whose counts and fill buckets add to those of the first.
#[tokio::test]
async fn reports_add_up_by_outcome_and_by_where_the_manifests_came_from() {
    let rig = Rig::new("tscache-attaches-sum");
    for (outcome, ms) in [
        (Outcome::Ok, 0),
        (Outcome::Ok, 3),
        (Outcome::Ok, 245),
        (Outcome::Absent, 1),
        (Outcome::Failed, 400_000),
    ] {
        rig.cache.tree_attached(Attach { outcome, ms });
    }
    let warm = Fetches {
        cache: 40,
        ..Fetches::default()
    };
    let mut cold = Fetches {
        cache: 1,
        filled: 3,
        failed: 1,
        fill_ms: 4 + 20 + 30 + 40,
        ..Fetches::default()
    };
    cold.fills[2] = 1;
    cold.fills[4] = 1;
    cold.fills[5] = 2;
    rig.cache.manifests_fetched(&warm);
    rig.cache.manifests_fetched(&cold);

    let mut ok = [0; BUCKETS];
    ok[0] = 1;
    ok[2] = 1;
    ok[7] = 1;
    let mut absent = [0; BUCKETS];
    absent[0] = 1;
    let mut failed = [0; BUCKETS];
    failed[BUCKETS - 1] = 1;
    let attaches = [
        (cumulated(ok), "0.248"),
        (cumulated(absent), "0.001"),
        (cumulated(failed), "400"),
    ];
    let fills = (cumulated(cold.fills), "0.094");
    assert_eq!(families(&rig), expected(attaches, [41, 3, 1], fills));
}
