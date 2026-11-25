//! The counts of the opens a core keeps (`core/opens.rs`): each open under its outcome and source in
//! the bucket of its span on the core's clock; the opens waiting from their timing to its end or
//! drop; and the report the look for idle trees sends, which is those counts.

use std::time::Duration;

use super::super::Opened;
use crate::server::local_cache::attaches::BUCKETS;
use crate::server::local_cache::opens::{Opens, Spans};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Rig, sample, sha, until};

/// The entries of the sample tree (`testing::sample`), numbered 1: the index of each.
const BIN: u32 = 2;
const TOOL: u32 = 4;
const EMPTY: u32 = 5;
const A_H: u32 = 7;

/// A rig with the sample tree attached.
fn attached(name: &str) -> Rig {
    let rig = Rig::new(name, sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    rig
}

/// Opens the entry `index` of the sample tree as a frontend does, `ms` milliseconds passing on the
/// core's clock between the arrival of the request and the open: whether it gave a handle, which
/// is closed again.
fn timed(rig: &Rig, index: u32, ms: u64) -> bool {
    let core = &rig.core;
    let mut timing = core.open_begins();
    rig.clock.advance(Duration::from_millis(ms));
    let opened = core.open_from(ino::of(1, index), false, &mut timing);
    let ok = opened.is_ok();
    if let Ok(Opened::Cached(fh) | Opened::Adopted { fh, .. }) = opened {
        core.release(fh);
    }
    core.open_ends(timing, ok);
    ok
}

/// Totals of one kind: `count` opens in the bucket `bucket`, of `ms` milliseconds in all.
fn spans(ms: u64, bucket: usize, count: u64) -> Spans {
    let mut buckets = [0; BUCKETS];
    buckets[bucket] = count;
    Spans { ms, buckets }
}

/// Asserts each kind of open in its exact bucket: the first open of a file the cache does not
/// hold fills it (`ok`, `filled`, 30 ms: the bucket up to 0.05 s); the next opens of it and of an
/// empty file find it (`ok`, `cached`, 2 ms and none: the buckets up to 0.002 s and 0.001 s); a
/// file whose fill fails is `error` and `filled` (7 s: up to 10 s); a directory, refused without
/// asking the node, is `error` and `cached` (just over 300 s: above the last bound).
#[test]
fn each_open_is_counted_by_outcome_and_source_in_the_bucket_of_its_span() {
    let rig = attached("tfs-opens-kinds");
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(sha(b"int a;"), "upstream 404".to_owned());
    assert!(timed(&rig, TOOL, 30));
    assert!(timed(&rig, TOOL, 2));
    assert!(timed(&rig, EMPTY, 0));
    assert!(!timed(&rig, A_H, 7_000));
    assert!(!timed(&rig, BIN, 300_001));
    let mut cached = spans(2, 1, 1);
    cached.buckets[0] = 1;
    let want = Opens {
        mount: "mount-1".to_owned(),
        process: u64::from(std::process::id()),
        waiting: 0,
        spans: [
            cached,
            spans(30, 5, 1),
            spans(300_001, BUCKETS - 1, 1),
            spans(7_000, 12, 1),
        ],
    };
    assert_eq!(rig.core.opens_report(), want);
}

/// Asserts that an open waits from its timing on, and no longer once its timing ends or is
/// dropped unanswered (its request dropped, its thread gone): one that ends is counted, one
/// dropped is not.
#[test]
fn an_open_waits_until_its_timing_ends_or_is_dropped() {
    let rig = attached("tfs-opens-waiting");
    let core = &rig.core;
    let timings: Vec<_> = (0..3).map(|_| core.open_begins()).collect();
    assert_eq!(core.opens_report().waiting, 3);
    let mut timings = timings.into_iter();
    core.open_ends(timings.next().unwrap(), true);
    assert_eq!(core.opens_report().waiting, 2);
    drop(timings);
    let report = core.opens_report();
    assert_eq!(report.waiting, 0);
    let counted: u64 = report.spans.iter().flat_map(|spans| spans.buckets).sum();
    assert_eq!(counted, 1);
}

/// Asserts that the look for idle trees tells the node the counts as the core has them, totals
/// that the next look carries again, grown by what came between.
#[test]
fn the_look_for_idle_trees_reports_the_opens() {
    let rig = attached("tfs-opens-look");
    let core = &rig.core;
    let told = || rig.upstream.asked("opens");
    assert!(timed(&rig, EMPTY, 3));
    core.look_around();
    until(|| told().len() == 1);
    assert_eq!(told()[0], core.opens_report().line());
    assert!(timed(&rig, EMPTY, 4));
    core.look_around();
    until(|| told().len() == 2);
    let last = Opens::parse(told()[1].strip_prefix("opens ").unwrap()).unwrap();
    // 3 ms and 4 ms: both in the bucket up to 0.005 s.
    assert_eq!(last.spans[0], spans(7, 2, 2));
}
