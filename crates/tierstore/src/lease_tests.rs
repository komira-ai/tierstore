//! The lease body: two holders of one name, a restarted process and the one it replaced, are not
//! mistaken for each other by the content tag; the check that a holder still holds the lease;
//! and the three times of the lease, all read from the monotonic clock of the process: the wait
//! of a taker, the time the holder is valid, and the timer that renews.

use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::faulty::{FaultyBackend, Op};
use crate::journal::LEASE_KEY;
use crate::lease::Lease;
use crate::{
    Backend, Error, GateBackend, MemoryBackend, Mono, Phase, Proxy, ProxyConfig, Reconciler,
    ReconcilerConfig, Sleeper, TestClock,
};

const TTL: u64 = 30_000;
const RENEW: u64 = 10_000;
const MARGIN: u64 = 10_000;

/// A hot tier on a clock the test sets, which is also the monotonic clock of the two leases of
/// the holder name `p` over it, as a process and its restart are.
fn two_holders() -> (Arc<MemoryBackend>, Arc<TestClock>, Lease, Lease) {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
    let a = Lease::new(hot.clone(), clock.clone(), "p", TTL, RENEW, MARGIN);
    let b = Lease::new(hot.clone(), clock.clone(), "p", TTL, RENEW, MARGIN);
    (hot, clock, a, b)
}

async fn tag(hot: &MemoryBackend) -> Option<String> {
    hot.head(LEASE_KEY).await.ok().map(|meta| meta.tag)
}

/// The count (third line) of the lease body.
async fn count(hot: &MemoryBackend) -> String {
    let body = hot.get(LEASE_KEY, None).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    text.rsplit('\n').next().unwrap().to_owned()
}

/// A holder `a` at its second body (taken at 0, renewed at 10.001 s), and a taker `b` that has
/// looked at that object at 10.001 s and took it over at 40.001 s, when the clock stands.
async fn taken_over() -> (Arc<MemoryBackend>, Arc<TestClock>, Lease, Lease) {
    let (hot, clock, a, b) = two_holders();
    assert!(a.ensure().await);
    clock.set(10_001);
    assert!(a.ensure().await);
    assert!(!b.ensure().await);
    clock.set(40_001);
    assert!(b.ensure().await);
    (hot, clock, a, b)
}

/// Asserts that a holder whose lease was taken over by a restart with the same name cannot renew
/// the new holder's lease. A writes bodies 1 (taking) and 2 (renewing), and holds the tag of the
/// second. B's take over also writes body 2, because its `acquire` counts a body for the
/// create-only put that fails and the renewal that replaces the object counts the next. Defect it
/// catches: A's body and B's are the same bytes, so the tag is the same, and A's renewal replaces
/// the new holder's object.
#[tokio::test]
async fn a_stale_holder_of_the_same_name_cannot_renew_the_new_holders_lease() {
    let (hot, _clock, a, b) = taken_over().await;
    let current = tag(&hot).await.unwrap();

    assert!(!a.ensure().await);
    assert_eq!(tag(&hot).await.unwrap(), current);
    assert!(b.ensure().await);
}

/// Asserts that the old holder's release leaves the new holder's lease in place. Defect it
/// catches: the old holder sees a tag equal to its own and deletes the new holder's object. A
/// is at body 2 when it is taken over, as B is when its take over writes, so the bodies and the
/// tags would be equal without the nonce (see the test above).
#[tokio::test]
async fn a_stale_holder_of_the_same_name_cannot_release_the_new_holders_lease() {
    let (hot, _clock, a, _b) = taken_over().await;
    let current = tag(&hot).await;

    a.release().await;
    assert_eq!(tag(&hot).await, current);
}

/// Asserts that two leases of one holder name write different bodies for the same count of
/// writes. Defect it catches: a body made of the name and the count alone, which every process
/// of that name reproduces.
#[tokio::test]
async fn two_leases_of_one_name_write_different_first_bodies() {
    let (hot, _clock, a, _b) = two_holders();
    assert!(a.ensure().await);
    let first = hot.get(LEASE_KEY, None).await.unwrap();

    let other_clock = Arc::new(TestClock::new());
    let other_hot = Arc::new(MemoryBackend::with_clock(Arc::clone(&other_clock)));
    let c = Lease::new(other_hot.clone(), other_clock, "p", TTL, RENEW, MARGIN);
    assert!(c.ensure().await);
    let second = other_hot.get(LEASE_KEY, None).await.unwrap();

    assert_ne!(first, second);
}

