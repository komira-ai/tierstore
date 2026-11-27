//! The announcement of a start (`pins-live`, `places-live`): sent by a thread of its own, asked
//! again until the node's cache answers (it waits for the write role), before every request that
//! changes what the node holds for the mount, and never by a process whose successor is taking
//! over. The announcement is seen in the tree store: the hold of the mount on a placement that the
//! start does not have attached is ended by it, and by nothing else.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use super::super::super::table::TreeTable;
use super::super::placed::key_of;
use super::super::{Core, Errno};
use super::announce;
use crate::TreeDigest;
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::Created;
use crate::server::local_cache::trees::testing::{Bench, fs_at};
use crate::server::local_cache::{LocalCache, Mode, Tick};
use crate::server::treefs::fills::AskError;
use crate::server::treefs::in_process::InProcess;
use crate::server::treefs::ino;
use crate::server::treefs::live_testing::{Live, MOUNT, walk};

/// How long the core of a test that waits the write role out waits for it.
const SHORT: Duration = Duration::from_millis(100);

/// How long the core of a test that gives the cache the write role waits for it: longer than the
/// test.
const LONG: Duration = Duration::from_secs(30);

/// A cache that is read-only (another instance holds the lock of its directory) until it ticks.
fn read_only(bench: &Bench) -> LocalCache {
    let _holder = bench.rig.locked();
    bench.rig.locked()
}

/// The runs of the mount that hold the placement `created`.
fn held(live: &Live, created: &Created) -> usize {
    live.bench
        .store
        .placement(&created.placement)
        .expect("a placement")
        .attachments
}

/// The placement at `at`, held by the mount: what an earlier start of it left, which the
/// announcement of this start ends, as it has the tree attached by no run.
fn held_at(live: &Live, at: &str) -> Created {
    let created = live.place(vec![fs_at(at)]);
    live.bench
        .store
        .attach(&created.placement, MOUNT)
        .expect("a hold");
    assert_eq!(held(live, &created), 1);
    created
}

/// The placement at `jobs/1f3a/root`, held by the mount.
fn held_placement(live: &Live) -> Created {
    held_at(live, "jobs/1f3a/root")
}

/// A core of the live rig's mount, over the same table of tree numbers and runtime directory,
/// whose requests wait `SHORT` for the write role and whose pauses between two tries of the
/// announcement are taken by `pause`.
fn core_over(live: &Live, pause: Arc<dyn Fn(Duration) + Send + Sync>) -> Arc<Core> {
    let asks = Arc::new(InProcess::new(
        (live.cache.clone(), Some(live.bench.store.clone())),
        tokio::runtime::Handle::current(),
        SHORT,
        pause,
    ));
    Arc::new(Core::new(
        live.core.settings().clone(),
        asks,
        TreeTable::open(&live.runtime).unwrap(),
        &live.runtime,
    ))
}

/// Gives the live rig's cache the write role it has been waiting for.
fn take_the_write_role(live: &Live) {
    assert_eq!(live.cache.tick().unwrap(), Tick::Took);
    assert_eq!(live.cache.mode(), Mode::ReadWrite);
}

/// Asserts that the thread of a start sends the announcement, and that a lookup that asks the node
/// for a placement while the announcement waits for the cache's write role waits for it: the
/// announcement ends the hold the earlier start left on another placement, then the `place` of the
/// lookup makes its own, which stays, never the other way round. Catches a hold that a late
/// announcement would end.
#[tokio::test(flavor = "multi_thread")]
async fn a_place_waits_for_the_announcement_the_thread_sends() {
    let live = Live::in_process_with("tfs-live-order", read_only, LONG).await;
    let stale = held_at(&live, "jobs/9999/root");
    let created = live.place(vec![fs_at("jobs/1f3a/root")]);
    let watching = live.cache.mode_watchers();
    live.core.tell_live_later().unwrap();
    until("the announcement waits for the write role", || {
        live.cache.mode_watchers() > watching
    })
    .await;
    let core = Arc::clone(&live.core);
    let lookup = tokio::task::spawn_blocking(move || walk(&core, &["jobs", "1f3a", "root"]));
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(!lookup.is_finished());
    assert_eq!(held(&live, &stale), 1);
    assert_eq!(held(&live, &created), 0);
    take_the_write_role(&live);
    let _ = lookup.await.unwrap();
    live.on_core(|core| core.settled().unwrap()).await;
    assert_eq!(held(&live, &stale), 0);
    assert_eq!(held(&live, &created), 1);
}

