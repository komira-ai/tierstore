//! Taking the sockets of a running instance: the instance is a real [`Offer`] where the test is
//! about the two together, and a fake one that says what the test wants where it is about the
//! taker alone.

use std::net::TcpListener as StdListener;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};

use crate::server::handoff::testing::{
    connect, handoff, listening, pretending, scratch, send, socket,
};
use crate::server::handoff::{Front, Handoff, HandoffError, Offer};

use super::{Want, take};

/// Takes with these addresses named.
async fn taking(
    handoff: &Handoff,
    s3: &[std::net::SocketAddr],
    oci: &[std::net::SocketAddr],
) -> Result<Option<super::Inherited>, HandoffError> {
    take(handoff, &Want { s3, oci, nfs: &[] }).await
}

/// Asserts that with no file at the control socket's path nothing answers, so the caller starts
/// cold: `Ok(None)`.
#[tokio::test]
async fn with_no_file_there_nothing_answers() {
    let (_dir, path) = scratch();
    let taken = taking(&handoff(path), &[], &[]).await.unwrap();
    assert!(taken.is_none());
}

/// Asserts that a file left by an instance that was killed, which no one listens on, is the same
/// as none: `Ok(None)`.
#[tokio::test]
async fn a_file_no_one_listens_on_is_the_same_as_none() {
    let (_dir, path) = scratch();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let taken = taking(&handoff(path), &[], &[]).await.unwrap();
    assert!(taken.is_none());
}

/// Asserts that a path the system cannot connect to for another reason than that no one answers
/// (here, one longer than a socket's path may be) is an error that says what was being done,
/// not a cold start.
#[tokio::test]
async fn a_path_that_cannot_be_connected_to_is_an_error() {
    let (dir, _) = scratch();
    let long = dir.path().join("x".repeat(200));
    let error = taking(&handoff(long), &[], &[]).await.err().unwrap();
    assert!(
        matches!(
            error,
            HandoffError::Io {
                what: "connecting to the control socket",
                ..
            }
        ),
        "{error}"
    );
}

/// Asserts that a running instance that accepts the connection and never answers ends the take
/// with a timeout that names the socket, and does not start the caller cold. The clock is the
/// paused one: the wait ends when every task waits, with no real time passing.
#[tokio::test(start_paused = true)]
async fn an_instance_that_never_answers_ends_in_a_timeout() {
    let (_dir, path) = scratch();
    let silent = tokio::net::UnixListener::bind(&path).unwrap();
    let wait = Handoff {
        socket: path.clone(),
        wait: Duration::from_secs(5),
    };
    let error = taking(&wait, &[], &[]).await.err().unwrap();
    assert_eq!(
        error.to_string(),
        format!(
            "hand-off: no end of the hand-off at {} in time",
            path.display()
        )
    );
    drop(silent);
}

/// Asserts the whole take: the sockets arrive in the order the instance serves them, the S3
/// ones as S3 and the OCI one as OCI, each is the very socket (a connection to its address is
/// accepted by the taken one, once the instance has let go), and the instance is told to stop by
/// the end of its `serve`.
#[tokio::test]
async fn the_sockets_arrive_in_order_and_are_the_same_sockets() {
    let (_dir, path) = scratch();
    let ((s3_a, a), (s3_b, b), (oci, c)) = (listening(), listening(), listening());
    let sockets = vec![
        socket(Front::S3, &s3_a),
        socket(Front::Oci, &oci),
        socket(Front::S3, &s3_b),
    ];
    drop((s3_a, s3_b, oci));
    let offer = Offer::bind(&path, sockets).unwrap();
    let serving = tokio::spawn(offer.serve());
    let taken = taking(&handoff(path.clone()), &[a, b], &[c])
        .await
        .unwrap()
        .unwrap();
    serving.await.unwrap();
    assert!(!path.exists(), "the instance gives up the control socket");
    let addrs = |listeners: &[TcpListener]| -> Vec<_> {
        listeners.iter().map(|l| l.local_addr().unwrap()).collect()
    };
    assert_eq!(addrs(&taken.s3), vec![a, b]);
    assert_eq!(addrs(&taken.oci), vec![c]);
    for (listener, at) in taken.s3.iter().zip([a, b]).chain(taken.oci.iter().zip([c])) {
        let (dialed, accepted) = tokio::join!(TcpStream::connect(at), listener.accept());
        dialed.unwrap();
        accepted.unwrap();
    }
}

/// Asserts that a port of 0 in the settings takes the socket at any port of the same host, since
/// the system picks it, while it takes no other host.
#[tokio::test]
async fn a_port_of_zero_in_the_settings_covers_any_port_of_the_host() {
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let offer = Offer::bind(&path, vec![socket(Front::S3, &listener)]).unwrap();
    let serving = tokio::spawn(offer.serve());
    let any: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let other: std::net::SocketAddr = "127.0.0.2:0".parse().unwrap();
    let wrong = taking(&handoff(path.clone()), &[other], &[])
        .await
        .err()
        .unwrap();
    assert!(matches!(wrong, HandoffError::Mismatch { .. }), "{wrong}");
    let taken = taking(&handoff(path), &[any], &[]).await.unwrap().unwrap();
    serving.await.unwrap();
    assert_eq!(taken.s3[0].local_addr().unwrap(), addr);
}