/// Asserts that a holder that has not taken the lease does not hold it.
#[tokio::test]
async fn still_held_is_false_before_the_lease_is_taken() {
    let (hot, _clock, a, _b) = two_holders();
    assert!(!a.still_held().await.unwrap());
    assert_eq!(tag(&hot).await, None);
}

/// Asserts that a holder whose object is as it wrote it still holds the lease, and that asking
/// does not renew it (the tag of the object is the same after).
#[tokio::test]
async fn still_held_is_true_for_the_holder_and_does_not_renew() {
    let (hot, _clock, a, _b) = two_holders();
    assert!(a.ensure().await);
    let before = tag(&hot).await;
    assert!(a.still_held().await.unwrap());
    assert!(a.still_held().await.unwrap());
    assert_eq!(tag(&hot).await, before);
}

/// Asserts that a holder that was taken over finds it has lost the lease, forgets it (its next
/// `ensure` cannot renew the new holder's object, and only writes down the tag it sees) and
/// leaves the new holder's object alone.
#[tokio::test]
async fn still_held_is_false_and_forgets_after_a_take_over() {
    let (hot, _clock, a, b) = taken_over().await;
    let current = tag(&hot).await;

    assert!(!a.still_held().await.unwrap());
    assert!(!a.ensure().await);
    assert_eq!(tag(&hot).await, current);
    assert!(b.still_held().await.unwrap());
}

/// Asserts that a holder whose lease object is gone does not hold the lease, and takes it anew.
#[tokio::test]
async fn still_held_is_false_when_the_object_is_gone() {
    let (hot, _clock, a, _b) = two_holders();
    assert!(a.ensure().await);
    hot.delete(LEASE_KEY).await.unwrap();
    assert!(!a.still_held().await.unwrap());
    assert!(a.ensure().await);
    assert!(a.still_held().await.unwrap());
}

/// Asserts that a HEAD that fails is given back as an error, and that the holder keeps its tag:
/// the next answer is that it still holds the lease.
#[tokio::test]
async fn still_held_gives_a_head_error_and_keeps_the_tag() {
    let clock = Arc::new(TestClock::new());
    let mem = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
    let hot = Arc::new(FaultyBackend::new(mem));
    let a = Lease::new(hot.clone(), clock, "p", TTL, RENEW, MARGIN);
    assert!(a.ensure().await);
    let down = Error::Unavailable("on purpose".to_owned());
    hot.fail_next_on(Op::Head, LEASE_KEY, down.clone());
    assert_eq!(a.still_held().await.unwrap_err(), down);
    assert!(a.still_held().await.unwrap());
}

/// A hot tier whose objects are stamped by a clock of its own that the test never moves, a
/// counter over it, and a holder and a taker whose monotonic clocks are `holder_mono` and
/// `taker_mono`.
struct Pair {
    hot: Arc<MemoryBackend>,
    counted: Arc<FaultyBackend>,
    holder_mono: Arc<Mono>,
    taker_mono: Arc<Mono>,
    holder: Lease,
    taker: Lease,
}

fn pair() -> Pair {
    let stamps = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(stamps));
    let counted = Arc::new(FaultyBackend::new(hot.clone()));
    let (holder_mono, taker_mono) = (Arc::new(Mono::new()), Arc::new(Mono::new()));
    let make = |mono: &Arc<Mono>, name: &str| {
        Lease::new(counted.clone(), mono.clone(), name, TTL, RENEW, MARGIN)
    };
    let (holder, taker) = (make(&holder_mono, "holder"), make(&taker_mono, "taker"));
    Pair {
        hot,
        counted,
        holder_mono,
        taker_mono,
        holder,
        taker,
    }
}

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

/// Asserts that a taker counts the ttl on its own monotonic clock from its first look at the
/// tag: the first look only writes the tag down and no replacing put is made, a look 29.999
/// seconds later (the stamp of the hot tier, which the test never moves, is 0 throughout) takes
/// nothing, and one at 30 seconds replaces the object. Catches a take over decided by the stamp
/// of the hot tier or by the wall clock of the taker, which no test here moves.
#[tokio::test]
async fn a_taker_takes_the_lease_when_the_tag_has_stood_for_the_ttl_on_its_own_clock() {
    let pair = pair();
    assert!(pair.holder.ensure().await);

    assert!(!pair.taker.ensure().await);
    pair.taker_mono.advance(ms(TTL - 1));
    assert!(!pair.taker.ensure().await);
    assert_eq!(pair.counted.calls(Op::PutIfMatch), 0);
    pair.taker_mono.advance(ms(1));
    assert!(pair.taker.ensure().await);
    assert_eq!(pair.counted.calls(Op::PutIfMatch), 1);
    assert!(
        String::from_utf8(pair.hot.get(LEASE_KEY, None).await.unwrap().to_vec())
            .unwrap()
            .starts_with("taker\n")
    );
}

