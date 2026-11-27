//! The ask of a start whose file has an `fs` interface on `server.handoff_socket`
//! ([`offered_with`] with no mount): the `ready` step and the cache lock with the sockets, the lock of
//! another file left with the running instance, and a running instance that knows no `ready` step.

use std::path::Path;
use std::time::Duration;

use crate::server::handoff::testing::{handoff, listening, scratch, settle, socket, within};
use crate::server::handoff::{Front, HandoffError, Offer};
use crate::server::local_cache::testing::Rig;
use crate::server::local_cache::{LocalCache, Mode};
use crate::server::lock::try_take;

use super::{Ask, Inherited, Offered, Want, offered_with};

/// The wait of the `ready` step: only a hung test reaches it.
const WAIT: Duration = Duration::from_secs(3600);

/// Whether a third process would take the lock at `path` now: it takes and lets it go at once.
fn free(path: &Path) -> bool {
    try_take(path).unwrap().is_some()
}

/// The offer of a running instance at `path` with its S3 socket `listener`, taking part in the
/// `ready` step when `ready` says so, and offering the lock of `cache`.
fn cache_offer(
    path: &Path,
    listener: &std::net::TcpListener,
    cache: LocalCache,
    ready: bool,
) -> Offer {
    let offer = Offer::bind(path, vec![socket(Front::S3, listener)]).unwrap();
    let offer = if ready {
        offer.with_ready_wait(WAIT)
    } else {
        offer
    };
    offer.with_lock(cache)
}

/// The offer of the running instance on `handoff` to a start that asks for the `ready` step and
/// the lock at `lock`, and leaves a lock of another file with that instance: what a start whose
/// file has an `fs` interface asks of an instance that holds no mount.
async fn offered_with_lock(
    handoff: &crate::server::handoff::Handoff,
    want: &Want<'_>,
    lock: &Path,
) -> Result<Option<Offered>, HandoffError> {
    let ask = Ask {
        mount: None,
        lock: Some(lock),
        leaves_another_lock: true,
    };
    offered_with(handoff, want, ask).await
}

/// What a start whose S3 address is `addr` and whose lock file is `lock` gets of the running
/// instance at `path`: the offer, accepted.
async fn ask(path: &Path, addr: std::net::SocketAddr, lock: &Path) -> Inherited {
    let addrs = [addr];
    let want = Want {
        s3: &addrs,
        oci: &[],
        nfs: &[],
    };
    let offered = offered_with_lock(&handoff(path.to_owned()), &want, lock).await;
    let offered = offered.unwrap().expect("an instance answers");
    within("the offer is accepted", offered.accept())
        .await
        .unwrap()
}

/// Asserts that the start is offered the sockets, the `ready` step and the lock of its own cache:
/// it gets the lock file it names, the running instance serves on (and holds the lock) until it
/// is told `ready`, and at `ready` that instance answers `bye`, ends, and is read-only from then
/// on, with the lock still held for the successor (no third process takes it). Catches a start
/// that asks for the sockets alone, whose cache then waits for the running instance's whole drain.
#[tokio::test]
async fn a_start_is_handed_the_sockets_the_ready_step_and_its_cache_lock() {
    let rig = Rig::new("handoff-ask-lock");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let serving = tokio::spawn(cache_offer(&path, &listener, old.clone(), true).serve());
    let taken = ask(&path, addr, &lock).await;
    assert_eq!(taken.s3_addrs, [addr]);
    let received = taken.lock.expect("the lock of its own cache");
    assert!(received.is_current());
    settle().await;
    assert!(!serving.is_finished(), "it serves until ready");
    assert_eq!(old.mode(), Mode::ReadWrite);
    let ready = taken.ready.expect("the ready step");
    let _predecessor = within("bye", ready.send_keeping()).await.unwrap();
    within("the running instance ends", serving).await.unwrap();
    assert_eq!(old.mode(), Mode::ReadOnly);
    assert!(!free(&lock));
    drop(received);
}

/// Asserts that a lock the running instance offers of another file than the start's (its file names
/// another cache directory) is closed, not refused: the start gets the sockets and the `ready` step
/// and no lock, the running instance keeps its lock (no third process takes it) and stays the writer
/// until `ready`. Catches a start that fails a hand-over over a cache directory the change moved,
/// which a take of the sockets alone hands over.
#[tokio::test]
async fn a_lock_of_another_file_is_left_with_the_running_instance_and_the_step_taken() {
    let rig = Rig::new("handoff-ask-other-lock");
    let held = rig.config.path.join(".lock");
    let old = rig.locked();
    let (dir, path) = scratch();
    let elsewhere = dir.path().join("other-cache.lock");
    let (listener, addr) = listening();
    let serving = tokio::spawn(cache_offer(&path, &listener, old.clone(), true).serve());
    let taken = ask(&path, addr, &elsewhere).await;
    assert_eq!(taken.s3_addrs, [addr]);
    assert!(taken.lock.is_none());
    assert!(!free(&held));
    assert_eq!(old.mode(), Mode::ReadWrite);
    let ready = taken.ready.expect("the ready step");
    within("bye", ready.send()).await.unwrap();
    within("the running instance ends", serving).await.unwrap();
    assert!(
        !elsewhere.exists(),
        "nothing was made at the start's own path"
    );
}

/// Asserts that a running instance that takes no `ready` step (it is given no wait, as every
/// release before the step) is asked with `take 1` alone and hands the sockets over as before: no
/// `ready` to say, no lock, and it ended at `done`, still holding its lock until it exits. Catches
/// a start that fails, or waits, on an instance of a release before the step.
#[tokio::test]
async fn an_instance_without_the_ready_step_hands_the_sockets_alone() {
    let rig = Rig::new("handoff-ask-plain");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let serving = tokio::spawn(cache_offer(&path, &listener, old.clone(), false).serve());
    let taken = ask(&path, addr, &lock).await;
    assert_eq!(taken.s3_addrs, [addr]);
    assert!(taken.ready.is_none());
    assert!(taken.lock.is_none());
    within("the running instance ends", serving).await.unwrap();
    assert_eq!(old.mode(), Mode::ReadWrite);
    assert!(!free(&lock));
}

/// Asserts that an offer whose sockets are not the start's is still refused with `nak`, as before:
/// the error names the offer's socket line, and the running instance serves on as the writer.
/// Catches a start that lets a mismatch of the sockets pass as it lets a lock of another file pass.
#[tokio::test]
async fn a_mismatch_of_the_sockets_is_still_refused() {
    let rig = Rig::new("handoff-ask-mismatch");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let (_other, elsewhere) = listening();
    let serving = tokio::spawn(cache_offer(&path, &listener, old.clone(), true).serve());
    let addrs = [elsewhere];
    let want = Want {
        s3: &addrs,
        oci: &[],
        nfs: &[],
    };
    let refused = offered_with_lock(&handoff(path.clone()), &want, &lock).await;
    let Err(HandoffError::Mismatch { offered, .. }) = refused else {
        panic!("a mismatch of the sockets");
    };
    assert_eq!(offered[0], format!("s3 {addr}"));
    settle().await;
    assert!(!serving.is_finished());
    assert_eq!(old.mode(), Mode::ReadWrite);
    serving.abort();
}
