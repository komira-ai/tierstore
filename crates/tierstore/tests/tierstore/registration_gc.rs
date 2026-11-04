//! The leader removes the registration of a proxy that heartbeated and left, once it has stayed
//! unchanged for the registration retention by the leader's own monotonic clock, and only then.
//! The clock is a [`Mono`] the test advances; the wall clock of the stores is not what counts.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tierstore::{Backend, Error, Mono, Op, Reconciler};

use crate::support::{Rig, journal_key};

const GONE: &str = "_tiering/proxies/gone";
const RETENTION: Duration = Duration::from_secs(600);

fn beating(count: u64) -> Bytes {
    Bytes::from(format!(r#"{{"proto":1,"boot":"b","hb":{count}}}"#))
}

fn leader(rig: &Rig, holder: &str, mono: &Arc<Mono>) -> Reconciler {
    let mut config = rig.reconciler_config(holder, 1 << 20);
    config.mono = mono.clone();
    config.registration_retention = RETENTION;
    rig.reconciler_with(config)
}

async fn there(rig: &Rig, key: &str) -> bool {
    rig.hot_mem.head(key).await.is_ok()
}

async fn put(rig: &Rig, key: &str, body: Bytes) {
    rig.hot_mem.put(key, body).await.unwrap();
}

/// Asserts that, of four registrations unchanged for the whole retention, only the one of a
/// process that heartbeated and left no journal object is removed: the empty one of an older
/// release, the one of a process whose count is zero (it never started its heartbeat) and the one
/// of a proxy that has a journal object stay. Nothing is removed a millisecond before the
/// retention is up.
#[tokio::test]
async fn after_the_retention_only_the_registration_of_a_departed_heartbeating_proxy_is_removed() {
    let rig = Rig::new();
    let mono = Arc::new(Mono::new());
    put(&rig, GONE, beating(3)).await;
    put(&rig, "_tiering/proxies/old", Bytes::new()).await;
    put(&rig, "_tiering/proxies/never", beating(0)).await;
    put(&rig, "_tiering/proxies/journaled", beating(5)).await;
    put(
        &rig,
        &journal_key("journaled", 1),
        Bytes::from_static(b"[]"),
    )
    .await;
    let mut leader = leader(&rig, "r1", &mono);

    assert_eq!(leader.pass().await.failed, Vec::<Error>::new());
    mono.advance(RETENTION - Duration::from_millis(1));
    assert_eq!(leader.pass().await.failed, Vec::<Error>::new());
    assert!(there(&rig, GONE).await);

    mono.advance(Duration::from_millis(1));
    assert_eq!(leader.pass().await.failed, Vec::<Error>::new());
    assert!(!there(&rig, GONE).await);
    for kept in ["old", "never", "journaled"] {
        assert!(
            there(&rig, &format!("_tiering/proxies/{kept}")).await,
            "{kept}"
        );
    }
}

/// Asserts that a registration that is rewritten (its owner is alive) is counted from its last
/// change: unchanged for 500 seconds, rewritten, then 500 and 99 more seconds, it is still there,
/// and one second later (600 since the change) it is removed.
#[tokio::test]
async fn a_registration_that_changes_is_counted_from_its_last_change() {
    let rig = Rig::new();
    let mono = Arc::new(Mono::new());
    put(&rig, GONE, beating(1)).await;
    let mut leader = leader(&rig, "r1", &mono);
    leader.pass().await;
    mono.advance(Duration::from_secs(500));
    leader.pass().await;

    rig.clock.advance(1_000);
    rig.hot_mem.delete(GONE).await.unwrap();
    put(&rig, GONE, beating(2)).await;
    leader.pass().await;
    mono.advance(Duration::from_secs(599));
    leader.pass().await;
    assert!(there(&rig, GONE).await);

    mono.advance(Duration::from_secs(1));
    leader.pass().await;
    assert!(!there(&rig, GONE).await);
}

/// Asserts that a leader that takes over counts from its own first sight: the first leader saw the
/// registration for 500 seconds and stopped, the second does not remove it before 600 seconds of
/// its own, removes it at 600, and so a departure is noticed late at worst, never early.
#[tokio::test]
async fn a_new_leader_counts_from_its_own_first_sight() {
    let rig = Rig::new();
    let mono = Arc::new(Mono::new());
    put(&rig, GONE, beating(1)).await;
    let mut first = leader(&rig, "r1", &mono);
    first.pass().await;
    mono.advance(Duration::from_secs(500));
    first.pass().await;
    first.stop().await;

    let mut second = leader(&rig, "r2", &mono);
    second.pass().await;
    mono.advance(Duration::from_secs(599));
    second.pass().await;
    assert!(there(&rig, GONE).await);
    mono.advance(Duration::from_secs(1));
    second.pass().await;
    assert!(!there(&rig, GONE).await);
}

/// Asserts that a delete the store fails is reported in the pass, that the registration is not
/// asked about again at the next passes (the registration is read once in the retention, not at
/// every pass), and that it is asked about and removed once another retention has gone by.
#[tokio::test]
async fn a_failed_removal_is_reported_and_tried_again_only_after_the_retention() {
    let rig = Rig::new();
    let mono = Arc::new(Mono::new());
    put(&rig, GONE, beating(1)).await;
    let mut leader = leader(&rig, "r1", &mono);
    leader.pass().await;
    mono.advance(RETENTION);

    rig.hot.fail_next(Op::DeleteIfMatch, 1);
    let report = leader.pass().await;
    assert!(
        matches!(report.failed[..], [Error::Unavailable(_)]),
        "{:?}",
        report.failed
    );
    assert!(there(&rig, GONE).await);
    assert_eq!(rig.hot.calls_on(Op::Get, GONE), 1);

    mono.advance(RETENTION - Duration::from_millis(1));
    assert_eq!(leader.pass().await.failed, Vec::<Error>::new());
    assert_eq!(rig.hot.calls_on(Op::Get, GONE), 1);

    mono.advance(Duration::from_millis(1));
    assert_eq!(leader.pass().await.failed, Vec::<Error>::new());
    assert_eq!(rig.hot.calls_on(Op::Get, GONE), 2);
    assert!(!there(&rig, GONE).await);
}

/// Asserts the restart boundary: a process that opens over the registration of an earlier process
/// that beat, and never starts its own heartbeat, keeps a registration the leader does not remove
/// after the retention, so a write it makes later is found and copied by a fresh reconciler.
#[tokio::test(start_paused = true)]
async fn a_process_that_opens_over_a_beating_registration_and_never_beats_is_not_removed() {
    let rig = Rig::new();
    let mono = Arc::new(Mono::new());
    put(&rig, "_tiering/proxies/p1", beating(7)).await;
    let proxy = rig.proxy("p1", 1000).await;

    let mut leader = leader(&rig, "r1", &mono);
    assert_eq!(leader.pass().await.failed, Vec::<Error>::new());
    mono.advance(RETENTION);
    assert_eq!(leader.pass().await.failed, Vec::<Error>::new());
    assert!(there(&rig, "_tiering/proxies/p1").await);

    proxy.put("a", Bytes::from_static(b"1")).await.unwrap();
    leader.stop().await;
    let report = rig.reconciler("r2", 1000).pass().await;
    assert!(report.held);
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
}

/// Asserts that a registration retention of zero removes no registration, however long it has
/// been unchanged (the switch to set before going back to a release whose proxies never rewrite
/// a registration), and that the leader asks the store about none of them.
#[tokio::test]
async fn a_retention_of_zero_removes_no_registration() {
    let rig = Rig::new();
    let mono = Arc::new(Mono::new());
    put(&rig, GONE, beating(3)).await;
    let mut config = rig.reconciler_config("r1", 1 << 20);
    config.mono = mono.clone();
    config.registration_retention = Duration::ZERO;
    let mut leader = rig.reconciler_with(config);

    assert_eq!(leader.pass().await.failed, Vec::<Error>::new());
    mono.advance(Duration::from_secs(100_000));
    assert_eq!(leader.pass().await.failed, Vec::<Error>::new());
    assert!(there(&rig, GONE).await);
    assert_eq!(rig.hot.calls_on(Op::Get, GONE), 0);
}
