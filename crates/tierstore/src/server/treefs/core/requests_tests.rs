//! The counts of the requests a core keeps (`core/requests.rs`): each request under its kind in the
//! bucket of its span on the core's clock, a request never answered not counted, and the report,
//! which is sent only when a request was answered since the report before.

use std::time::Duration;

use crate::server::local_cache::attaches::BUCKETS;
use crate::server::local_cache::opens::Spans;
use crate::server::local_cache::opens::requests::{RequestKind, Requests};
use crate::server::treefs::testing::{Rig, sample};

/// Times one request of `kind` that takes `ms` milliseconds on the core's clock of `rig`.
fn timed(rig: &Rig, kind: RequestKind, ms: u64) {
    let arrival = rig.core.request_begins();
    rig.clock.advance(Duration::from_millis(ms));
    rig.core.request_ends(arrival, kind);
}

/// Totals of one kind: one request of `ms` milliseconds, in the bucket `bucket`.
fn one(ms: u64, bucket: usize) -> Spans {
    let mut buckets = [0; BUCKETS];
    buckets[bucket] = 1;
    Spans { ms, buckets }
}

/// Asserts each kind in its own totals and each span in its exact bucket, at the edges of the
/// buckets: 0 ms and 1 ms in the bucket up to 0.001 s, 2 ms in the one up to 0.002 s, 3 ms in
/// the one up to 0.005 s, 300 s in the last bound's and just over it above the last.
#[test]
fn each_request_is_counted_under_its_kind_in_the_bucket_of_its_span() {
    let rig = Rig::new("tfs-requests-kinds", sample());
    timed(&rig, RequestKind::Lookup, 0);
    timed(&rig, RequestKind::Getattr, 1);
    timed(&rig, RequestKind::Access, 2);
    timed(&rig, RequestKind::Open, 3);
    timed(&rig, RequestKind::Read, 300_000);
    timed(&rig, RequestKind::Readdir, 300_001);
    timed(&rig, RequestKind::Readlink, 10);
    timed(&rig, RequestKind::Other, 11);
    let want = Requests {
        mount: "mount-1".to_owned(),
        process: u64::from(std::process::id()),
        spans: [
            one(0, 0),
            one(1, 0),
            one(2, 1),
            one(3, 2),
            one(300_000, BUCKETS - 2),
            one(300_001, BUCKETS - 1),
            one(10, 3),
            one(11, 4),
        ],
    };
    assert_eq!(rig.core.requests_report(), want);
}

/// Asserts that a request that arrived and was never answered (its parked job dropped) counts
/// nothing, while one answered after it counts.
#[test]
fn a_request_never_answered_is_not_counted() {
    let rig = Rig::new("tfs-requests-unanswered", sample());
    let _never = rig.core.request_begins();
    rig.clock.advance(Duration::from_millis(5));
    assert_eq!(rig.core.requests_report().spans, [Spans::default(); 8]);
    timed(&rig, RequestKind::Getattr, 0);
    let mut want = [Spans::default(); 8];
    want[RequestKind::Getattr.index()] = one(0, 0);
    assert_eq!(rig.core.requests_report().spans, want);
}

/// What the node's metrics hold of the lookups: the count and the sum, as the text gives them.
fn lookups(rig: &Rig) -> (String, String) {
    let text = rig.node.metrics();
    let of = |series: &str| {
        let prefix = format!("tierstore_fs_request_seconds_{series}{{kind=\"lookup\"}} ");
        text.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .map(str::to_owned)
            .unwrap_or_default()
    };
    (of("count"), of("sum"))
}

/// Tells the node, as a process of the core's own mount and id, totals ahead of the core's in
/// every kind but lookups. The node counts a report whose totals went down as a process that
/// started anew and adds all of them, so a report the core sends after this one adds its totals
/// to the lookups of the node, where one that grew only adds the growth, nothing for the same
/// totals again: what the core sends then shows in the node's lookups.
fn put_node_ahead(rig: &Rig) {
    let mut spans = [Spans::default(); 8];
    spans[RequestKind::Other.index()] = one(1_000, 0);
    let ahead = Requests {
        spans,
        ..rig.core.requests_report()
    };
    rig.node.requests_reported(ahead);
}

/// Asserts that the report is sent only when a request was answered since the report before: a
/// core that answered none tells the node none; one request, and the node counts it; no request
/// since, and the core tells the node nothing more (the node, put ahead, would count the same
/// totals again if it were told them); one more, and the next report carries both, the totals
/// since the process started. A look reports in process, so the node has what it was told when
/// the look returns.
#[test]
fn the_report_is_sent_only_when_a_request_was_answered_since_the_last() {
    let rig = Rig::new("tfs-requests-report", sample());
    let core = &rig.core;
    core.look_around();
    assert_eq!(lookups(&rig), ("0".to_owned(), "0".to_owned()));
    timed(&rig, RequestKind::Lookup, 4);
    core.look_around();
    assert_eq!(lookups(&rig), ("1".to_owned(), "0.004".to_owned()));
    put_node_ahead(&rig);
    core.look_around();
    assert_eq!(lookups(&rig), ("1".to_owned(), "0.004".to_owned()));
    timed(&rig, RequestKind::Lookup, 1);
    core.look_around();
    // The report of the totals since the start: two lookups of 5 ms, added to the one before.
    assert_eq!(lookups(&rig), ("3".to_owned(), "0.009".to_owned()));
}
