//! The order in which two instances of a combined node let go of what they hold and take it up,
//! through a hand-over: two `serve_combined` in one process over the same tiers, cache
//! directory, hot-tier lock file and control socket, with the debug events of the shutdown and
//! the start-up recorded in the order they were emitted (no times).
//!
//! The runtime is the test's single thread and the subscriber is that thread's default, so the
//! events of every task of both instances are in one list. An event of a thread that may block
//! (a hold tick) is not on it; the hold loop says on the async path that it took the lock.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::sync::oneshot;
use tracing::Level;
use tracing::subscriber::set_default;

use super::Drained;
use super::cli::{Addresses, Error};
use super::combined_cli::serve_combined;
use super::combined_cli_tests::{Node, QUICK};
use super::config::settings_from_text;
use super::config_combined::CombinedSettings;
use super::config_testing::oci_interface;
use super::local_cache::{Disk, SystemDisk};
use crate::server::build_info::stderr_log::LineLog;

/// How many looks a wait for an event makes, with [`PAUSE`] between two.
const TRIES: u32 = 1_000;

/// A short pause between two looks at the events.
const PAUSE: Duration = Duration::from_millis(20);

/// The event the successor's taking of the sockets ends the first instance's offer with.
const TAKEN: &str = "tierstore: hand-off: the sockets were taken";

/// An event of a lease taken, and of one given up (a nonce follows).
pub(super) const LEASE_TAKEN: &str = "tierstore: lease taken";
pub(super) const LEASE_RELEASED: &str = "tierstore: lease released";

/// The events of the test, one line each, in the order they were emitted.
#[derive(Clone, Default)]
pub(super) struct Events(Arc<Mutex<Vec<u8>>>);