/// Asserts that a renewal restarts the wait: the taker looked at 0, the holder renews, the taker
/// looks again at 30 seconds, sees another tag, and takes the lease 30 seconds after that look
/// and not before. Catches a taker that counts from the first tag it ever saw.
#[tokio::test]
async fn a_renewed_lease_restarts_the_wait_of_a_taker() {
    let pair = pair();
    assert!(pair.holder.ensure().await);
    assert!(!pair.taker.ensure().await);

    pair.holder_mono.advance(ms(RENEW + 1));
    assert!(pair.holder.ensure().await);
    pair.taker_mono.advance(ms(TTL));
    assert!(!pair.taker.ensure().await);
    pair.taker_mono.advance(ms(TTL - 1));
    assert!(!pair.taker.ensure().await);
    pair.taker_mono.advance(ms(1));
    assert!(pair.taker.ensure().await);
}

/// Asserts that a taker does not take a lease whose object is gone from under it by a stale look
/// at an older tag: it looked at the first tag, the holder gave the lease up and another took it
/// (a new tag), and the wait starts again from the look at the new tag.
#[tokio::test]
async fn a_new_tag_after_a_gone_lease_restarts_the_wait() {
    let pair = pair();
    assert!(pair.holder.ensure().await);
    assert!(!pair.taker.ensure().await);
    pair.holder.release().await;
    let other = Lease::new(
        pair.counted.clone(),
        pair.holder_mono.clone(),
        "other",
        TTL,
        RENEW,
        MARGIN,
    );
    assert!(other.ensure().await);

    pair.taker_mono.advance(ms(TTL));
    assert!(!pair.taker.ensure().await);
    pair.taker_mono.advance(ms(TTL));
    assert!(pair.taker.ensure().await);
}

/// Asserts the boundary of the margin: a holder that wrote at 0 is valid until `ttl - margin`
/// (20 s), and `margin_ok` is true while at least the margin (10 s) of that is left, that is up
/// to 10 s, and false from 10.001 s.
#[tokio::test]
async fn margin_ok_holds_while_at_least_the_margin_is_left() {
    let pair = pair();
    assert!(!pair.holder.margin_ok());
    assert!(pair.holder.ensure().await);

    assert!(pair.holder.margin_ok());
    pair.holder_mono.advance(ms(RENEW));
    assert!(pair.holder.margin_ok());
    pair.holder_mono.advance(ms(1));
    assert!(!pair.holder.margin_ok());
}

/// Asserts that `ensure` writes nothing while the margin is left (the timer does the renewing)
/// and renews, with an `If-Match` put, once less than the margin is left.
#[tokio::test]
async fn ensure_renews_only_when_less_than_the_margin_is_left() {
    let pair = pair();
    assert!(pair.holder.ensure().await);
    pair.holder_mono.advance(ms(RENEW));
    assert!(pair.holder.ensure().await);
    assert_eq!(pair.counted.calls(Op::PutIfMatch), 0);
    assert_eq!(count(&pair.hot).await, "1");

    pair.holder_mono.advance(ms(1));
    assert!(pair.holder.ensure().await);
    assert_eq!(pair.counted.calls(Op::PutIfMatch), 1);
    assert_eq!(count(&pair.hot).await, "2");
    assert!(pair.holder.margin_ok());
}

/// Asserts that the time a holder is valid counts from when it sent the write, not from when the
/// answer came: the renewal is sent at 10.001 s and answers at 15.001 s, and the holder is valid
/// until 30.001 s less the margin of 10 s, so `margin_ok` is true at 20.001 s and false 1 ms
/// later. Catches a validity counted from the answer, which would run 5 s past the lease.
#[tokio::test]
async fn validity_counts_from_the_start_of_the_write() {
    let mono = Arc::new(Mono::new());
    let memory = Arc::new(MemoryBackend::new());
    let gated = Arc::new(GateBackend::new(memory));
    let lease = Lease::new(gated.clone(), mono.clone(), "p", TTL, RENEW, MARGIN);
    assert!(lease.ensure().await);
    mono.advance(ms(RENEW + 1));
    let gate = gated.gate(Op::PutIfMatch, LEASE_KEY, Phase::Pre);

    let mut renewing = pin!(lease.ensure());
    tokio::select! {
        _ = &mut renewing => panic!("the renewal finished before it was parked"),
        () = gate.parked() => {}
    }
    mono.advance(ms(5_000));
    assert!(gate.release());
    assert!(renewing.await);

    mono.advance(ms(5_000));
    assert!(lease.margin_ok());
    mono.advance(ms(1));
    assert!(!lease.margin_ok());
}

