//! The signs of a stall of the fills: the gauges of the requests that wait and the fills that run,
//! the reads of the upstream in flight, the answers `busy`, and what the watchdog and the busy
//! log say. The clock of the cache is the test clock, moved by hand, and the tokio clock is
//! paused: no test waits for time to pass.

use std::sync::Arc;
use std::time::Duration;

use super::extents::RangeWant;
use super::pack_testing::Pack;
use super::source::FillError;
use super::stall::{BUSY_EVERY, BusyLog, Counts, Report, STALL_AFTER, Watch};
use super::testing::{Rig, digest, until};
use crate::ByteRange;

const WAITING: &str = "tierstore_cache_fill_socket_waiting";
const RUNNING: &str = "tierstore_cache_fill_socket_running";
const IN_FLIGHT: &str = "tierstore_cache_upstream_reads_in_flight";
const BUSY: &str = "tierstore_cache_fill_socket_busy_total";

/// The value of the sample `name` (one without labels) in the text of `/metricz` now.
fn sample(rig: &Rig, name: &str) -> u64 {
    let text = rig.cache.metrics();
    let prefix = format!("{name} ");
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no sample {name} in\n{text}"));
    line.parse().expect("a number")
}

/// A rig whose fill socket has one permit and waits for it for `wait`.
fn rig_of(name: &str, wait: Duration) -> Rig {
    Rig::with(name, |config| {
        config.fill_socket_concurrency = 1;
        config.fill_socket_wait = wait;
    })
}

/// A fill of the blob `i` of `pack` on a task of its own.
fn fill_on_task(
    rig: &Rig,
    pack: &Pack,
    i: usize,
) -> tokio::task::JoinHandle<Result<(), FillError>> {
    let cache = rig.cache.clone();
    let (sha, from) = (pack.blobs[i].0.clone(), pack.key.clone());
    let (offset, size) = (pack.blobs[i].1, pack.blobs[i].2);
    tokio::spawn(async move {
        let blob = super::source::Blob {
            sha256: &sha,
            size,
            from: &from,
            offset,
        };
        cache.fill_blob(&blob).await
    })
}

/// Asserts that the gauges of the fill socket count a fill that holds the permit and is parked
/// at the upstream (running 1, one read in flight), and a second request that waits for the
/// permit (waiting 1), as `/metricz` shows them; that a request whose future is dropped while it
/// waits counts down; and that every gauge is 0 once the fills have ended.
#[tokio::test(start_paused = true)]
async fn the_gauges_count_a_waiting_request_a_running_fill_and_a_read_in_flight() {
    let rig = rig_of("tstall-gauges", Duration::from_secs(600));
    let pack = Pack::put(&rig, "packs/gauges", &[400, 500]).await;
    assert_eq!(
        [WAITING, RUNNING, IN_FLIGHT, BUSY].map(|name| sample(&rig, name)),
        [0, 0, 0, 0]
    );
    let gate = rig.tamper.hold();
    let first = fill_on_task(&rig, &pack, 0);
    until("the first fill is at the upstream", || {
        sample(&rig, IN_FLIGHT) == 1
    })
    .await;
    assert_eq!(sample(&rig, RUNNING), 1);
    assert_eq!(sample(&rig, WAITING), 0);
    let second = fill_on_task(&rig, &pack, 1);
    until("the second request waits for a permit", || {
        sample(&rig, WAITING) == 1
    })
    .await;
    assert_eq!(sample(&rig, RUNNING), 1);
    assert_eq!(sample(&rig, IN_FLIGHT), 1);
    second.abort();
    assert!(second.await.unwrap_err().is_cancelled());
    assert_eq!(sample(&rig, WAITING), 0);
    let third = fill_on_task(&rig, &pack, 1);
    until("the third request waits for a permit", || {
        sample(&rig, WAITING) == 1
    })
    .await;
    gate.add_permits(16);
    assert_eq!(first.await.unwrap(), Ok(()));
    assert_eq!(third.await.unwrap(), Ok(()));
    until("every gauge is back at 0", || {
        [WAITING, RUNNING, IN_FLIGHT]
            .iter()
            .all(|name| sample(&rig, name) == 0)
    })
    .await;
    assert_eq!(sample(&rig, BUSY), 0);
}

