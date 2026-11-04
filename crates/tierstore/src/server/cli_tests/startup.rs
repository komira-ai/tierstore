//! The reconciler's loop and the exit statuses of a failed start.

use super::*;

/// Asserts that the reconciler's loop works at once and then at every interval, tries again at
/// the next interval a copy that failed, and gives its lease up when it is halted: the copy that
/// failed in the first pass is made in the second, and the lease is gone after the halt.
#[tokio::test(start_paused = true)]
async fn the_reconciler_works_every_interval_until_halted() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
    cold.fail_next(Op::Put, 1);
    let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    let reconciler = Reconciler::new(hot.clone(), cold.clone(), settings);
    let halt = Arc::new(Notify::new());
    let working = reconcile(reconciler, Duration::from_secs(5), Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(cold_mem.get("k", None).await.is_err());
        tokio::time::sleep(Duration::from_secs(5)).await;
        let copied = cold_mem.get("k", None).await.unwrap();
        assert_eq!(copied, Bytes::from_static(b"v"));
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(hot.get("_tiering/lease", None).await.is_err());
}

/// Asserts that a halt cuts a pass that is still running and gives the lease up at once: a copy
/// to a cold tier that takes an hour is not waited for, the object is not copied, and the lease
/// is gone.
#[tokio::test(start_paused = true)]
async fn a_halt_cuts_the_pass_that_is_running() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
    cold.set_delay(Op::Put, Some(Duration::from_secs(3600)));
    let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    let reconciler = Reconciler::new(hot.clone(), cold.clone(), settings);
    let halt = Arc::new(Notify::new());
    let working = reconcile(reconciler, Duration::from_secs(5), Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        halt.notify_one();
    };
    let both = async { tokio::join!(working, watching) };
    let done = tokio::time::timeout(Duration::from_secs(60), both).await;
    assert!(done.is_ok());
    assert!(hot.get("_tiering/lease", None).await.is_err());
    assert!(cold_mem.get("k", None).await.is_err());
}

/// Asserts that the id of a start's cache-directory probe has the proxy id, the time and the process, with
/// every character a key cannot carry in the proxy id replaced by an underscore.
#[test]
fn the_probe_run_id_is_made_of_the_proxy_the_time_and_the_process() {
    assert_eq!(probe_run_id("p1", 1234, 77), "p1-1234-77");
    assert_eq!(probe_run_id("a/b c.d-e", 5, 6), "a_b_c_d-e-5-6");
    assert_ne!(probe_run_id("p1", 1, 7), probe_run_id("p1", 2, 7));
}

/// Asserts the exit statuses of a failed start: flags that cannot go together are status 2, a
/// cache directory whose file system answered the check of the cache wrongly is status 3, and any
/// other failure is status 1 (a tier that cannot be reached is one: a restart may mend it).
#[test]
fn the_exit_status_of_a_failed_start_is_told_by_what_failed() {
    let invalid: super::super::Error = Box::new(InvalidFlags("x".to_owned()));
    let wrong: super::super::Error = Box::new(LocalTierFailure::Failed {
        path: "/cache".to_owned(),
        check: "create-only",
    });
    let other: super::super::Error = "another reason".into();
    assert_eq!(exit_status(&invalid), 2);
    assert_eq!(exit_status(&wrong), 3);
    assert_eq!(exit_status(&other), 1);
}

/// Asserts that the report of a drain names the requests that the deadline cut, and that a drain
/// that cut none reports nothing.
#[test]
fn a_drain_that_cut_requests_reports_them_and_one_that_cut_none_says_nothing() {
    assert_eq!(cut_note(Drained { abandoned: 0 }), "");
    assert_eq!(
        cut_note(Drained { abandoned: 3 }),
        "tierstore: 3 requests were cut at the drain deadline\n"
    );
}
