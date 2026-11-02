//! The reconciler's lease when more than one taker is at the door, when the holder's pass is
//! longer than the lease, and when the clocks disagree.
//!
//! The lease is the object `_tiering/lease` of the hot tier. It is taken create-only, renewed and
//! taken over with `If-Match`, and it expires when the object was last written `ttl` ago by the
//! contender's own clock. Nothing here sleeps: every clock is set by hand.
//!
//! Gap: these tests run over the in-memory backend only, whose replace-on-tag is a mutex-held
//! check and write. `put_if_match` over the directory backend (atomic only within one process,
//! through its `replacing` lock) and over a real S3 store (an `If-Match` PUT) is not exercised.

use std::sync::Arc;

use futures::future::join_all;
use tierstore::{
    Backend, Error, MemoryBackend, Op, PassReport, Reconciler, ReconcilerConfig, TestClock,
};
use tokio::sync::Notify;

use crate::hold::{Gate, Interpose, Stall};
use crate::support::{Rig, assert_absent, body, read, text};

const TAKERS: usize = 8;

/// Eight reconcilers with names of their own over `hot`, each running one pass, all polled
/// together: a pass that waits at the gate lets the others run. Gives the number of each with what
/// its pass reported.
async fn passes(rig: &Rig, hot: Arc<dyn Backend>) -> Vec<(usize, PassReport)> {
    let mut reconcilers: Vec<Reconciler> = (0..TAKERS)
        .map(|number| {
            let config = rig.reconciler_config(&format!("taker-{number}"), 1 << 20);
            Reconciler::new(hot.clone(), rig.cold.clone(), config)
        })
        .collect();
    let reports = join_all(reconcilers.iter_mut().map(|reconciler| reconciler.pass())).await;
    reports.into_iter().enumerate().collect()
}

/// Asserts that exactly one of the takers holds the lease, that the lease object names it, that
/// the others did nothing, and that the one that holds it can go on to renew it.
async fn assert_one_holder(rig: &Rig, done: &[(usize, PassReport)]) {
    let holders: Vec<usize> = done
        .iter()
        .filter(|(_, report)| report.held)
        .map(|(number, _)| *number)
        .collect();
    assert_eq!(holders.len(), 1, "{done:?}");
    for (number, report) in done {
        if *number != holders[0] {
            assert_eq!(*report, PassReport::default(), "taker {number}");
        }
    }
    let lease = text(&*rig.hot_mem, "_tiering/lease").await;
    assert!(
        lease.starts_with(&format!("taker-{}\n", holders[0])),
        "{lease}"
    );
}

/// Asserts the claim "of two takers one fails" for eight: a lease that has expired is taken over
/// by eight reconcilers that are all inside the replacing put (`If-Match` on the tag they all
/// read) before any of them goes on. One passes and holds; the lease object names it; the other
/// seven report nothing done. Defects it catches: a take over that is not conditional on the
/// tag it read, which gives more than one holder; a take over that gives the lease to nobody.
#[tokio::test]
async fn eight_reconcilers_that_find_the_lease_expired_at_once_have_one_holder() {
    let rig = Rig::new();
    assert!(rig.reconciler("old", 1 << 20).pass().await.held);
    rig.clock.set(30_000);
    let hot: Arc<dyn Backend> =
        Arc::new(Gate::on_replaces(rig.hot.clone(), "_tiering/lease", TAKERS));
    let done = passes(&rig, hot).await;
    assert_one_holder(&rig, &done).await;
}

/// Asserts the same for a lease that does not exist: eight reconcilers are all inside the
/// create-only put of the lease before any goes on. One creates it and holds it; the other seven
/// find it taken and not yet expired, and do nothing. Defect it catches: a first take of the
/// lease that is not create-only.
#[tokio::test]
async fn eight_reconcilers_that_find_no_lease_at_once_have_one_holder() {
    let rig = Rig::new();
    let hot: Arc<dyn Backend> = Arc::new(Gate::new(rig.hot.clone(), "_tiering/lease", TAKERS));
    let done = passes(&rig, hot).await;
    assert_one_holder(&rig, &done).await;
}