/// A sleeper that writes down each wait it is asked for and then waits for the test to let it
/// go: the test sees every wait as it is asked, and decides when it ends.
struct StepSleeper {
    asked: UnboundedSender<Duration>,
    go: Mutex<UnboundedReceiver<()>>,
}

impl StepSleeper {
    /// The sleeper, the waits it is asked for, and the way to let a wait end.
    fn new() -> (Self, UnboundedReceiver<Duration>, UnboundedSender<()>) {
        let (asked, waits) = unbounded_channel();
        let (release, go) = unbounded_channel();
        let sleeper = Self {
            asked,
            go: Mutex::new(go),
        };
        (sleeper, waits, release)
    }
}

#[async_trait]
impl Sleeper for StepSleeper {
    async fn sleep(&self, duration: Duration) {
        let _ = self.asked.send(duration);
        let _ = self.go.lock().await.recv().await;
    }
}

/// The work a renewing test runs the timer beside: it ends when the test sends on the channel.
type Work = tokio::sync::oneshot::Receiver<()>;

fn work() -> (tokio::sync::oneshot::Sender<()>, Work) {
    tokio::sync::oneshot::channel()
}

/// Asserts that the timer waits the renewal period, renews when the wait ends and then asks for
/// the next wait, every time, and that the work's answer is the answer of the call: the waits
/// are exactly the period, the bodies written count 2 and 3, each renewal is the holder's own
/// `If-Match` put, and the call gives what the work gave once the work ends. Catches a timer that
/// renews before it first waits, or that waits a different time than the period.
#[tokio::test]
async fn the_timer_renews_after_each_wait_of_the_period_while_the_work_runs() {
    let pair = pair();
    let (sleeper, mut waits, go) = StepSleeper::new();
    assert!(pair.holder.ensure().await);
    let (finish, rx) = work();
    let mut run = pin!(pair.holder.renewing(&sleeper, async { rx.await.is_ok() }));

    tokio::select! {
        _ = &mut run => panic!("the work ended before the test sent"),
        first = waits.recv() => assert_eq!(first, Some(ms(RENEW))),
    }
    assert_eq!(count(&pair.hot).await, "1");

    go.send(()).unwrap();
    tokio::select! {
        _ = &mut run => panic!("the work ended before the test sent"),
        second = waits.recv() => assert_eq!(second, Some(ms(RENEW))),
    }
    assert_eq!(count(&pair.hot).await, "2");

    go.send(()).unwrap();
    tokio::select! {
        _ = &mut run => panic!("the work ended before the test sent"),
        third = waits.recv() => assert_eq!(third, Some(ms(RENEW))),
    }
    assert_eq!(count(&pair.hot).await, "3");
    assert_eq!(pair.counted.calls(Op::PutIfMatch), 2);

    finish.send(()).unwrap();
    assert!(run.await);
}

/// Asserts that work that ends before the first wait is over writes nothing: the lease keeps its
/// first body and no `If-Match` put is made.
#[tokio::test]
async fn work_that_ends_before_the_first_wait_renews_nothing() {
    let pair = pair();
    let (sleeper, _waits, _go) = StepSleeper::new();
    assert!(pair.holder.ensure().await);

    assert_eq!(pair.holder.renewing(&sleeper, async { 7 }).await, 7);
    assert_eq!(pair.counted.calls(Op::PutIfMatch), 0);
    assert_eq!(count(&pair.hot).await, "1");
}

/// Asserts that the timer keeps the holder valid: after the renewal at the period the holder has
/// the margin left again at once, where without it the holder would have run out.
#[tokio::test]
async fn a_renewal_by_the_timer_gives_the_holder_a_new_validity() {
    let pair = pair();
    let (sleeper, mut waits, go) = StepSleeper::new();
    assert!(pair.holder.ensure().await);
    let (_finish, rx) = work();
    let mut run = pin!(pair.holder.renewing(&sleeper, async { rx.await.is_ok() }));
    tokio::select! {
        _ = &mut run => panic!("the work ended before the test sent"),
        _ = waits.recv() => {}
    }

    pair.holder_mono.advance(ms(RENEW + 1));
    assert!(!pair.holder.margin_ok());
    go.send(()).unwrap();
    tokio::select! {
        _ = &mut run => panic!("the work ended before the test sent"),
        _ = waits.recv() => {}
    }
    assert!(pair.holder.margin_ok());
}

