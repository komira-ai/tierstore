//! The running instance's side, spoken to by hand over its control socket.

use std::os::unix::fs::PermissionsExt;

use crate::server::handoff::testing::{ask, connect, listening, scratch, send, socket};
use crate::server::handoff::wire;
use crate::server::handoff::{Front, HandoffError};

use super::Offer;

/// The text of a frame's lines, for a comparison.
fn texts(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

/// Asserts the offer: a `take 1` is answered with `offer`, a line for each socket in the order
/// they were given (the front and the address), and the sockets themselves, and the instance
/// goes on serving after a taker that answers `nak`, and after one that goes away without
/// answering, so that the next taker is offered the same; an `ack` ends the serve, removes the
/// control socket, and is answered `done`.
#[tokio::test]
async fn an_offer_names_its_sockets_and_an_ack_ends_it() {
    let (_dir, path) = scratch();
    let ((s3, a), (oci, b)) = (listening(), listening());
    let sockets = vec![socket(Front::S3, &s3), socket(Front::Oci, &oci)];
    let serving = tokio::spawn(Offer::bind(&path, sockets).unwrap().serve());
    let expected = ["offer".to_owned(), format!("s3 {a}"), format!("oci {b}")];

    let declining = connect(&path).await;
    let offer = ask(&declining, &["take 1"]).await;
    assert_eq!(texts(&offer.lines), texts(&expected));
    assert_eq!(offer.fds.len(), 2);
    send(&declining, &["nak"]).await;

    let leaving = connect(&path).await;
    let offer = ask(&leaving, &["take 1"]).await;
    assert_eq!(texts(&offer.lines), texts(&expected));
    drop(leaving);

    let taking = connect(&path).await;
    let offer = ask(&taking, &["take 1"]).await;
    assert_eq!(texts(&offer.lines), texts(&expected));
    assert!(!serving.is_finished());
    let done = ask(&taking, &["ack"]).await;
    assert_eq!(texts(&done.lines), vec!["done"]);
    serving.await.unwrap();
    assert!(!path.exists());
}

/// Asserts that while one taker holds the offer, another is refused with the reason, in the exact
/// words, and that it is the first that then takes.
#[tokio::test]
async fn a_second_taker_is_refused_while_the_first_is_at_it() {
    let (_dir, path) = scratch();
    let (s3, _) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &s3)])
            .unwrap()
            .serve(),
    );
    let first = connect(&path).await;
    let offer = ask(&first, &["take 1"]).await;
    assert_eq!(offer.lines[0], "offer");
    let second = connect(&path).await;
    let refused = ask(&second, &["take 1"]).await;
    assert_eq!(
        texts(&refused.lines),
        vec!["refused", "another taker is taking the sockets"]
    );
    assert!(refused.fds.is_empty());
    let done = ask(&first, &["ack"]).await;
    assert_eq!(texts(&done.lines), vec!["done"]);
    serving.await.unwrap();
}

/// Asserts that a request that is not `take 1` is refused, in the exact words, and gets no
/// socket.
#[tokio::test]
async fn a_request_that_is_not_a_take_is_refused() {
    let (_dir, path) = scratch();
    let (s3, _) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &s3)])
            .unwrap()
            .serve(),
    );
    for request in ["hello", "take 2"] {
        let client = connect(&path).await;
        let refused = ask(&client, &[request]).await;
        assert_eq!(
            texts(&refused.lines),
            vec!["refused", "the request is not understood"],
            "{request}"
        );
        assert!(refused.fds.is_empty());
    }
    serving.abort();
}

/// Asserts that a taker of another user than the instance's is refused, in the exact words, and
/// gets no socket.
#[tokio::test]
async fn a_taker_of_another_user_is_refused() {
    let (_dir, path) = scratch();
    let (s3, _) = listening();
    let offer = Offer::bind(&path, vec![socket(Front::S3, &s3)]).unwrap();
    let other = rustix::process::geteuid().as_raw().wrapping_add(1);
    let serving = tokio::spawn(offer.of_user(other).serve());
    let client = connect(&path).await;
    let refused = ask(&client, &["take 1"]).await;
    assert_eq!(
        texts(&refused.lines),
        vec!["refused", "the taker is not the user of this instance"]
    );
    assert!(refused.fds.is_empty());
    serving.abort();
}