/// Asserts the documented overlap, and that the old holder stores nothing after it. A pass that
/// is still copying when the lease is taken over (the copy of one object waits) is not stopped:
/// the old holder's pass goes on, copies beside the new holder, whose pass is whole, and reports
/// that it held the lease. The cold tier then holds every object whole; the lease is the new
/// holder's; the old holder's next pass finds it lost the lease and does nothing. This PINS WHAT
/// THE CODE DOES NOT DO: there is no fencing token, so the copies of the stale pass are not
/// fenced (they are create-only and find the new holder's, and the pass reports no failure). What
/// it does do is check the lease, without renewing it, before it stores: it finds the lease
/// gone, so it forgets what it read and stores no checkpoint and prunes nothing (the test of the
/// checkpoint below says what is stored). Defects it catches: a take over while the old holder's
/// lease is young; an old holder that renews a lease another holder replaced; an overlap that
/// loses an object; a stale pass that keeps its books. (Renamed from
/// `a_pass_longer_than_the_lease_overlaps_the_new_holder_and_stops_at_its_next_renewal`: the
/// assertions of that test all stand, with one added.)
#[tokio::test(start_paused = true)]
async fn a_pass_longer_than_the_lease_copies_beside_the_new_holder_and_stores_nothing() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    let names = ["k1", "k2", "k3", "k4"];
    for name in names {
        proxy.put(name, body(name)).await.unwrap();
    }
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let slow: Arc<dyn Backend> = Arc::new(Stall::new(
        rig.cold.clone(),
        "k3",
        entered.clone(),
        Some(release.clone()),
    ));
    let mut one = Reconciler::new(rig.hot.clone(), slow, rig.reconciler_config("r1", 1 << 20));
    let mut two = rig.reconciler("r2", 1 << 20);
    let takeover = async {
        entered.notified().await;
        rig.clock.set(30_000);
        let report = two.pass().await;
        release.notify_one();
        report
    };

    let (first, second) = tokio::join!(one.pass(), takeover);

    assert!(first.held && second.held, "{first:?} {second:?}");
    assert_eq!(second.failed, Vec::<Error>::new());
    // PINS A KNOWN LIMIT: copies are not fenced.
    assert_eq!(first.failed, Vec::<Error>::new(), "{first:?}");
    assert_eq!(first.copied, names.len(), "{first:?}");
    assert_eq!(first.pruned, 0, "{first:?}");
    assert_eq!(one.hot_bytes(), 0);
    for name in names {
        assert_eq!(read(&*rig.cold_mem, name).await, body(name), "{name}");
        assert_eq!(read(&*rig.hot_mem, name).await, body(name), "{name}");
    }
    assert!(
        text(&*rig.hot_mem, "_tiering/lease")
            .await
            .starts_with("r2\n")
    );
    assert_eq!(one.pass().await, PassReport::default());
    assert!(two.pass().await.held);
}

/// Asserts that a checkpoint never moves backwards: reconciler `r1` has a pass that is longer
/// than the lease (a copy waits), `r2` takes the lease over, applies two more journal objects
/// than `r1` has read and stores its checkpoint, and then `r1`'s pass ends: it checks the lease,
/// finds it lost, and stores nothing. The checkpoint stored for the proxy after that is still
/// `r2`'s (journal objects up to number 4 applied), not `r1`'s (up to number 2), and a
/// reconciler started afterwards reads the proxy's new puts, as a checkpoint that went back past
/// pruned journal objects would keep it from doing.
/// Catches a stale pass that stores its older checkpoint over the new holder's, so that the
/// next holder reads and copies journal objects again and the prune of journal objects follows
/// a number that is older than the work done.
#[tokio::test(start_paused = true)]
async fn a_stale_pass_does_not_move_the_stored_checkpoint_back() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    // One journal object for each put: numbers 1 and 2.
    for name in ["k1", "k2"] {
        proxy.put(name, body(name)).await.unwrap();
    }
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let slow: Arc<dyn Backend> = Arc::new(Stall::new(
        rig.cold.clone(),
        "k2",
        entered.clone(),
        Some(release.clone()),
    ));
    let mut one = Reconciler::new(rig.hot.clone(), slow, rig.reconciler_config("r1", 1 << 20));
    let mut two = rig.reconciler("r2", 1 << 20);
    let takeover = async {
        entered.notified().await;
        // r1 has read journal objects 1 and 2 and waits in the copy of k2. Two more come.
        for name in ["k3", "k4"] {
            proxy.put(name, body(name)).await.unwrap();
        }
        rig.clock.set(30_000);
        let report = two.pass().await;
        assert!(report.held, "{report:?}");
        let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
        assert!(stored.contains("\"seq\":4"), "r2 stored {stored}");
        release.notify_one();
    };

    let (first, ()) = tokio::join!(one.pass(), takeover);

    assert!(first.held, "{first:?}");
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert!(
        stored.contains("\"seq\":4"),
        "the stale pass put the checkpoint back: {stored}"
    );

    two.stop().await;
    proxy.put("k5", body("k5")).await.unwrap();
    let mut three = rig.reconciler("r3", 1 << 20);
    let report = three.pass().await;
    assert!(report.held, "{report:?}");
    assert_eq!(report.copied, 1, "{report:?}");
    assert_eq!(read(&*rig.cold_mem, "k5").await, body("k5"));
}

