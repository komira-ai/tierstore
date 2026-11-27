//! What the mounts report of the requests of their frontends, as the cache counts and renders it:
//! the kinds and their words; the report read back as written and every other form refused,
//! another number of kinds or of buckets among them; the family at zero before any report; each
//! process's totals added by what they grew by, across a hand-over and a lost report; the bound
//! on the mounts.

use super::{KINDS, RequestKind, Requests};
use crate::server::local_cache::attaches::BUCKETS;
use crate::server::local_cache::descriptors::MOST_MOUNTS;
use crate::server::local_cache::opens::Spans;
use crate::server::local_cache::testing::Rig;

/// The `le` of each bucket, as rendered.
const LE: [&str; BUCKETS] = [
    "0.001", "0.002", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5",
    "10", "30", "60", "300", "+Inf",
];

/// The words of the kinds, in the order of [`KINDS`].
const WORDS: [&str; 8] = [
    "lookup", "getattr", "access", "open", "read", "readdir", "readlink", "other",
];

/// Totals of one kind: `ms` milliseconds, and `count` requests in the bucket `bucket`.
fn spans(ms: u64, bucket: usize, count: u64) -> Spans {
    let mut buckets = [0; BUCKETS];
    buckets[bucket] = count;
    Spans { ms, buckets }
}

/// A report of `mount` by `process` whose only requests are `count` of the kind `kind` in the
/// first bucket, of `ms` in all.
fn only(mount: &str, process: u64, kind: RequestKind, count: u64, ms: u64) -> Requests {
    let mut all = [Spans::default(); KINDS.len()];
    all[kind.index()] = spans(ms, 0, count);
    Requests {
        mount: mount.to_owned(),
        process,
        spans: all,
    }
}

/// The lines of the family in the cache's `/metricz` text.
fn family(rig: &Rig) -> Vec<String> {
    rig.cache
        .metrics()
        .lines()
        .filter(|line| line.contains("tierstore_fs_request_seconds"))
        .map(str::to_owned)
        .collect()
}

/// The lines of the histogram of `kind` with every bucket at or above `from` holding `count`, of
/// `sum` seconds; every bucket below `from` empty.
fn histogram(kind: &str, from: usize, count: u64, sum: &str) -> Vec<String> {
    let labels = format!("kind=\"{kind}\"");
    let mut lines: Vec<String> = LE
        .iter()
        .enumerate()
        .map(|(at, le)| {
            let n = if at >= from { count } else { 0 };
            format!("tierstore_fs_request_seconds_bucket{{{labels},le=\"{le}\"}} {n}")
        })
        .collect();
    lines.push(format!(
        "tierstore_fs_request_seconds_sum{{{labels}}} {sum}"
    ));
    lines.push(format!(
        "tierstore_fs_request_seconds_count{{{labels}}} {count}"
    ));
    lines
}

/// The count of `kind`, in the cache's text.
fn count(rig: &Rig, kind: &str) -> u64 {
    let prefix = format!("tierstore_fs_request_seconds_count{{kind=\"{kind}\"}} ");
    family(rig)
        .iter()
        .find_map(|line| line.strip_prefix(&prefix).map(|n| n.parse().unwrap()))
        .unwrap()
}

/// Asserts that the kinds are the eight in order, each at the place its index gives it, with its
/// word.
#[test]
fn every_kind_has_its_place_and_its_word() {
    for (at, kind) in KINDS.into_iter().enumerate() {
        assert_eq!(kind.index(), at);
        assert_eq!(kind.word(), WORDS[at]);
    }
}

/// Asserts that a report reads back as written, the largest numbers included, and that a report
/// with a bucket fewer or more (a mount of another release), the cells of a kind fewer, a missing
/// or an extra word, an empty word, a number with a sign or a non-digit, a number over 64 bits, a
/// bad mount name and no numbers at all are refused.
#[test]
fn a_requests_report_reads_back_as_written_and_any_other_form_is_refused() {
    let mut one = only("boot-1", 4242, RequestKind::Lookup, 2, 30);
    one.spans[KINDS.len() - 1] = spans(9, 3, 1);
    let line = one.line();
    let rest = line.strip_prefix("requests ").unwrap();
    assert!(rest.starts_with("boot-1 4242 30 2 0 0"), "{line}");
    assert_eq!(rest.split(' ').count(), 2 + KINDS.len() * (1 + BUCKETS));
    assert_eq!(Requests::parse(rest), Some(one));
    let most = Requests {
        mount: "m".to_owned(),
        process: u64::MAX,
        spans: [spans(u64::MAX, BUCKETS - 1, u64::MAX); KINDS.len()],
    };
    let line = most.line();
    assert_eq!(
        Requests::parse(line.strip_prefix("requests ").unwrap()),
        Some(most)
    );
    let fewer = rest.rsplit_once(' ').unwrap().0.to_owned();
    let more = format!("{rest} 0");
    let kind_fewer = rest
        .split(' ')
        .take(2 + (KINDS.len() - 1) * (1 + BUCKETS))
        .collect::<Vec<_>>()
        .join(" ");
    let big = rest.replacen(" 4242 ", &format!(" {}0 ", u64::MAX), 1);
    let signed = rest.replacen(" 4242 ", " +4242 ", 1);
    let lettered = rest.replacen(" 4242 ", " 4242x ", 1);
    let doubled = rest.replacen(" 4242 ", "  4242 ", 1);
    let named = rest.replacen("boot-1", "boot\u{e9}", 1);
    let unnamed = rest.replacen("boot-1", "", 1);
    for refused in [
        "",
        "boot-1",
        "boot-1 4242",
        fewer.as_str(),
        more.as_str(),
        kind_fewer.as_str(),
        big.as_str(),
        signed.as_str(),
        lettered.as_str(),
        doubled.as_str(),
        named.as_str(),
        unnamed.as_str(),
    ] {
        assert_eq!(Requests::parse(refused), None, "{refused:?}");
    }
}

