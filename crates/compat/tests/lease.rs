//! The reconciler's lease object, `_tiering/lease`, shared by this branch and the release before
//! it, over one directory: each holds the lease the other can read, and each takes it from the
//! other by its own rule. The release before is the real `tierstore` library of its commit
//! (`tierstore_prev`): it takes a lease over when the object's time of last write is `ttl` old on
//! its wall clock, and this branch when the same tag has stood for `ttl` on its own monotonic
//! clock. The bytes of the object and the way it is replaced are the same in both.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{DirBackend, Mono, PassReport, Reconciler, ReconcilerConfig, TestClock};
use tierstore_prev::{
    Backend as PrevBackend, DirBackend as PrevDir, Reconciler as PrevReconciler,
    ReconcilerConfig as PrevConfig, TestClock as PrevClock,
};
use tierstore_testkit::dir::TestDir;

const LEASE: &str = "_tiering/lease";
const TTL_MILLIS: u64 = 30_000;

/// A hot and a cold directory, and the reconcilers of the two releases over them: this branch's
/// with a monotonic clock the test advances, the release before's with a wall clock the test sets.
struct Pair {
    before_hot: Arc<PrevDir>,
    mono: Arc<Mono>,
    before_clock: Arc<PrevClock>,
    now: Reconciler,
    before: PrevReconciler,
    _dirs: [TestDir; 2],
}

fn pair(name: &str) -> Pair {
    let hot = TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("compat-lease-hot-{name}"),
    );
    let cold = TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("compat-lease-cold-{name}"),
    );
    let mono = Arc::new(Mono::new());
    let mut config = ReconcilerConfig::new("new", 1 << 20, Arc::new(TestClock::new()));
    config.mono = mono.clone();
    let now = Reconciler::new(
        Arc::new(DirBackend::open(hot.path()).unwrap()),
        Arc::new(DirBackend::open(cold.path()).unwrap()),
        config,
    );
    let before_hot = Arc::new(PrevDir::open(hot.path()).unwrap());
    let before_clock = Arc::new(PrevClock::new());
    let before = PrevReconciler::new(
        before_hot.clone(),
        Arc::new(PrevDir::open(cold.path()).unwrap()),
        PrevConfig::new("old", 1 << 20, before_clock.clone()),
    );
    Pair {
        before_hot,
        mono,
        before_clock,
        now,
        before,
        _dirs: [hot, cold],
    }
}

/// The first line of the lease object, the holder's name, as the release before reads it.
async fn holder(pair: &Pair) -> String {
    let body = pair.before_hot.get(LEASE, None).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    text.lines().next().unwrap().to_owned()
}

/// Asserts that the release before reads and takes over a lease this branch wrote, by the time
/// of last write of the object as it always did: it finds the lease held 29.999 s after the write
/// and takes it at 30 s, replacing the object by its tag, and this branch's next pass, which is
/// past the margin of its lease, finds the object is another holder's and does nothing. The test
/// sets the release before's clock from the object's own time of last write, so that nothing
/// depends on the time of the machine.
#[tokio::test]
async fn the_previous_release_takes_a_lease_this_branch_wrote_and_this_branch_then_stops() {
    let mut pair = pair("by-old");
    assert!(pair.now.pass().await.held);
    assert_eq!(holder(&pair).await, "new");

    let written = pair.before_hot.head(LEASE).await.unwrap().modified;
    pair.before_clock.set(written + TTL_MILLIS - 1);
    assert!(!pair.before.pass().await.held);
    assert_eq!(holder(&pair).await, "new");
    pair.before_clock.set(written + TTL_MILLIS);
    assert!(pair.before.pass().await.held);
    assert_eq!(holder(&pair).await, "old");

    pair.mono.advance(Duration::from_millis(TTL_MILLIS / 3 + 1));
    assert_eq!(pair.now.pass().await, PassReport::default());
    assert_eq!(holder(&pair).await, "old");
}

/// Asserts that this branch reads and takes over a lease the release before wrote: its first
/// pass finds the lease held and writes the tag down, it still finds it held 29.999 s of its
/// monotonic clock later, and takes it at 30 s; the release before, whose next pass renews by the
/// tag it wrote, finds the object replaced and does nothing.
#[tokio::test]
async fn this_branch_takes_a_lease_the_previous_release_wrote_and_the_previous_release_then_stops()
{
    let mut pair = pair("by-new");
    assert!(pair.before.pass().await.held);
    assert_eq!(holder(&pair).await, "old");

    assert!(!pair.now.pass().await.held);
    pair.mono.advance(Duration::from_millis(TTL_MILLIS - 1));
    assert!(!pair.now.pass().await.held);
    assert_eq!(holder(&pair).await, "old");
    pair.mono.advance(Duration::from_millis(1));
    assert!(pair.now.pass().await.held);
    assert_eq!(holder(&pair).await, "new");

    assert!(!pair.before.pass().await.held);
    assert_eq!(holder(&pair).await, "new");
}