/// Asserts that a lease object that is deleted during a pass (the holder before it stopped, say)
/// counts as a lost lease: the pass that finds it gone at the check reports that it held the
/// lease at its start and no failure, stores no checkpoint and no usage, and forgets what it
/// read; the next pass takes the lease anew, works through the journal again (the copies find
/// the cold tier's) and stores both.
/// Catches a pass that stores a checkpoint without a lease, and one that treats a missing lease
/// object as still held.
#[tokio::test(start_paused = true)]
async fn a_lease_deleted_during_a_pass_stops_the_pass_before_its_stores() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    for name in ["k1", "k2"] {
        proxy.put(name, body(name)).await.unwrap();
    }
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let slow: Arc<dyn Backend> = Arc::new(Stall::new(
        rig.cold.clone(),
        "k2",
        entered.clone(),
        Some(release.clone()),
    ));
    let mut one = Reconciler::new(rig.hot.clone(), slow, rig.reconciler_config("r1", 1 << 20));
    let gone = async {
        entered.notified().await;
        rig.hot_mem.delete("_tiering/lease").await.unwrap();
        release.notify_one();
    };

    let (first, ()) = tokio::join!(one.pass(), gone);

    assert!(first.held, "{first:?}");
    assert_eq!(first.failed, Vec::<Error>::new(), "{first:?}");
    assert_eq!(first.copied, 2, "{first:?}");
    assert_eq!(one.hot_bytes(), 0);
    assert_absent(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_absent(&*rig.hot_mem, "_tiering/usage").await;

    let second = one.pass().await;
    assert!(second.held, "{second:?}");
    assert_eq!(second.failed, Vec::<Error>::new(), "{second:?}");
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":2,"evict_seq":0,"evict_idx":0}"#);
    assert!(!text(&*rig.hot_mem, "_tiering/usage").await.is_empty());
}

/// Asserts that a checkpoint that another holder stored ahead of this pass, after its check of
/// the lease, is kept and adopted: the pass reports no failure, the stored checkpoint is the
/// other's byte for byte, and the pass after it, which has nothing to add to that, does not read
/// the checkpoint again (a HEAD of it that is made to fail is not made).
/// Catches a pass that writes its own, lower checkpoint over one that is ahead, and a pass that
/// reads the checkpoint on every pass for as long as the stored one is ahead of its books.
#[tokio::test(start_paused = true)]
async fn a_checkpoint_ahead_of_the_pass_is_kept_and_adopted() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    proxy.put("k1", body("k1")).await.unwrap();
    let ahead = r#"{"seq":9,"evict_seq":0,"evict_idx":0}"#;
    let hot: Arc<dyn Backend> = Arc::new(Interpose::new(
        rig.hot.clone(),
        rig.hot_mem.clone(),
        "_tiering/checkpoint/p1",
        body(ahead),
    ));
    let mut reconciler =
        Reconciler::new(hot, rig.cold.clone(), rig.reconciler_config("r1", 1 << 20));

    let first = reconciler.pass().await;
    assert_eq!(first.failed, Vec::<Error>::new(), "{first:?}");
    assert_eq!(text(&*rig.hot_mem, "_tiering/checkpoint/p1").await, ahead);

    rig.hot.fail_next_on(
        Op::Head,
        "_tiering/checkpoint/",
        Error::Unavailable("read".to_owned()),
    );
    let second = reconciler.pass().await;
    assert_eq!(second.failed, Vec::<Error>::new(), "{second:?}");
    assert_eq!(text(&*rig.hot_mem, "_tiering/checkpoint/p1").await, ahead);
}