/// Asserts that the thread asks again while the cache is read-only, resting between tries, and
/// sends the announcement once the cache takes the write role: the hold of the mount on a
/// placement it has not attached is ended after the third rest, and an announcement told is not
/// sent again (a hold made after it stays).
#[tokio::test(flavor = "multi_thread")]
async fn the_thread_asks_again_until_the_cache_takes_the_write_role() {
    let live = Live::in_process_with("tfs-live-later", read_only, SHORT).await;
    let created = held_placement(&live);
    let rests = Arc::new(AtomicUsize::new(0));
    let pause = {
        let (rests, cache) = (Arc::clone(&rests), live.cache.clone());
        Arc::new(move |_: Duration| {
            if rests.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
                let _ = cache.tick();
            }
        })
    };
    let core = core_over(&live, pause);
    core.tell_live_later().unwrap();
    until("the announcement ends the hold", || held(&live, &created) == 0).await;
    let settled = Arc::clone(&core);
    tokio::task::spawn_blocking(move || settled.settled().unwrap())
        .await
        .unwrap();
    assert_eq!(rests.load(Ordering::SeqCst), 3);
    assert_eq!(live.cache.mode(), Mode::ReadWrite);
    live.bench
        .store
        .attach(&created.placement, MOUNT)
        .expect("a hold");
    let settled = Arc::clone(&core);
    tokio::task::spawn_blocking(move || settled.settled().unwrap())
        .await
        .unwrap();
    assert_eq!(held(&live, &created), 1, "sent once");
}

/// Asserts that a request that changes a hold, while the cache stays read-only, fails as it does
/// with no announcement owed (the lookup of a placement is `EIO`, the announcement `ReadOnly`),
/// that the end of a tree of manifests and of a placement ends nothing, that the announcement stays
/// owed (the hold of the earlier start is still there), and that it is sent once the cache takes
/// the write role.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_that_finds_the_cache_read_only_fails_and_the_announcement_stays_owed() {
    let live = Live::in_process_with("tfs-live-away", read_only, SHORT).await;
    let created = held_placement(&live);
    let (rest, resting) = mpsc::channel::<()>();
    let resting = Mutex::new(resting);
    let entered = Arc::new(AtomicUsize::new(0));
    let pause = {
        let entered = Arc::clone(&entered);
        Arc::new(move |_: Duration| {
            let _ = entered.fetch_add(1, Ordering::SeqCst);
            // The thread rests until the test ends.
            let _ = resting.lock().unwrap().recv();
        })
    };
    let core = core_over(&live, pause);
    core.tell_live_later().unwrap();
    until("the thread rested after its first try", || {
        entered.load(Ordering::SeqCst) == 1
    })
    .await;
    let asked = Arc::clone(&core);
    let (looked, settled) = tokio::task::spawn_blocking(move || {
        (asked.lookup(ino::ROOT, "jobs"), asked.settled())
    })
    .await
    .unwrap();
    assert_eq!(looked, Err(Errno::Io));
    assert_eq!(settled, Err(AskError::ReadOnly));
    // The end of a tree of manifests (`unpin`) and of a placement (`unplace`) is not sent either.
    let letting = Arc::clone(&core);
    let placement = key_of(created.placement.as_str(), "jobs/1f3a/root").unwrap();
    tokio::task::spawn_blocking(move || {
        letting.let_go(&TreeDigest::from_bytes([7; 32]));
        letting.let_go(&placement);
    })
    .await
    .unwrap();
    assert_eq!(held(&live, &created), 1);
    take_the_write_role(&live);
    let settled = Arc::clone(&core);
    tokio::task::spawn_blocking(move || settled.settled().unwrap())
        .await
        .unwrap();
    assert_eq!(held(&live, &created), 0);
    drop(rest);
}

