//! The counts of the requests a core keeps (`core/requests.rs`): each request under its kind in the
//! bucket of its span on the core's clock, a request never answered not counted, and the report,
//! which is sent only when a request was answered since the report before.

use std::time::Duration;

use crate::server::local_cache::attaches::BUCKETS;
use crate::server::local_cache::opens::Spans;
use crate::server::local_cache::requests::{RequestKind, Requests};
use crate::server::treefs::testing::{Rig, sample, until};

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

/// Asserts that the report is sent only when a request was answered since the report before: a
/// core that answered none sends none; one request, and the report carries it; no request
/// since, and nothing more is sent; one more, and the next report carries both, the totals since
/// the process started. Each look sends `opens` too, in order on the same thread, so once its
/// `opens` is there a `requests` it sent would be.
#[test]
fn the_report_is_sent_only_when_a_request_was_answered_since_the_last() {
    let rig = Rig::new("tfs-requests-report", sample());
    let core = &rig.core;
    let told = || rig.upstream.asked("requests");
    let looked = |looks: usize| {
        core.look_around();
        until(|| rig.upstream.asked("opens").len() == looks);
    };
    looked(1);
    assert_eq!(told(), Vec::<String>::new());
    timed(&rig, RequestKind::Lookup, 4);
    looked(2);
    assert_eq!(told(), [core.requests_report().line()]);
    looked(3);
    assert_eq!(told().len(), 1);
    timed(&rig, RequestKind::Lookup, 1);
    looked(4);
    let told = told();
    assert_eq!(told.len(), 2);
    let last = Requests::parse(told[1].strip_prefix("requests ").unwrap()).unwrap();
    let mut both = [0; BUCKETS];
    both[0] = 1;
    both[2] = 1;
    assert_eq!(
        last.spans[RequestKind::Lookup.index()],
        Spans {
            ms: 5,
            buckets: both
        }
    );
}
