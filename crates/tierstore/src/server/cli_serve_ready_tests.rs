//! The sockets of a start whose file has an `fs` interface ([`ask_sockets`], then [`bind_fronts`]
//! with them): the running instance is told `ready` before the start makes its control socket,
//! the lock it handed over is followed, and a running instance that is gone before `ready` fails
//! the start before it serves.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::{Bound, Got, HandingError, ask_sockets, bind_fronts};
use crate::server::cli::Error;
use crate::server::handoff::testing::{handoff, listening, scratch, socket, within};
use crate::server::handoff::{Front, Offer};
use crate::server::local_cache::testing::Rig;
use crate::server::local_cache::{LocalCache, Mode};
use crate::server::lock::try_take;
use crate::{Backend, MemoryBackend};

/// Whether a third process would take the lock at `path` now: it takes and lets it go at once.
fn free(path: &Path) -> bool {
    try_take(path).unwrap().is_some()
}

/// A running instance at `path` serving `listener` and offering the lock of `cache`, in the
/// `ready` step.
fn running(path: &Path, listener: &std::net::TcpListener, cache: LocalCache) -> JoinHandle<()> {
    let offer = Offer::bind(path, vec![socket(Front::S3, listener)])
        .unwrap()
        .with_ready_wait(Duration::from_secs(3600))
        .with_lock(cache);
    tokio::spawn(offer.serve())
}

/// The sockets the running instance at `path` offers to a start whose S3 address is `addr` and
/// whose lock file is `lock`, accepted (the running instance has said `done`).
async fn taken(path: &Path, addr: SocketAddr, lock: &Path) -> Got {
    let asked = ask_sockets(&handoff(path.to_owned()), &[addr], &[], lock).await;
    let offered = asked.unwrap().expect("an instance answers");
    let inherited = within("done", offered.accept()).await.unwrap();
    Got::Taken(Box::new(inherited))
}

/// [`bind_fronts`] for the S3 address `addr` over the control socket at `path`, with `got`: a start
/// that took sockets binds none of its own.
async fn binding(path: &Path, addr: SocketAddr, got: Got) -> Result<Bound, HandingError> {
    let own = || -> Result<Vec<TcpListener>, Error> { panic!("a start that took binds none") };
    let backend: std::sync::Arc<dyn Backend> = std::sync::Arc::new(MemoryBackend::new());
    let handoff = handoff(path.to_owned());
    let binding = bind_fronts(Some(&handoff), Some(got), &[addr], own, None, backend);
    within("the fronts are bound", binding).await
}

/// [`binding`], which goes on.
async fn bound(path: &Path, addr: SocketAddr, got: Got) -> Bound {
    let bound = binding(path, addr, got).await;
    bound.unwrap_or_else(|why| panic!("the start goes on: {why}"))
}

/// Asserts that the sockets of a running instance that takes part in the `ready` step are told
/// `ready` by [`bind_fronts`]: that instance answers `bye` and ends, and is read-only from then on;
/// the start serves the taken socket, makes its control socket, and has the lock it was handed to
/// follow (the lock file of its cache, held for it: no third process takes it), not to take at
/// once. Catches a start that never says `ready`, which leaves the running instance serving until
/// its wait runs out and then serving on, or one that takes the write role before `written`.
#[tokio::test]
async fn sockets_taken_with_the_ready_step_are_told_ready_and_the_lock_is_followed() {
    let rig = Rig::new("serve-ready-lock");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let serving = running(&path, &listener, old.clone());
    let got = taken(&path, addr, &lock).await;
    let bound = bound(&path, addr, got).await;
    within("the running instance ends at ready", serving)
        .await
        .unwrap();
    assert_eq!(old.mode(), Mode::ReadOnly);
    let addrs: Vec<SocketAddr> = bound.s3.iter().map(|l| l.local_addr().unwrap()).collect();
    assert_eq!(addrs, [addr]);
    assert!(bound.offer.is_some());
    assert!(path.exists(), "the start's own control socket");
    assert!(bound.lock.is_none(), "nothing to take at once");
    let (_predecessor, received) = bound.follow.expect("the lock to follow");
    assert!(received.is_current());
    assert!(!free(&lock));
}

/// Asserts that a running instance that is gone before the start says `ready` (its conversation
/// ended after `done`: it gave up first and serves on, or has exited) fails the start, by name,
/// before it serves anything: no control socket of its own is made (the path stays that
/// instance's), and the lock stays with that instance, which is still its writer. Catches a start
/// that serves the sockets beside an instance that serves them on, or takes the write role or the
/// control socket's path from an instance that did not hand over.
#[tokio::test]
async fn a_running_instance_gone_before_ready_fails_the_start_before_it_serves() {
    let rig = Rig::new("serve-ready-gone");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let serving = running(&path, &listener, old.clone());
    let got = taken(&path, addr, &lock).await;
    serving.abort();
    let _ = serving.await;
    let Err(failed) = binding(&path, addr, got).await else {
        panic!("the start must not go on beside the running instance");
    };
    assert!(
        failed
            .to_string()
            .starts_with("hand-off: telling the running instance it can end: "),
        "{failed}"
    );
    assert!(!path.exists());
    assert!(!free(&lock));
    assert_eq!(old.mode(), Mode::ReadWrite);
}

/// Asserts that a running instance that offers no lock (its cache is read-only: another process
/// holds the lock) is still told `ready`, and ends: the start makes its control socket and has no
/// lock to follow, so its cache takes the lock at a tick once the holder lets it go. Catches a
/// start that says `ready` only when it was handed the lock.
#[tokio::test]
async fn an_instance_that_offers_no_lock_is_told_ready_and_nothing_is_followed() {
    let rig = Rig::new("serve-ready-nolock");
    let lock = rig.config.path.join(".lock");
    let _holder = rig.locked();
    let old = rig.locked();
    assert_eq!(old.mode(), Mode::ReadOnly);
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let serving = running(&path, &listener, old);
    let got = taken(&path, addr, &lock).await;
    let bound = bound(&path, addr, got).await;
    within("the running instance ends at ready", serving)
        .await
        .unwrap();
    assert!(bound.offer.is_some());
    assert!(bound.lock.is_none());
    assert!(bound.follow.is_none());
}
