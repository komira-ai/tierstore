//! The sockets of a start with a hand-off ([`bind_fronts`]) at the edges of the commit point, and
//! [`stopped_by`].
//!
//! The commit point is the running instance's `done`: once it is sent, that instance drains. So
//! every check that can fail is made before the start asks (T05), and what follows `done` cannot
//! stop the start.

use std::cell::Cell;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

use super::{Bound, HandingError, bind_fronts, bind_s3, stopped_by};
use crate::server::cli::Error;
use crate::server::handoff::testing::{pretending_with, scratch, within};
use crate::server::handoff::{Handoff, HandoffError, Offer, Socket};
use crate::{Backend, MemoryBackend};

/// The hand-off over `path`, with a wait of `wait`.
fn over(path: &Path, wait: Duration) -> Handoff {
    Handoff {
        socket: path.to_owned(),
        wait,
    }
}

/// A backend for the OCI front, which these starts do not have.
fn backend() -> Arc<dyn Backend> {
    Arc::new(MemoryBackend::new())
}

/// A fake running instance at `path` that only accepts: a taker that connects is seen in its
/// queue.
fn listening_only(path: &Path) -> std::os::unix::net::UnixListener {
    let listener = std::os::unix::net::UnixListener::bind(path).expect("a control socket");
    listener.set_nonblocking(true).expect("non-blocking");
    listener
}

/// Whether a taker connected to `listener`: a connection waits in its queue even after the taker
/// has gone.
fn was_asked(listener: &std::os::unix::net::UnixListener) -> bool {
    match listener.accept() {
        Ok(_) => true,
        Err(why) if why.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(why) => panic!("the fake instance's accept failed: {why}"),
    }
}

/// The hand-off error of `error`, if it is one.
fn handoff_error(error: &HandingError) -> Option<&HandoffError> {
    match error {
        HandingError::Handoff(why) => Some(why),
        _ => None,
    }
}

/// Runs [`bind_fronts`] for the S3 addresses `s3` over `handoff`, and says whether it bound
/// sockets of its own.
async fn start(handoff: &Handoff, s3: &[SocketAddr]) -> (Result<Bound, HandingError>, bool) {
    let bound_own = Cell::new(false);
    let own = || -> Result<Vec<TcpListener>, Error> {
        bound_own.set(true);
        bind_s3(s3, crate::server::ListenTuning::default().backlog)
    };
    let started = bind_fronts(Some(handoff), s3, own, None, backend()).await;
    (started, bound_own.get())
}

/// Asserts (T05) that a start with more S3 sockets than one message of the hand-off can carry is
/// refused before it asks the running instance anything: `TooMany`, the running instance's
/// control socket was never connected to, and no socket was bound. Catches a start that takes the
/// running instance's sockets, so that it drains, and only then finds it cannot offer them on.
#[tokio::test]
async fn more_sockets_than_a_message_carries_are_refused_before_the_start_asks() {
    let (_dir, path) = scratch();
    let running = listening_only(&path);
    let addrs: Vec<SocketAddr> = (0..254).map(|_| "127.0.0.1:0".parse().unwrap()).collect();
    let (started, bound_own) = start(&over(&path, Duration::from_millis(50)), &addrs).await;
    let error = started.err().expect("refused");
    assert!(
        matches!(handoff_error(&error), Some(HandoffError::TooMany(254))),
        "{error}"
    );
    assert!(!was_asked(&running), "the running instance was asked");
    assert!(!bound_own);
}

/// Asserts (T05) that a control socket in a directory that does not exist is refused before
/// anything is asked or bound, with the path and what the system said in the exact words.
/// Catches a start that binds its sockets and only then finds it cannot make its control socket.
#[tokio::test]
async fn a_control_socket_in_a_missing_directory_is_refused_before_anything_is_bound() {
    let (dir, _) = scratch();
    let missing = dir.path().join("gone");
    let path = missing.join("control.sock");
    let addrs: Vec<SocketAddr> = vec!["127.0.0.1:0".parse().unwrap()];
    let (started, bound_own) = start(&over(&path, Duration::from_secs(3600)), &addrs).await;
    let error = started.err().expect("refused");
    assert_eq!(
        error.to_string(),
        format!(
            "hand-off: the directory of the control socket {}: No such file or directory (os \
             error 2)",
            missing.display()
        )
    );
    assert!(!bound_own);
}