/// Asserts that a core whose successor is taking over sends no announcement: `settled` is
/// `Unavailable` while the hold is on, and the hold of the earlier start is not ended. A hand-over
/// that did not complete lets the hold go, and the thread, which rested meanwhile, sends the
/// announcement, once.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_core_sends_no_announcement_until_the_hold_is_let_go() {
    let live = Live::new("tfs-live-held").await;
    let created = held_placement(&live);
    live.core.hold(true);
    live.core.tell_live_later().unwrap();
    assert_eq!(live.core.settled(), Err(AskError::Unavailable));
    assert_eq!(held(&live, &created), 1);
    live.core.hold(false);
    until("the announcement ends the hold", || held(&live, &created) == 0).await;
    live.on_core(|core| core.settled().unwrap()).await;
}

/// Asserts that the sending of the announcement ends when the core is gone, with nothing sent: the
/// hold of the earlier start is not ended.
#[tokio::test(flavor = "multi_thread")]
async fn the_announcement_of_a_dropped_core_is_not_sent() {
    let live = Live::new("tfs-live-dropped").await;
    let created = held_placement(&live);
    let core = live.successor();
    let gone = Arc::downgrade(&core);
    drop(core);
    announce(&gone);
    assert_eq!(held(&live, &created), 1);
}

/// Asserts that requests that find the announcement being sent (it waits for the cache's write
/// role) wait for that attempt and take its outcome: all of them are answered `ok` once it is
/// told, by the one announcement, which ends the hold of the earlier start; none is answered before.
#[tokio::test(flavor = "multi_thread")]
async fn requests_that_find_the_announcement_being_sent_wait_for_it() {
    let live = Live::in_process_with("tfs-live-shared", read_only, LONG).await;
    let created = held_placement(&live);
    let watching = live.cache.mode_watchers();
    live.core.tell_live_later().unwrap();
    until("the announcement waits for the write role", || {
        live.cache.mode_watchers() > watching
    })
    .await;
    let waiting: Vec<_> = (0..3)
        .map(|_| {
            let core = Arc::clone(&live.core);
            tokio::task::spawn_blocking(move || core.settled())
        })
        .collect();
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(waiting.iter().all(|waiter| !waiter.is_finished()));
    assert_eq!(held(&live, &created), 1);
    take_the_write_role(&live);
    for waiter in waiting {
        assert_eq!(waiter.await.unwrap(), Ok(()));
    }
    assert_eq!(held(&live, &created), 0);
    // A hold made now stays: the announcement was sent once, however many waited for it.
    live.bench
        .store
        .attach(&created.placement, MOUNT)
        .expect("a hold");
    live.on_core(|core| core.settled().unwrap()).await;
    assert_eq!(held(&live, &created), 1);
}

/// Asserts that a hold waits for an announcement that is being sent: once the hold is on, the
/// announcement has been answered (the hold of the earlier start is ended), so none of this
/// process follows the successor's.
#[tokio::test(flavor = "multi_thread")]
async fn a_hold_waits_for_the_announcement_being_sent() {
    let live = Live::in_process_with("tfs-live-hold-waits", read_only, LONG).await;
    let created = held_placement(&live);
    let watching = live.cache.mode_watchers();
    live.core.tell_live_later().unwrap();
    until("the announcement waits for the write role", || {
        live.cache.mode_watchers() > watching
    })
    .await;
    let core = Arc::clone(&live.core);
    let hold = tokio::task::spawn_blocking(move || core.hold(true));
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(!hold.is_finished());
    take_the_write_role(&live);
    hold.await.unwrap();
    assert_eq!(held(&live, &created), 0);
    assert_eq!(live.core.settled(), Ok(()));
}