/// Asserts that settings that name other addresses than the instance serves do not take its
/// sockets: the error says both sets, in the exact words, the instance is told `nak` and goes on
/// (a taker with the right addresses then takes), and a front the instance has and the settings
/// lack is a mismatch too.
#[tokio::test]
async fn settings_that_name_other_addresses_take_nothing() {
    let (_dir, path) = scratch();
    let ((s3, a), (oci, b)) = (listening(), listening());
    let sockets = vec![socket(Front::S3, &s3), socket(Front::Oci, &oci)];
    let serving = tokio::spawn(Offer::bind(&path, sockets).unwrap().serve());
    let elsewhere: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
    let error = taking(&handoff(path.clone()), &[elsewhere], &[b])
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        format!(
            "hand-off: the running instance listens on [\"s3 {a}\", \"oci {b}\"], \
             these settings name [\"s3 127.0.0.1:1\", \"oci {b}\"]"
        )
    );
    let no_oci = taking(&handoff(path.clone()), &[a], &[])
        .await
        .err()
        .unwrap();
    assert!(matches!(no_oci, HandoffError::Mismatch { .. }), "{no_oci}");
    assert!(
        !serving.is_finished(),
        "a refused offer leaves the instance serving"
    );
    let taken = taking(&handoff(path), &[a], &[b]).await.unwrap();
    assert!(taken.is_some());
    serving.await.unwrap();
}

/// Asserts that a taker finding another taker at it is refused with the instance's reason, as an
/// error of the kind `Refused`, and starts nothing.
#[tokio::test]
async fn a_second_taker_is_refused_with_the_reason() {
    let (_dir, path) = scratch();
    let (listener, a) = listening();
    let offer = Offer::bind(&path, vec![socket(Front::S3, &listener)]).unwrap();
    let serving = tokio::spawn(offer.serve());
    let first = connect(&path).await;
    send(&first, &["take 1"]).await;
    let _offer = crate::server::handoff::wire::recv(&first).await.unwrap();
    let error = taking(&handoff(path), &[a], &[]).await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "hand-off: the running instance refused: another taker is taking the sockets"
    );
    serving.abort();
}

/// An answer of a fake instance: its lines, how many sockets it sends, the lines it ends with
/// after an `ack` (if it reads one), and the message the taker's error is to carry.
type Case = (
    Vec<&'static str>,
    usize,
    Option<Vec<&'static str>>,
    &'static str,
);

/// Asserts that each answer that is not the protocol is refused with its own message: a frame
/// that is not an offer, a number of lines that is not the number of sockets, a socket line
/// that is not `<front> <address>`, an address that is not one, and an end that is not `done`.
#[tokio::test]
async fn an_answer_that_is_not_the_protocol_is_refused() {
    let (extra, _) = listening();
    let cases: Vec<Case> = vec![
        (vec!["hello"], 0, None, "a frame that is not an offer"),
        (
            vec!["offer", "s3 127.0.0.1:1"],
            0,
            None,
            "another number of sockets than lines",
        ),
        (
            vec!["offer", "nonsense"],
            1,
            None,
            "a socket line that is not <front> <address>",
        ),
        (
            vec!["offer", "s3 not-an-address"],
            1,
            None,
            "a socket line that is not <front> <address>",
        ),
        (
            vec!["offer", "s3 127.0.0.1:1"],
            1,
            Some(vec!["nope"]),
            "something other than done after ack",
        ),
    ];
    for (lines, sockets, then, message) in cases {
        let (_dir, path) = scratch();
        let fds: Vec<StdListener> = (0..sockets).map(|_| extra.try_clone().unwrap()).collect();
        let lines = lines.into_iter().map(str::to_owned).collect();
        let then = then.map(|t| t.into_iter().map(str::to_owned).collect());
        let fake = pretending(&path, lines, fds, then);
        let want: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        let error = taking(&handoff(path), &[want], &[]).await.err().unwrap();
        assert_eq!(
            error.to_string(),
            format!("hand-off: the running instance sent {message}")
        );
        fake.await.unwrap();
    }
}

/// A copy that the system refuses, as a process out of descriptors (EMFILE) does.
fn no_copy(_: &TcpListener) -> std::io::Result<StdListener> {
    Err(std::io::Error::from_raw_os_error(24))
}

/// Asserts (T05) that a taker that cannot make the copies of the sockets for its own offer (the
/// process is out of descriptors) answers `nak` before the running instance's `done`: the take is
/// an error saying what was being done, the running instance goes on serving with its control
/// socket, and a later taker that can copy takes the sockets. Catches a taker that fails to copy
/// only after the running instance has begun its drain.
#[tokio::test]
async fn a_taker_that_cannot_copy_the_sockets_refuses_before_the_running_instance_drains() {
    let (_dir, path) = scratch();
    let (s3, a) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &s3)])
            .unwrap()
            .serve(),
    );
    let want = Want {
        s3: &[a],
        oci: &[],
        nfs: &[],
    };
    let error = super::exchange(&path, &want, no_copy).await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "hand-off: taking over a socket: Too many open files (os error 24)"
    );
    assert!(
        path.exists(),
        "the running instance keeps its control socket"
    );
    assert!(!serving.is_finished());
    let taken = taking(&handoff(path.clone()), &[a], &[]).await.unwrap();
    assert!(taken.is_some());
    serving.await.unwrap();
}