/// Asserts that the timer does not take a lease: a lease that was never taken, or that the holder
/// lost (the timer's renewal found another tag), is not written by the timer, which asks for its
/// waits and makes no put at all after the one renewal that failed.
#[tokio::test]
async fn the_timer_never_takes_a_lease_and_stops_writing_after_the_lease_is_lost() {
    let pair = pair();
    let (sleeper, mut waits, go) = StepSleeper::new();
    let (_finish, rx) = work();
    let mut run = pin!(pair.holder.renewing(&sleeper, async { rx.await.is_ok() }));
    tokio::select! {
        _ = &mut run => panic!("the work ended before the test sent"),
        _ = waits.recv() => {}
    }
    go.send(()).unwrap();
    tokio::select! {
        _ = &mut run => panic!("the work ended before the test sent"),
        _ = waits.recv() => {}
    }
    assert_eq!(tag(&pair.hot).await, None);
    assert_eq!(pair.counted.calls(Op::Put), 0);
    assert_eq!(pair.counted.calls(Op::PutIfMatch), 0);

    assert!(pair.holder.ensure().await);
    let ours = tag(&pair.hot).await.unwrap();
    pair.hot
        .put_if_match(LEASE_KEY, "other\nn\n1".into(), &ours)
        .await
        .unwrap();
    for _ in 0..2 {
        go.send(()).unwrap();
        tokio::select! {
            _ = &mut run => panic!("the work ended before the test sent"),
            _ = waits.recv() => {}
        }
    }
    assert_eq!(pair.counted.calls(Op::PutIfMatch), 1);
    assert!(!pair.holder.margin_ok());
    assert!(!pair.holder.still_held().await.unwrap());
}

/// Asserts that a renewal period of zero is a millisecond, so that a timer never spins.
#[tokio::test]
async fn a_renewal_period_of_zero_waits_a_millisecond() {
    let pair = pair();
    let (sleeper, mut waits, _go) = StepSleeper::new();
    let lease = Lease::new(
        pair.counted.clone(),
        pair.holder_mono.clone(),
        "p",
        TTL,
        0,
        MARGIN,
    );
    let (_finish, rx) = work();
    let mut run = pin!(lease.renewing(&sleeper, async { rx.await.is_ok() }));
    tokio::select! {
        _ = &mut run => panic!("the work ended before the test sent"),
        first = waits.recv() => assert_eq!(first, Some(ms(1))),
    }
}

/// Asserts that the lease is renewed on the timer while a pass is stuck in one copy, and that the
/// pass then ends having held it throughout. The pass copies `k` and the copy parks in the cold
/// tier; the timer asks for its first wait (a third of the ttl, 10 s) and the pass has written
/// the lease once; the monotonic clock moves on by that wait and the wait is let go; the lease
/// then has its second body while the copy is still parked. Then the copy goes on and the pass
/// reports the copy and that it held the lease. Catches a reconciler that renews only when a pass
/// starts, which holds a lease for no longer than the ttl while one copy lasts.
#[tokio::test]
async fn the_lease_is_renewed_on_the_timer_while_a_pass_is_stuck_in_a_copy() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_memory = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let mut proxy_config = ProxyConfig::new("p1", 1 << 20, clock.clone());
    proxy_config.linger = ms(1);
    let proxy = Proxy::open(hot.clone(), cold_memory.clone(), proxy_config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();

    let cold = Arc::new(GateBackend::new(cold_memory));
    let gate = cold.gate(Op::Put, "k", Phase::Pre);
    let mono = Arc::new(Mono::new());
    let (sleeper, mut waits, go) = StepSleeper::new();
    let mut config = ReconcilerConfig::new("r1", 1 << 20, clock);
    config.mono = mono.clone();
    config.sleeper = Arc::new(sleeper);
    let mut reconciler = Reconciler::new(hot.clone(), cold, config);

    let mut pass = pin!(reconciler.pass());
    tokio::select! {
        _ = &mut pass => panic!("the pass ended before the copy parked"),
        () = gate.parked() => {}
    }
    assert_eq!(waits.recv().await, Some(ms(RENEW)));
    assert_eq!(count(&hot).await, "1");

    mono.advance(ms(RENEW));
    go.send(()).unwrap();
    tokio::select! {
        _ = &mut pass => panic!("the pass ended while its copy was parked"),
        second = waits.recv() => assert_eq!(second, Some(ms(RENEW))),
    }
    assert_eq!(count(&hot).await, "2");

    assert!(gate.release());
    let report = pass.await;
    assert!(report.held);
    assert_eq!(report.copied, 1);
    assert_eq!(report.failed, Vec::<Error>::new());
}

