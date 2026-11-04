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

/// Asserts that the pause after a failed accept is the default when `with_accept_pause` is not
/// called, and the one given when it is.
#[tokio::test]
async fn the_accept_pause_is_the_default_until_one_is_given() {
    let (_dir, path) = scratch();
    let offer = Offer::bind(&path, Vec::new()).unwrap();
    assert_eq!(offer.accept_pause, crate::server::ACCEPT_PAUSE);
    let pause = std::time::Duration::from_millis(321);
    assert_ne!(pause, crate::server::ACCEPT_PAUSE);
    let offer = offer.with_accept_pause(pause);
    assert_eq!(offer.accept_pause, pause);
}