/// A holder on the clock of the hot tier and a taker on a clock of its own, over a hot tier
/// whose objects are stamped by the first.
struct Skewed {
    store: Arc<TestClock>,
    taker_clock: Arc<TestClock>,
    holder: Reconciler,
    taker: Reconciler,
}

impl Skewed {
    /// The holder has taken the lease at time zero of the hot tier.
    async fn new() -> Self {
        let store = Arc::new(TestClock::new());
        let taker_clock = Arc::new(TestClock::new());
        let hot = Arc::new(MemoryBackend::with_clock(store.clone()));
        let cold = Arc::new(MemoryBackend::with_clock(store.clone()));
        let config = |name: &str, clock: &Arc<TestClock>| {
            ReconcilerConfig::new(name, 1 << 20, clock.clone())
        };
        let mut holder = Reconciler::new(hot.clone(), cold.clone(), config("holder", &store));
        let taker = Reconciler::new(hot, cold, config("taker", &taker_clock));
        assert!(holder.pass().await.held);
        Self {
            store,
            taker_clock,
            holder,
            taker,
        }
    }

    /// The taker's pass when the hot tier's time is `store` and the taker's own clock reads
    /// `taker`.
    async fn taker_passes(&mut self, store: u64, taker: u64) -> bool {
        self.store.set(store);
        self.taker_clock.set(taker);
        self.taker.pass().await.held
    }
}

/// Asserts what the design says of skew when the taker's clock is ahead of the hot tier's by 10
/// seconds and the ttl is 30: the lease changes hands when its object is 20 seconds old, not
/// 30, because the age is the taker's clock minus the hot tier's stamp. Not a second earlier.
/// The holder, still alive, is stopped at its next renewal. Defect it catches: an age computed
/// from anything but the contender's own clock and the stamp of the hot tier.
#[tokio::test]
async fn a_taker_10_seconds_ahead_takes_the_lease_when_it_is_20_seconds_old() {
    let mut skew = Skewed::new().await;
    assert!(!skew.taker_passes(19_999, 29_999).await);
    assert!(skew.taker_passes(20_000, 30_000).await);
    assert_eq!(skew.holder.pass().await, PassReport::default());
}

/// Asserts that a taker 40 seconds ahead, which is more than the ttl, takes the lease at once
/// from a holder that wrote it in the same instant: the lease changes hands early when the skew
/// is more than the ttl, and the old holder, which has done nothing wrong, is stopped at its next
/// renewal. This is why the lease needs bounded skew. Defect it catches: a
/// lease that is never early.
#[tokio::test]
async fn a_taker_40_seconds_ahead_takes_a_lease_that_was_just_written() {
    let mut skew = Skewed::new().await;
    assert!(skew.taker_passes(0, 40_000).await);
    assert_eq!(skew.holder.pass().await, PassReport::default());
}

/// Asserts that a taker whose clock is 10 seconds behind takes the lease late, at 40 seconds of
/// the hot tier's time and not before: skew in this direction costs availability, not safety.
/// Defect it catches: an age computed from the hot tier's own clock.
#[tokio::test]
async fn a_taker_10_seconds_behind_takes_the_lease_late() {
    let mut skew = Skewed::new().await;
    assert!(!skew.taker_passes(39_999, 29_999).await);
    assert!(skew.taker_passes(40_000, 30_000).await);
}

/// Asserts that a renewal moves the age of the lease on: a taker waits the whole ttl from the
/// holder's last renewal, not from the time the lease was first taken. Defect it catches: a
/// renewal that does not move the time of the lease object on, which lets a live holder be
/// replaced a ttl after it first took the lease.
#[tokio::test]
async fn a_renewal_moves_the_age_of_the_lease_on() {
    let mut skew = Skewed::new().await;
    skew.store.set(25_000);
    assert!(skew.holder.pass().await.held);
    assert!(!skew.taker_passes(54_999, 54_999).await);
    assert!(skew.taker_passes(55_000, 55_000).await);
}