/// Asserts that a margin of zero does not keep a lease that has run out: the holder wrote at 0
/// and is valid until the ttl, and `margin_ok` is true before that time and false at it and
/// after it. Catches a check that compares only the time left, which is zero at and after the
/// end, and so passes for a margin of zero.
#[tokio::test]
async fn a_zero_margin_does_not_keep_a_lease_that_has_expired() {
    let mono = Arc::new(Mono::new());
    let hot = Arc::new(MemoryBackend::with_clock(Arc::new(TestClock::new())));
    let lease = Lease::new(hot, mono.clone(), "p", TTL, RENEW, 0);
    assert!(lease.ensure().await);

    mono.advance(ms(TTL - 1));
    assert!(lease.margin_ok());
    mono.advance(ms(1));
    assert!(!lease.margin_ok());
    mono.advance(ms(1));
    assert!(!lease.margin_ok());
}

/// Asserts that a taker counts the wait from when the answer of its look came: the HEAD is held
/// before the store, the lease object is replaced while it is held, and the taker's clock reads 29 s when the
/// answer arrives; so at 30 s, one second after the first sight of the new tag, the taker does
/// not take it, and does at 59 s. Catches a first sight stamped when the question was asked.
#[tokio::test]
async fn a_taker_counts_the_wait_from_the_answer_of_its_look() {
    let stamps = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(stamps));
    let gated = Arc::new(GateBackend::new(hot.clone()));
    let mono = Arc::new(Mono::new());
    let taker = Lease::new(gated.clone(), mono.clone(), "taker", TTL, RENEW, MARGIN);
    hot.put(LEASE_KEY, Bytes::from_static(b"old"))
        .await
        .unwrap();
    let gate = gated.gate(Op::Head, LEASE_KEY, Phase::Pre);

    let mut look = pin!(taker.ensure());
    tokio::select! {
        _ = &mut look => panic!("the look ended while its HEAD was held"),
        () = gate.parked() => {}
    }
    hot.delete(LEASE_KEY).await.unwrap();
    hot.put(LEASE_KEY, Bytes::from_static(b"new"))
        .await
        .unwrap();
    mono.advance(ms(TTL - 1_000));
    assert!(gate.release());
    assert!(!look.await);

    mono.advance(ms(1_000));
    assert!(!taker.ensure().await);
    mono.advance(ms(TTL - 1_000));
    assert!(taker.ensure().await);
}

/// Asserts that the timer does not stop the work while a renewal waits for the lock of a check of
/// the work: the holder's `still_held` parks in its HEAD with the lock, the timer's wait ends and
/// its renewal waits for the lock, and when the HEAD is let go the check ends with the lease held
/// (the pass is done, so the renewal that waited is dropped and no second body is written). Catches a timer that stops polling the work
/// to renew, which then waits for a lock only the work can give up.
#[tokio::test]
async fn a_renewal_waiting_for_the_lock_does_not_stop_the_work_that_holds_it() {
    let stamps = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(stamps));
    let gated = Arc::new(GateBackend::new(hot.clone()));
    let mono = Arc::new(Mono::new());
    let holder = Lease::new(gated.clone(), mono, "p", TTL, RENEW, MARGIN);
    assert!(holder.ensure().await);
    let (sleeper, mut waits, go) = StepSleeper::new();
    let gate = gated.gate(Op::Head, LEASE_KEY, Phase::Pre);

    let mut run = pin!(holder.renewing(&sleeper, holder.still_held()));
    tokio::select! {
        _ = &mut run => panic!("the check ended while its HEAD was held"),
        () = gate.parked() => {}
    }
    assert_eq!(waits.recv().await, Some(ms(RENEW)));
    go.send(()).unwrap();
    for _ in 0..8 {
        tokio::select! {
            _ = &mut run => panic!("the check ended while its HEAD was held"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(count(&hot).await, "1");

    assert!(gate.release());
    assert!(run.await.unwrap());
    assert_eq!(count(&hot).await, "1");
}