/// Asserts that before any report the family has its `# TYPE` line and every series of the eight
/// kinds at zero: the whole text, in order.
#[test]
fn the_family_is_at_zero_before_a_report() {
    let rig = Rig::new("requests-none");
    let mut expected = vec!["# TYPE tierstore_fs_request_seconds histogram".to_owned()];
    for word in WORDS {
        expected.extend(histogram(word, 0, 0, "0"));
    }
    assert_eq!(family(&rig), expected);
}

/// Asserts the whole text after one report with a request of each kind in its own bucket: the
/// buckets cumulated from the bucket the request fell in, the sum in seconds and the count.
#[test]
fn a_report_shows_each_kind_in_its_buckets() {
    let rig = Rig::new("requests-one");
    let mut report = only("m", 1, RequestKind::Lookup, 0, 0);
    for (at, spans) in report.spans.iter_mut().enumerate() {
        // The kind at `at` fell in the bucket `2 * at`, of `at + 1` ms.
        *spans = self::spans(at as u64 + 1, 2 * at, 1);
    }
    rig.cache.requests_reported(report);
    let mut expected = vec!["# TYPE tierstore_fs_request_seconds histogram".to_owned()];
    for (at, word) in WORDS.iter().enumerate() {
        let sum = format!("0.00{}", at + 1);
        expected.extend(histogram(word, 2 * at, 1, &sum));
    }
    assert_eq!(family(&rig), expected);
}

/// Asserts how the counts follow the totals of the processes of one mount, by the rule of the
/// opens: a report that was lost loses nothing the next one carries; the two processes of a
/// hand-over, reporting in turns, each add only what their own totals grew by; a process whose
/// totals went down is a new one; and of two mounts each counts its own.
#[test]
fn the_counts_add_what_each_processs_totals_grew_by() {
    let rig = Rig::new("requests-processes");
    let lookups = |rig: &Rig| count(rig, "lookup");
    let lookup = RequestKind::Lookup;
    let steps: [(Requests, u64); 7] = [
        (only("a", 10, lookup, 2, 2), 2),
        // The report of 5 was lost: the next carries it.
        (only("a", 10, lookup, 9, 9), 9),
        // The successor of a hand-over, and the two in turns.
        (only("a", 11, lookup, 1, 1), 10),
        (only("a", 10, lookup, 12, 12), 13),
        (only("a", 11, lookup, 4, 4), 16),
        // A new process given the id of one before: its totals are new.
        (only("a", 11, lookup, 2, 2), 18),
        // Another mount, whose process has the same id.
        (only("b", 10, lookup, 3, 3), 21),
    ];
    for (report, want) in steps {
        let (mount, process) = (report.mount.clone(), report.process);
        rig.cache.requests_reported(report);
        assert_eq!(lookups(&rig), want, "after {mount} {process}");
    }
    assert_eq!(count(&rig, "other"), 0);
}

/// Asserts that a mount beyond [`MOST_MOUNTS`] is dropped, counted nowhere, while the mounts kept
/// go on counting.
#[test]
fn a_mount_beyond_the_bound_is_dropped() {
    let rig = Rig::new("requests-bound");
    let read = RequestKind::Read;
    for n in 0..MOST_MOUNTS {
        rig.cache
            .requests_reported(only(&format!("m{n:03}"), 1, read, 1, 1));
    }
    rig.cache.requests_reported(only("extra", 1, read, 9, 9));
    assert_eq!(count(&rig, "read"), MOST_MOUNTS as u64);
    rig.cache.requests_reported(only("m000", 1, read, 3, 3));
    assert_eq!(count(&rig, "read"), MOST_MOUNTS as u64 + 2);
}