/// Asserts (T05) that a control socket path that is a directory is refused before anything is
/// asked or bound, naming the path. Catches a start that binds its sockets and then fails to
/// replace the directory with its control socket.
#[tokio::test]
async fn a_control_socket_path_that_is_a_directory_is_refused_before_anything_is_bound() {
    let (_dir, path) = scratch();
    std::fs::create_dir(&path).unwrap();
    let addrs: Vec<SocketAddr> = vec!["127.0.0.1:0".parse().unwrap()];
    let (started, bound_own) = start(&over(&path, Duration::from_secs(3600)), &addrs).await;
    let error = started.err().expect("refused");
    assert_eq!(
        error.to_string(),
        format!(
            "hand-off: the control socket {}: is a directory, not a socket",
            path.display()
        )
    );
    assert!(!bound_own);
}

/// Asserts (T05) that a control socket in a directory this user may not write is refused before
/// the running instance is asked: the instance's control socket was never connected to, and no
/// socket was bound. A user who may write any directory (root) is not refused: for it the test
/// asserts that the start does ask. Catches a start that takes the sockets, so that the running
/// instance drains, and then cannot make its own control socket.
#[tokio::test]
async fn a_control_socket_in_a_directory_that_cannot_be_written_is_refused_before_the_start_asks() {
    let (dir, path) = scratch();
    let running = listening_only(&path);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let addrs: Vec<SocketAddr> = vec!["127.0.0.1:0".parse().unwrap()];
    let (started, bound_own) = start(&over(&path, Duration::from_millis(50)), &addrs).await;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = started.err().expect("refused");
    assert!(!bound_own);
    if rustix::process::geteuid().is_root() {
        assert!(
            was_asked(&running),
            "root may write the directory, so it asks"
        );
        return;
    }
    assert_eq!(
        error.to_string(),
        format!(
            "hand-off: the directory of the control socket {}: Permission denied (os error 13)",
            dir.path().display()
        )
    );
    assert!(!was_asked(&running), "the running instance was asked");
}

/// Asserts (T05, the decided case after the commit point) that a control socket that cannot be
/// made once the running instance has said `done` (here a directory is put at its path between
/// the `ack` and the `done`) does not fail the start: the start serves the sockets it took, with
/// no control socket, and binds none of its own. Catches a start that fails after the running
/// instance has begun its drain, which leaves the ports with no one to serve them.
#[tokio::test]
async fn a_control_socket_that_cannot_be_made_after_done_leaves_the_start_serving() {
    let (_dir, path) = scratch();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = held.local_addr().unwrap();
    let blocked = path.clone();
    let running = pretending_with(
        &path,
        vec!["offer".to_owned(), format!("s3 {at}")],
        vec![held],
        vec!["done".to_owned()],
        move || {
            std::fs::remove_file(&blocked).unwrap();
            std::fs::create_dir(&blocked).unwrap();
        },
    );
    let (started, bound_own) = start(&over(&path, Duration::from_secs(3600)), &[at]).await;
    within("the fake instance", running).await.unwrap();
    let bound = started.expect("the start goes on");
    assert!(bound.offer.is_none(), "no control socket");
    assert!(!bound_own);
    let addrs: Vec<SocketAddr> = bound.s3.iter().map(|l| l.local_addr().unwrap()).collect();
    assert_eq!(addrs, vec![at]);
    assert!(path.is_dir());
}

/// Asserts (T03) [`stopped_by`] without an offer: it ends exactly when the stop does.
#[tokio::test]
async fn without_an_offer_the_stop_is_the_stop() {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let waiting = tokio::spawn(stopped_by(
        async move {
            let _ = stopped.await;
        },
        None,
    ));
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    stop.send(()).unwrap();
    within("the stop", waiting).await.unwrap();
}

/// Asserts (T03) [`stopped_by`] with an offer: it ends when a taker has taken the sockets, though
/// the stop never comes, and, with an offer no one takes, when the stop comes.
#[tokio::test]
async fn an_offer_that_is_taken_is_a_stop() {
    let (_dir, path) = scratch();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = held.local_addr().unwrap();
    let sockets = vec![Socket::new(
        crate::server::handoff::Front::S3,
        held.try_clone().unwrap(),
    )];
    let offer = Offer::bind(&path, sockets).unwrap();
    let waiting = tokio::spawn(stopped_by(std::future::pending(), Some(offer)));
    let want = [at];
    let taken = crate::server::handoff::take(
        &over(&path, Duration::from_secs(3600)),
        &crate::server::handoff::Want {
            s3: &want,
            oci: &[],
            nfs: &[],
        },
    )
    .await
    .unwrap();
    assert!(taken.is_some());
    within("the end of the offer", waiting).await.unwrap();

    let (_other_dir, other) = scratch();
    let untaken = Offer::bind(&other, Vec::new()).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let waiting = tokio::spawn(stopped_by(
        async move {
            let _ = stopped.await;
        },
        Some(untaken),
    ));
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    stop.send(()).unwrap();
    within("the stop", waiting).await.unwrap();
}