impl Write for Events {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Events {
    /// The lines so far.
    pub(super) fn lines(&self) -> Vec<String> {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Looks at most [`TRIES`] times for the lines to be ones for which `wanted` holds, and
    /// panics with the lines if they never are.
    async fn wait_for(&self, what: &str, wanted: impl Fn(&[String]) -> bool) {
        for _ in 0..TRIES {
            if wanted(&self.lines()) {
                return;
            }
            tokio::time::sleep(PAUSE).await;
        }
        panic!("no event: {what}\n{:#?}", self.lines());
    }
}

/// The index of the first line from `from` on for which `wanted` holds; panics with the lines if
/// there is none.
pub(super) fn first_from(
    lines: &[String],
    from: usize,
    what: &str,
    wanted: impl Fn(&str) -> bool,
) -> usize {
    let found = lines.iter().skip(from).position(|line| wanted(line));
    match found {
        Some(at) => at + from,
        None => panic!("no event: {what}\n{lines:#?}"),
    }
}

/// The indexes of all lines for which `wanted` holds.
pub(super) fn all(lines: &[String], wanted: impl Fn(&str) -> bool) -> Vec<usize> {
    (0..lines.len()).filter(|at| wanted(&lines[*at])).collect()
}

/// The settings of the node with the hand-off over `socket`, a lock file for the hot tier, and
/// an S3 and an OCI front.
fn settings(node: &Node, socket: &Path, lock: &Path) -> CombinedSettings {
    let server = format!("handoff_socket = {:?}", socket.display().to_string());
    let hot = format!("lock_file = {:?}", lock.display().to_string());
    let oci = oci_interface("127.0.0.1:0", ", prefix = \"cas/\"");
    let text = node.text_with(&server, &hot, &oci);
    settings_from_text(&text, QUICK)
        .unwrap()
        .into_combined()
        .unwrap()
}

/// Runs `serve_combined` with `settings` on the real disk, tells `told` the addresses, and stops
/// when `stopped` resolves or is dropped.
async fn instance(
    settings: &CombinedSettings,
    told: oneshot::Sender<Addresses>,
    stopped: oneshot::Receiver<()>,
) -> Result<Drained, Error> {
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    serve_combined(
        settings,
        disk,
        async move {
            let _ = stopped.await;
        },
        move |at| drop(told.send(at)),
    )
    .await
}

/// What a hand-over left: the events in order, and the two lock files the instances held.
struct Handed {
    lines: Vec<String>,
    /// The hot tier's lock file.
    hot_lock: PathBuf,
    /// The cache directory's lock file.
    cache_lock: PathBuf,
}

/// Runs a hand-over of a node with an S3 and an OCI front, a cache and a hot-tier lock file, and
/// records the events of both instances. The second instance starts when the first has bound;
/// it is stopped once it holds the cache directory's lock and the lease.
async fn handed_over(name: &str) -> Handed {
    let events = Events::default();
    let _recording = set_default(LineLog::new(events.clone(), Level::DEBUG));
    let node = Node::new(name);
    node.activate().await;
    let dir = TestDir::under(std::env::temp_dir(), name);
    let socket = dir.path().join("control.sock");
    let hot_lock = node.dir.path().join("instance.lock");
    let settings = settings(&node, &socket, &hot_lock);
    let (old_told, old_bound) = oneshot::channel();
    let (new_told, new_bound) = oneshot::channel();
    let (go, going) = oneshot::channel::<()>();
    let (stop_old, stopped_old) = oneshot::channel();
    let (stop_new, stopped_new) = oneshot::channel();
    let first = instance(&settings, old_told, stopped_old);
    let second = async {
        going.await.unwrap();
        instance(&settings, new_told, stopped_new).await
    };
    let seen = events.clone();
    let driving = async move {
        let at = old_bound.await.unwrap();
        go.send(()).unwrap();
        let taken = new_bound.await.unwrap();
        assert_eq!(taken, at, "the second serves the very sockets");
        seen.wait_for("the second took the cache lock", |lines| {
            lines
                .iter()
                .any(|line| line.contains("took the lock, read-write"))
        })
        .await;
        seen.wait_for("the second took the lease", |lines| {
            all(lines, |line| line.starts_with(LEASE_TAKEN)).len() == 2
        })
        .await;
        let _ = stop_new.send(());
        drop(stop_old);
    };
    let (old, new, ()) = tokio::join!(first, second, driving);
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
    Handed {
        lines: events.lines(),
        hot_lock,
        cache_lock: node.cache_dir().join(".lock"),
    }
}

/// Asserts (T27) the order of a hand-over between two instances of a combined node with an S3
/// and an OCI front, a cache (with its scrub, maintenance and hold loops) and a hot-tier lock
/// file, read from the debug events of both:
///
/// - the first instance, after its sockets were taken: its drain begins, then its fronts are
///   drained, then its maintenance, hold, scrub and reconciler loops end; its lease is given up
///   before its hot-tier lock file is let go; and the cache directory's lock is let go after all
///   of that, so that no event of the instance follows it;
/// - the second instance: it starts no reconciler round, takes no lease and takes the hot-tier
///   lock file only after the first let the hot-tier lock file go; it takes the cache directory's
///   lock (read-write) only after the first let that go;
/// - `Proxy::open` is called twice, once for each instance.
///
/// The two leases have different nonces, which is how the instances are told apart.
/// Catches: a first instance whose loops go on after it let go of its locks, a lock let go
/// before the lease, and a second instance that works beside the first.
#[tokio::test]
async fn the_first_instance_lets_go_in_order_and_the_second_takes_up_only_after() {
    let Handed {
        lines,
        hot_lock,
        cache_lock,
    } = handed_over("tscombined-order").await;
    let hot = hot_lock.display().to_string();
    let cache = cache_lock.display().to_string();
    let hot_taken = format!("tierstore: lock file taken: {hot}");
    let hot_waited = format!("tierstore: lock file held after waiting: {hot}");
    let hot_released = format!("tierstore: lock file released: {hot}");
    let cache_released = format!("tierstore: lock file released: {cache}");

    let sockets = first_from(&lines, 0, "the sockets were taken", |l| l == TAKEN);
    let drain = first_from(&lines, sockets, "drain begun", |l| {
        l == "tierstore: drain begun"
    });
    let fronts = first_from(&lines, drain, "fronts drained", |l| {
        l == "tierstore: fronts drained"
    });
    let ended = |name: &str| {
        let event = format!("tierstore: {name} ended");
        first_from(&lines, fronts, &event, |l| l == event)
    };
    let tasks = ["maintain", "hold", "scrub", "reconciler"].map(ended);

    let taken = all(&lines, |l| l.starts_with(LEASE_TAKEN));
    let released = all(&lines, |l| l.starts_with(LEASE_RELEASED));
    assert_eq!((taken.len(), released.len()), (2, 2), "{lines:#?}");
    let nonce = |at: usize, prefix: &str| lines[at][prefix.len()..].to_owned();
    let (old_nonce, new_nonce) = (nonce(taken[0], LEASE_TAKEN), nonce(taken[1], LEASE_TAKEN));
    assert_ne!(old_nonce, new_nonce);
    assert_eq!(nonce(released[0], LEASE_RELEASED), old_nonce);
    assert_eq!(nonce(released[1], LEASE_RELEASED), new_nonce);

    // The first instance takes the hot lock file when it starts; the second waits for it.
    let first_takes = all(&lines, |l| l == hot_taken);
    let second_takes = all(&lines, |l| l == hot_waited);
    assert_eq!(
        (first_takes.len(), second_takes.len()),
        (1, 1),
        "{lines:#?}"
    );
    let hot_lets_go = all(&lines, |l| l == hot_released);
    let cache_lets_go = all(&lines, |l| l == cache_released);
    assert_eq!(hot_lets_go.len(), 2, "{lines:#?}");
    assert_eq!(cache_lets_go.len(), 2, "{lines:#?}");
    let new_took_cache = first_from(&lines, 0, "the second took the cache lock", |l| {
        l.contains("took the lock, read-write")
    });

    // The first instance: drain, fronts, loops, lease, hot lock, cache lock.
    assert!(sockets < drain && drain < fronts, "{lines:#?}");
    for task in tasks {
        assert!(fronts < task, "a loop ended before the fronts: {lines:#?}");
        assert!(
            task < cache_lets_go[0],
            "a loop ended after the cache lock was let go: {lines:#?}"
        );
    }
    assert!(
        released[0] < hot_lets_go[0],
        "the lease went after the lock: {lines:#?}"
    );
    assert!(hot_lets_go[0] < cache_lets_go[0], "{lines:#?}");
    // Nothing of the first instance is left to follow the release of the cache directory's lock:
    // each of its events is there once before it, the second instance's after.
    let before_release = &lines[..cache_lets_go[0]];
    for event in [
        "tierstore: drain begun",
        "tierstore: fronts drained",
        "tierstore: maintain ended",
        "tierstore: hold ended",
        "tierstore: scrub ended",
        "tierstore: reconciler ended",
    ] {
        let count = |lines: &[String]| lines.iter().filter(|l| *l == event).count();
        assert_eq!(
            (count(before_release), count(&lines)),
            (1, 2),
            "{event}: {lines:#?}"
        );
    }

    // The second instance: nothing before the first let the hot lock go; the cache after.
    let rounds = all(&lines, |l| l == "tierstore: reconciler round start");
    for round in rounds.iter().filter(|at| **at > released[0]) {
        assert!(
            *round > hot_lets_go[0] && *round > second_takes[0],
            "a round before the second held the hot lock: {lines:#?}"
        );
    }
    assert!(!rounds.is_empty() && rounds.iter().any(|at| *at > second_takes[0]));
    assert!(hot_lets_go[0] < second_takes[0], "{lines:#?}");
    assert!(
        released[0] < taken[1] && hot_lets_go[0] < taken[1],
        "{lines:#?}"
    );
    assert!(second_takes[0] < taken[1], "{lines:#?}");
    assert!(cache_lets_go[0] < new_took_cache, "{lines:#?}");

    let opens = all(&lines, |l| l.starts_with("tierstore: proxy opened"));
    assert_eq!(opens.len(), 2, "{lines:#?}");
}