/// Asserts that the control socket can be used by its user only: mode 0600.
#[tokio::test]
async fn the_control_socket_is_for_its_user_only() {
    let (_dir, path) = scratch();
    let _offer = Offer::bind(&path, Vec::new()).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Asserts that an instance that ends without handing over removes its control socket, and that
/// one that did hand over leaves the path to the one that took it: a file at the path after the
/// serve ended is the successor's.
#[tokio::test]
async fn only_an_instance_that_did_not_hand_over_removes_its_socket() {
    let (_dir, path) = scratch();
    drop(Offer::bind(&path, Vec::new()).unwrap());
    assert!(!path.exists());

    let serving = tokio::spawn(Offer::bind(&path, Vec::new()).unwrap().serve());
    let taker = connect(&path).await;
    ask(&taker, &["take 1"]).await;
    ask(&taker, &["ack"]).await;
    serving.await.unwrap();
    let successor = Offer::bind(&path, Vec::new()).unwrap();
    assert!(path.exists());
    drop(successor);
}

/// Asserts that a file left by an instance that was killed is replaced, and that a path that
/// cannot be cleared (a directory) is an error that says so.
#[tokio::test]
async fn a_file_left_by_a_dead_instance_is_replaced() {
    let (dir, path) = scratch();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let offer = Offer::bind(&path, Vec::new()).unwrap();
    drop(offer);

    let directory = dir.path().join("dir");
    std::fs::create_dir(&directory).unwrap();
    let error = Offer::bind(&directory, Vec::new()).err().unwrap();
    assert!(
        matches!(
            error,
            HandoffError::Io {
                what: "removing the old control socket",
                ..
            }
        ),
        "{error}"
    );
}

/// Asserts that a path whose directory is missing is an error that says the socket could not be
/// bound.
#[tokio::test]
async fn a_control_socket_that_cannot_be_bound_is_an_error() {
    let (dir, _) = scratch();
    let missing = dir.path().join("missing").join("control.sock");
    let error = Offer::bind(&missing, Vec::new()).err().unwrap();
    assert!(
        matches!(
            error,
            HandoffError::Io {
                what: "binding the control socket",
                ..
            }
        ),
        "{error}"
    );
}

/// Asserts that more sockets than a message can carry are refused when the offer is made, saying
/// how many.
#[tokio::test]
async fn more_sockets_than_a_message_can_carry_are_refused() {
    let (_dir, path) = scratch();
    let (s3, _) = listening();
    let sockets = (0..=wire::MAX_FDS)
        .map(|_| socket(Front::S3, &s3))
        .collect();
    let error = Offer::bind(&path, sockets).err().unwrap();
    assert_eq!(
        error.to_string(),
        "hand-off: 254 sockets are more than a message can carry"
    );
}

/// The inode of the file at `path`.
fn inode(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).unwrap().ino()
}

/// Waits until the file at `path` is another one than the inode `was`: the event that the
/// instance has made its control socket again.
async fn remade(path: &std::path::Path, was: u64) {
    use std::os::unix::fs::MetadataExt as _;
    crate::server::handoff::testing::within("the control socket made again", async {
        while std::fs::symlink_metadata(path).map_or(true, |meta| meta.ino() == was) {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// A taker that asks, answers `ack`, and is gone at once: the instance finds no one to tell
/// `done`.
async fn ack_and_go(path: &std::path::Path) {
    let dying = connect(path).await;
    ask(&dying, &["take 1"]).await;
    send(&dying, &["ack"]).await;
}

/// Asserts (T06) the commit point from the running instance's side, for a taker that answers
/// `ack` and is gone before `done` can reach it: the instance does not hand over; it goes on
/// serving and makes its control socket again, a socket of mode 0600, so that a later taker is
/// offered the same sockets and takes them, and that taker's `done` ends the serve. Catches an
/// instance that, having removed its control socket for a taker that died, serves on with no file
/// at the path, so that every later update finds no one to take from and binds into the ports it
/// still holds.
#[tokio::test]
async fn a_taker_gone_before_done_leaves_the_instance_serving_with_its_control_socket() {
    use std::os::unix::fs::FileTypeExt as _;

    let (_dir, path) = scratch();
    let (s3, a) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &s3)])
            .unwrap()
            .serve(),
    );
    let was = inode(&path);
    ack_and_go(&path).await;
    remade(&path, was).await;
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert!(!serving.is_finished());

    let setup = crate::server::handoff::testing::handoff(path.clone());
    let wanted = [a];
    let want = super::super::take::Want {
        s3: &wanted,
        oci: &[],
    };
    let taken = super::super::take::take(&setup, &want)
        .await
        .expect("the later take")
        .expect("an instance answers");
    let addrs: Vec<_> = taken.s3.iter().map(|l| l.local_addr().unwrap()).collect();
    assert_eq!(addrs, vec![a]);
    serving.await.unwrap();
    assert!(!path.exists(), "the later taker is the one that took");
}

/// Asserts (T06) that an instance whose control socket cannot be made again after a taker gone
/// before `done` (its directory is gone) goes on serving its sockets and does not end its serve:
/// it is not handed over, and it says so on stderr rather than draining.
#[tokio::test]
async fn an_instance_that_cannot_make_its_control_socket_again_goes_on_serving() {
    let (dir, path) = scratch();
    let (s3, _) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &s3)])
            .unwrap()
            .serve(),
    );
    let dying = connect(&path).await;
    ask(&dying, &["take 1"]).await;
    // The directory goes before the `ack`: the instance's removal of its file still works on the
    // unlinked name it holds no more, and the new bind finds no directory.
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(dir.path()).unwrap();
    send(&dying, &["ack"]).await;
    drop(dying);
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert!(
        !serving.is_finished(),
        "an instance not handed over does not drain"
    );
    serving.abort();
}

/// Asserts (T06, the decided case after the commit point) that a taker that reads `done` and is
/// then gone, as a successor killed right after the hand-off is, has taken the sockets: the
/// instance's serve ends (its caller drains, as on a stop) and the control socket is not made
/// again, so the successor's restart starts cold.
#[tokio::test]
async fn a_taker_gone_after_done_has_taken_the_sockets() {
    let (_dir, path) = scratch();
    let (s3, _) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &s3)])
            .unwrap()
            .serve(),
    );
    let taker = connect(&path).await;
    ask(&taker, &["take 1"]).await;
    let done = ask(&taker, &["ack"]).await;
    assert_eq!(texts(&done.lines), vec!["done"]);
    drop(taker);
    serving.await.unwrap();
    assert!(!path.exists());
}