/// Asserts that a HEAD and a GET parked at the upstream each count one read in flight, that the
/// count goes back to 0 when the call is answered, and that it goes back to 0 when the future of
/// the call is dropped while it is parked.
#[tokio::test]
async fn an_upstream_call_counts_in_flight_until_it_is_answered_or_dropped() {
    let rig = Rig::new("tstall-flight");
    let pack = Pack::put(&rig, "packs/flight", &[400]).await;
    let inner = Arc::clone(&rig.cache.inner);
    let heads = rig.tamper.hold_heads();
    let head = {
        let (inner, key) = (Arc::clone(&inner), pack.key.clone());
        tokio::spawn(async move { inner.upstream_head(&key).await })
    };
    until("the HEAD is parked", || rig.tamper.heading() == 1).await;
    assert_eq!(sample(&rig, IN_FLIGHT), 1);
    heads.add_permits(1);
    head.await.unwrap().unwrap();
    assert_eq!(sample(&rig, IN_FLIGHT), 0);

    let gate = rig.tamper.hold();
    let range = ByteRange::new(0, 100);
    let get = {
        let (inner, key) = (Arc::clone(&inner), pack.key.clone());
        tokio::spawn(async move { inner.upstream_get(&key, range).await })
    };
    until("the GET is parked", || sample(&rig, IN_FLIGHT) == 1).await;
    get.abort();
    assert!(get.await.unwrap_err().is_cancelled());
    assert_eq!(sample(&rig, IN_FLIGHT), 0);

    let within = {
        let (inner, key) = (Arc::clone(&inner), pack.key.clone());
        tokio::spawn(async move { inner.upstream_get_within(&key, range, 100).await })
    };
    until("the streamed GET is parked", || {
        sample(&rig, IN_FLIGHT) == 1
    })
    .await;
    gate.add_permits(4);
    assert_eq!(within.await.unwrap().unwrap().len(), 100);
    assert_eq!(sample(&rig, IN_FLIGHT), 0);
}

/// Asserts that a blob request and a range request that find no permit free within
/// `fill_socket_wait` are answered `busy` and each counted in
/// `tierstore_cache_fill_socket_busy_total`, and that the permit held meanwhile shows as one fill
/// running.
#[tokio::test(start_paused = true)]
async fn requests_answered_busy_are_counted() {
    let rig = rig_of("tstall-busy", Duration::from_secs(5));
    let pack = Pack::put(&rig, "cas/pack/busy", &[400, 500]).await;
    let held = Arc::clone(&rig.cache.inner.source_permits)
        .try_acquire_owned()
        .expect("the only permit");
    assert_eq!(sample(&rig, RUNNING), 1);
    assert_eq!(
        rig.cache.fill_blob(&pack.blob(0)).await,
        Err(FillError::Busy)
    );
    assert_eq!(sample(&rig, BUSY), 1);
    let sha = digest(&pack.bytes()[..100]);
    let want = RangeWant {
        pack: &pack.key,
        known: None,
        offset: 0,
        len: 100,
        sha256: &sha,
    };
    assert!(rig.cache.fill_range(&want).await.is_err());
    assert_eq!(sample(&rig, BUSY), 2);
    assert_eq!(sample(&rig, WAITING), 0);
    drop(held);
    assert_eq!(sample(&rig, RUNNING), 0);
}

/// The counts of a stall: requests wait, nothing is in flight.
fn stalled() -> Counts {
    Counts {
        waiting: 3,
        running: 2,
        in_flight: 0,
    }
}

/// The text of the warning of a stall that has lasted `secs` seconds.
fn warning(secs: u64) -> Report {
    Report::Stalled(format!(
        "tierstore: local cache: 3 fill requests have waited {secs} s with no upstream read in \
         flight (2 fills hold a permit)"
    ))
}

/// Asserts the decision of the watchdog on the clock of the cache: nothing before 30 s of
/// waiting with no read in flight, the warning at 30 s with the counts in its text, nothing
/// between, the warning again at 60 s, and, when the requests stop waiting, the line that the
/// fills move again, once.
#[test]
fn the_watchdog_warns_at_thirty_seconds_repeats_and_says_when_it_clears() {
    let mut watch = Watch::default();
    assert_eq!(STALL_AFTER, 30_000);
    assert_eq!(watch.judge(stalled(), 5_000), None);
    assert_eq!(watch.judge(stalled(), 34_999), None);
    assert_eq!(watch.judge(stalled(), 35_000), Some(warning(30)));
    assert_eq!(watch.judge(stalled(), 36_000), None);
    assert_eq!(watch.judge(stalled(), 64_999), None);
    assert_eq!(watch.judge(stalled(), 65_000), Some(warning(60)));
    let idle = Counts {
        waiting: 0,
        running: 0,
        in_flight: 0,
    };
    assert_eq!(
        watch.judge(idle, 66_000),
        Some(Report::Moved(
            "tierstore: local cache: the fills move again".to_owned()
        ))
    );
    assert_eq!(watch.judge(idle, 67_000), None);
    assert_eq!(watch.judge(stalled(), 68_000), None);
}