/// Asserts that the sockets taken come with their addresses and a copy of each, made before the
/// `ack`: the addresses are the offered ones, by front, and each copy is the same socket as its
/// original, offered under the same front.
#[tokio::test]
async fn the_taken_sockets_come_with_their_addresses_and_copies() {
    let (_dir, path) = scratch();
    let ((s3, a), (oci, b)) = (listening(), listening());
    let sockets = vec![socket(Front::S3, &s3), socket(Front::Oci, &oci)];
    drop((s3, oci));
    let serving = tokio::spawn(Offer::bind(&path, sockets).unwrap().serve());
    let taken = taking(&handoff(path.clone()), &[a], &[b])
        .await
        .unwrap()
        .unwrap();
    serving.await.unwrap();
    assert_eq!(taken.s3_addrs, vec![a]);
    assert_eq!(taken.oci_addrs, vec![b]);
    let copies: Vec<_> = taken
        .copies
        .iter()
        .map(|copy| (copy.front(), copy.local_addr().unwrap()))
        .collect();
    assert_eq!(copies, vec![(Front::S3, a), (Front::Oci, b)]);
}

/// Asserts (T10, in-process) that every socket taken over the control socket is close-on-exec,
/// though the kernel hands descriptors over without the flag: a program the new instance starts
/// does not hold its listening sockets. Catches a taker that leaves the flag as it came.
#[tokio::test]
async fn the_taken_sockets_are_close_on_exec() {
    use rustix::io::{FdFlags, fcntl_getfd};

    let (_dir, path) = scratch();
    let ((s3, a), (oci, b)) = (listening(), listening());
    let sockets = vec![socket(Front::S3, &s3), socket(Front::Oci, &oci)];
    let serving = tokio::spawn(Offer::bind(&path, sockets).unwrap().serve());
    let taken = taking(&handoff(path), &[a], &[b]).await.unwrap().unwrap();
    serving.await.unwrap();
    for listener in taken.s3.iter().chain(&taken.oci) {
        let flags = fcntl_getfd(listener).unwrap();
        assert!(flags.contains(FdFlags::CLOEXEC), "{flags:?}");
    }
}

/// Asserts that a line that names a front this release does not know (one added after it, as `nfs`
/// was) is skipped with its descriptor closed, and the sockets of the fronts it knows are taken: an
/// older release that is offered a newer front's socket does not fail the update.
#[tokio::test]
async fn an_old_release_ignores_the_new_front_kind_line() {
    let (_dir, path) = scratch();
    let ((s3, a), (later, b)) = (listening(), listening());
    let lines = vec!["offer".to_owned(), format!("later {b}"), format!("s3 {a}")];
    let fake = pretending(&path, lines, vec![later, s3], Some(vec!["done".to_owned()]));
    let taken = taking(&handoff(path), &[a], &[]).await.unwrap().unwrap();
    fake.await.unwrap();
    assert_eq!(taken.s3_addrs, vec![a]);
    assert_eq!(taken.s3.len(), 1);
    assert!(taken.oci.is_empty() && taken.nfs.is_empty());
    assert_eq!(taken.copies.len(), 1);
}

/// Asserts that the NFS front's socket is taken with its address and a copy offered under the same
/// front, and that a running instance that listens on another NFS address than the taker names is
/// refused with both lists in the message, without the sockets being taken.
#[tokio::test]
async fn the_nfs_front_is_taken_by_its_address_and_a_mismatch_is_refused() {
    let (_dir, path) = scratch();
    let (nfs, a) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::Nfs, &nfs)])
            .unwrap()
            .serve(),
    );
    let elsewhere: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
    let error = take(
        &handoff(path.clone()),
        &Want {
            s3: &[],
            oci: &[],
            nfs: &[elsewhere],
        },
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        error.to_string(),
        format!(
            "hand-off: the running instance listens on [\"nfs {a}\"], these settings name [\"nfs {elsewhere}\"]"
        )
    );
    assert!(!serving.is_finished());
    let taken = take(
        &handoff(path),
        &Want {
            s3: &[],
            oci: &[],
            nfs: &[a],
        },
    )
    .await
    .unwrap()
    .unwrap();
    serving.await.unwrap();
    assert_eq!(taken.nfs_addrs, vec![a]);
    assert_eq!(taken.nfs.len(), 1);
    let copies: Vec<_> = taken
        .copies
        .iter()
        .map(|c| (c.front(), c.local_addr().unwrap()))
        .collect();
    assert_eq!(copies, vec![(Front::Nfs, a)]);
}