/// Asserts that requests that wait while an upstream read is in flight are no stall: nothing is
/// said however long it lasts, a stall that was warned of is cleared when a read comes in
/// flight, and a wait that is broken by a read starts its 30 s again.
#[test]
fn the_watchdog_is_quiet_while_an_upstream_read_is_in_flight() {
    let mut watch = Watch::default();
    let moving = Counts {
        waiting: 3,
        running: 2,
        in_flight: 1,
    };
    for now in [0, 30_000, 90_000, 600_000] {
        assert_eq!(watch.judge(moving, now), None);
    }
    assert_eq!(watch.judge(stalled(), 600_000), None);
    assert_eq!(watch.judge(moving, 620_000), None);
    assert_eq!(watch.judge(stalled(), 630_000), None);
    assert_eq!(watch.judge(stalled(), 659_999), None);
    assert_eq!(watch.judge(stalled(), 660_000), Some(warning(30)));
    assert_eq!(
        watch.judge(moving, 661_000),
        Some(Report::Moved(
            "tierstore: local cache: the fills move again".to_owned()
        ))
    );
}

/// Asserts the watchdog on a cache: requests that wait for the one permit while it is held and
/// nothing is asked of the upstream are said at 30 s of the cache's clock with the counts the
/// gauges have, and a request that gives up ends the stall, said once.
#[tokio::test(start_paused = true)]
async fn the_watchdog_reads_the_gauges_of_the_cache() {
    let rig = rig_of("tstall-watch", Duration::from_secs(3600));
    let pack = Pack::put(&rig, "packs/watch", &[400]).await;
    let held = Arc::clone(&rig.cache.inner.source_permits)
        .try_acquire_owned()
        .expect("the only permit");
    let waiting = fill_on_task(&rig, &pack, 0);
    until("the request waits", || sample(&rig, WAITING) == 1).await;
    let mut watch = Watch::default();
    let inner = &rig.cache.inner;
    assert_eq!(inner.watch_fills(&mut watch), None);
    rig.clock.advance(29_999);
    assert_eq!(inner.watch_fills(&mut watch), None);
    rig.clock.advance(1);
    assert_eq!(
        inner.watch_fills(&mut watch),
        Some(Report::Stalled(
            "tierstore: local cache: 1 fill requests have waited 30 s with no upstream read in \
             flight (1 fills hold a permit)"
                .to_owned()
        ))
    );
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    assert_eq!(
        inner.watch_fills(&mut watch),
        Some(Report::Moved(
            "tierstore: local cache: the fills move again".to_owned()
        ))
    );
    drop(held);
}

/// Asserts the busy log: the first answer is said at once with its count of 1, those within 10 s
/// are not said but counted, the next one after 10 s says the count of all since, and the text
/// has the limit of the pool.
#[test]
fn busy_answers_are_said_at_most_once_in_ten_seconds_with_the_count_of_the_rest() {
    let mut log = BusyLog::default();
    let said = |n: u64| {
        Some(format!(
            "tierstore: local cache: {n} fill requests were answered busy: no permit of \
             fill_socket_concurrency (16) came free within fill_socket_wait"
        ))
    };
    assert_eq!(BUSY_EVERY, 10_000);
    assert_eq!(log.answered(1_000, 16), said(1));
    assert_eq!(log.answered(1_001, 16), None);
    assert_eq!(log.answered(10_999, 16), None);
    assert_eq!(log.answered(11_000, 16), said(3));
    assert_eq!(log.answered(21_000, 16), said(1));
}

/// Asserts that the watchdog's loop looks once a second on the tokio clock and goes on through a
/// stall and its end, and that it ends when it is halted.
#[tokio::test(start_paused = true)]
async fn the_watchdog_loop_looks_through_a_stall_and_ends_when_halted() {
    let rig = rig_of("tstall-loop", Duration::from_secs(3600));
    let pack = Pack::put(&rig, "packs/loop", &[400]).await;
    let held = Arc::clone(&rig.cache.inner.source_permits)
        .try_acquire_owned()
        .expect("the only permit");
    let waiting = fill_on_task(&rig, &pack, 0);
    until("the request waits", || sample(&rig, WAITING) == 1).await;
    let halt = Arc::new(tokio::sync::Notify::new());
    let cache = rig.cache.clone();
    let stop = Arc::clone(&halt);
    let watching = tokio::spawn(async move { cache.watch_fills(stop).await });
    // The first look starts the stall; 30 s later on the cache clock it is said.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    rig.clock.advance(STALL_AFTER);
    tokio::time::sleep(Duration::from_secs(1)).await;
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!watching.is_finished());
    halt.notify_one();
    watching.await.expect("the loop ends");
    drop(held);
}
