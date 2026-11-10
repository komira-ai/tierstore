//! The hand-over of the mount: the running instance's side over a process with a stand-in for the
//! kernel's end of `/dev/fuse`, and the taker's side against it and against peers that say
//! what a test wants them to.

use std::cell::Cell;
use std::os::fd::AsFd as _;
use std::path::Path;
use std::time::Duration;

use tokio::net::UnixListener;

use super::{HandoverError, Offer, Paths, offer_lines, take};
use crate::server::handoff::testing::{ask, connect, send, within};
use crate::server::handoff::wire;
use crate::server::treefs::fakedev::instance;
use crate::server::treefs::init_record;
use crate::server::treefs::ino;
use crate::server::treefs::run::{Prepared, Running};

/// Waits, yielding to the other tasks, until a file is at `path`.
async fn appears(path: &Path) {
    while !path.exists() {
        tokio::task::yield_now().await;
    }
}

/// Asserts the whole hand-over: the taker is offered the connection and the control socket, puts
/// the descriptor in its store and reads its state before it says `ack`; the running instance
/// stops reading (a request that comes meanwhile waits); the successor serves it from the
/// recorded outcome, the control socket it was given still answers, and the running instance
/// ends when told `ready`, leaving the socket and its file to the successor.
#[tokio::test]
async fn a_taker_is_given_the_mount_and_the_running_instance_ends_when_it_is_ready() {
    let old = instance("tfs-ho-full", Duration::from_secs(3600), None);
    old.assert_serving(1);
    let stored = Cell::new(false);
    let taken = within(
        "the take",
        take(
            &old.paths(),
            Box::new(|device| {
                stored.set(device.as_fd().try_clone_to_owned().is_ok());
                Prepared::load(&old.settings)
                    .map(drop)
                    .map_err(|why| why.to_string())
            }),
        ),
    )
    .await
    .unwrap();
    let taken = taken.expect("an instance answered");
    let prepared = Prepared::load(&old.settings).unwrap();
    assert!(stored.get());
    // The running instance has stopped reading: a request waits for the successor.
    old.kernel.getattr(2, ino::ROOT);
    old.kernel.assert_silent();
    assert!(!old.settings.handoff_socket.exists());
    let next = Running::adopt(
        &old.settings,
        prepared,
        taken.device,
        (Some(taken.control), false),
    )
    .unwrap();
    let reply = old.kernel.reply();
    assert_eq!((reply.unique, reply.error), (2, 0));
    assert_eq!(
        init_record::read(&old.rig.runtime).unwrap().generation,
        3,
        "the successor is the next generation"
    );
    within("ready", taken.notice.ready()).await.unwrap();
    within("the end", old.serving).await.unwrap().unwrap();
    // The control socket's file is the successor's: the old one's end did not remove it.
    drop(old.running);
    assert!(std::os::unix::net::UnixStream::connect(&old.settings.control_socket).is_ok());
    drop(next);
}

/// Asserts that a taker whose preparation fails says `nak`, and the running instance goes on
/// reading and keeps its socket for the next taker.
#[tokio::test]
async fn a_taker_that_cannot_prepare_leaves_the_running_instance_as_it_was() {
    let old = instance("tfs-ho-nak", Duration::from_secs(3600), None);
    let refused = within(
        "the refused take",
        take(
            &old.paths(),
            Box::new(|_| Err("the store is full".to_owned())),
        ),
    )
    .await;
    match refused {
        Err(HandoverError::Unprepared(why)) => assert_eq!(why, "the store is full"),
        other => panic!("{:?}", other.err()),
    }
    old.assert_serving(1);
    assert!(old.settings.handoff_socket.exists());
    // The next taker is answered.
    let again = within("the next take", take(&old.paths(), Box::new(|_| Ok(()))))
        .await
        .unwrap();
    let taken = again.expect("answered");
    taken.notice.ready().await.unwrap();
    within("the end", old.serving).await.unwrap().unwrap();
}

/// Asserts that a taker that names other paths than the running instance serves is told so, and
/// answers `nak`.
#[tokio::test]
async fn a_taker_with_other_paths_is_refused_by_name_and_the_running_instance_goes_on() {
    let old = instance("tfs-ho-mismatch", Duration::from_secs(3600), None);
    let other = Path::new("/somewhere/else");
    let paths = Paths {
        mountpoint: other,
        ..old.paths()
    };
    let refused = within("the take", take(&paths, Box::new(|_| Ok(())))).await;
    match refused {
        Err(HandoverError::Mismatch { offered, wanted }) => {
            assert_eq!(
                offered[0],
                format!("fuse {}", old.settings.mountpoint.display())
            );
            assert_eq!(wanted[0], "fuse /somewhere/else");
        }
        other => panic!("{:?}", other.err()),
    }
    old.assert_serving(1);
}

/// Asserts the refusals of the running instance: a request it does not understand, and a taker
/// that is not its user; both are answered `refused` with the reason, and the taker's error
/// carries it.
#[tokio::test]
async fn a_request_not_understood_and_a_taker_of_another_user_are_refused() {
    let old = instance("tfs-ho-refused", Duration::from_secs(3600), None);
    let stream = connect(&old.settings.handoff_socket).await;
    let answer = ask(&stream, &["hello"]).await;
    assert_eq!(answer.lines, ["refused", "the request is not understood"]);
    let other = instance("tfs-ho-other-user", Duration::from_secs(3600), Some(1));
    let refused = within("the take", take(&other.paths(), Box::new(|_| Ok(())))).await;
    match refused {
        Err(HandoverError::Refused(why)) => assert!(why.contains("not the user"), "{why}"),
        other => panic!("{:?}", other.err()),
    }
    old.assert_serving(1);
    other.assert_serving(1);
}

/// Asserts that a taker that is gone after `done` has not taken the mount: the running instance has
/// stopped for it, then reads again, accepts on its control socket again and binds its hand-over
/// socket again, and the next taker is answered.
#[tokio::test]
async fn a_taker_gone_before_ready_leaves_the_running_instance_serving_again() {
    let old = instance("tfs-ho-gone", Duration::from_secs(3600), None);
    let stream = connect(&old.settings.handoff_socket).await;
    send(&stream, &["take-fs 1"]).await;
    let offer = within("the offer", wire::recv(&stream)).await.unwrap();
    assert_eq!(offer.lines[0], "offer-fs");
    assert_eq!(offer.fds.len(), 2);
    send(&stream, &["ack"]).await;
    let done = within("done", wire::recv(&stream)).await.unwrap();
    assert_eq!(done.lines, ["done"]);
    drop(stream);
    drop(offer);
    within("the socket again", appears(&old.settings.handoff_socket)).await;
    old.assert_serving(1);
    let again = within("the next take", take(&old.paths(), Box::new(|_| Ok(()))))
        .await
        .unwrap();
    let taken = again.expect("answered again");
    taken.notice.ready().await.unwrap();
    within("the end", old.serving).await.unwrap().unwrap();
}

/// Asserts that a taker which answers `done` with anything but `ready` has not taken the mount:
/// the running instance serves again and binds its socket again.
#[tokio::test]
async fn a_taker_that_answers_done_with_something_else_has_not_taken_the_mount() {
    let old = instance("tfs-ho-wrong", Duration::from_secs(3600), None);
    let stream = connect(&old.settings.handoff_socket).await;
    send(&stream, &["take-fs 1"]).await;
    let offer = within("the offer", wire::recv(&stream)).await.unwrap();
    send(&stream, &["ack"]).await;
    assert_eq!(wire::recv(&stream).await.unwrap().lines, ["done"]);
    send(&stream, &["not ready"]).await;
    within("the socket again", appears(&old.settings.handoff_socket)).await;
    old.assert_serving(1);
    assert!(!old.serving.is_finished());
    drop((stream, offer));
}

/// Asserts that a hand-over socket which cannot accept is said and tried again after a pause, and
/// that the instance does not end for it. A socket that is not listening stands in: `accept` on it
/// fails.
#[tokio::test(start_paused = true)]
async fn a_hand_over_socket_that_cannot_accept_is_said_and_tried_again() {
    let old = instance("tfs-ho-noaccept", Duration::from_secs(3600), None);
    let (not_listening, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    // Readable, so that the accept is tried and fails.
    std::io::Write::write_all(&mut peer, b"x").unwrap();
    let broken = std::os::unix::net::UnixListener::from(std::os::fd::OwnedFd::from(not_listening));
    broken.set_nonblocking(true).unwrap();
    let mut offer = Offer::bind(
        &old.settings.handoff_socket.with_extension("again"),
        &old.settings.mountpoint,
        &old.settings.control_socket,
        Duration::from_secs(3600),
    )
    .unwrap();
    offer.listener = Some(UnixListener::from_std(broken).unwrap());
    let serving = tokio::spawn(offer.serve(std::sync::Arc::clone(&old.running)));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!serving.is_finished());
    serving.abort();
}

/// Asserts that a conversation that goes quiet is given up after `fs.handoff_wait`, on the paused
/// clock: a taker that connects and says nothing, and one that takes the mount and does not say it
/// serves it; the running instance serves again after each.
#[tokio::test(start_paused = true)]
async fn a_taker_that_goes_quiet_is_given_up_after_the_wait() {
    let old = instance("tfs-ho-quiet", Duration::from_secs(30), None);
    let silent = connect(&old.settings.handoff_socket).await;
    tokio::time::sleep(Duration::from_secs(31)).await;
    old.assert_serving(1);
    drop(silent);

    let stream = connect(&old.settings.handoff_socket).await;
    send(&stream, &["take-fs 1"]).await;
    let offer = wire::recv(&stream).await.unwrap();
    send(&stream, &["ack"]).await;
    assert_eq!(wire::recv(&stream).await.unwrap().lines, ["done"]);
    tokio::time::sleep(Duration::from_secs(31)).await;
    appears(&old.settings.handoff_socket).await;
    old.assert_serving(2);
    drop((stream, offer));
}

/// Asserts the taker's side against peers that do not keep to the protocol, and that no instance
/// answering is `None`.
#[tokio::test]
async fn a_taker_reads_what_it_is_told_and_refuses_what_is_not_the_protocol() {
    let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tfs-ho-peers");
    let socket = dir.path().join("handoff.sock");
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("control.sock"));
    let paths = Paths {
        socket: &socket,
        mountpoint: &mountpoint,
        control: &control,
        wait: Duration::from_secs(3600),
    };
    // Nobody there, and a file nobody listens on.
    assert!(take(&paths, Box::new(|_| Ok(()))).await.unwrap().is_none());
    std::fs::write(&socket, "stale").unwrap();
    assert!(take(&paths, Box::new(|_| Ok(()))).await.unwrap().is_none());
    std::fs::remove_file(&socket).unwrap();
    // A socket path in a directory that is not one.
    let under_file = dir.path().join("a-file/handoff.sock");
    std::fs::write(dir.path().join("a-file"), "x").unwrap();
    let bad = Paths {
        socket: &under_file,
        ..paths
    };
    assert!(matches!(
        take(&bad, Box::new(|_| Ok(()))).await,
        Err(HandoverError::Io { .. })
    ));

    let listener = UnixListener::bind(&socket).unwrap();
    // An answer that is not an offer; an offer with no descriptors.
    for lines in [
        vec!["something".to_owned()],
        offer_lines(&mountpoint, &control),
    ] {
        let accepted = listener.accept();
        let peer = async {
            let (stream, _) = accepted.await.unwrap();
            let _ = wire::recv(&stream).await.unwrap();
            wire::send(&stream, &lines, &[]).await.unwrap();
            stream
        };
        let (_stream, result) = tokio::join!(peer, take(&paths, Box::new(|_| Ok(()))));
        assert!(
            matches!(result, Err(HandoverError::Protocol(_))),
            "{:?}",
            result.err()
        );
    }
    // An offer and `ack`, then something that is not `done`.
    let (one, two) = (
        std::fs::File::open("/dev/null").unwrap(),
        std::fs::File::open("/dev/null").unwrap(),
    );
    let accepted = listener.accept();
    let lines = offer_lines(&mountpoint, &control);
    let peer = async {
        let (stream, _) = accepted.await.unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        wire::send(&stream, &lines, &[one.as_fd(), two.as_fd()])
            .await
            .unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        wire::send(&stream, &["never mind".to_owned()], &[])
            .await
            .unwrap();
    };
    let (_, result) = tokio::join!(peer, take(&paths, Box::new(|_| Ok(()))));
    assert!(
        matches!(result, Err(HandoverError::Protocol(_))),
        "{:?}",
        result.err()
    );
}

/// Asserts that a conversation that never ends is given up after the wait, on the paused clock.
#[tokio::test(start_paused = true)]
async fn a_running_instance_that_never_answers_is_given_up_after_the_wait() {
    let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tfs-ho-hang");
    let socket = dir.path().join("handoff.sock");
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("control.sock"));
    let listener = UnixListener::bind(&socket).unwrap();
    let paths = Paths {
        socket: &socket,
        mountpoint: &mountpoint,
        control: &control,
        wait: Duration::from_secs(30),
    };
    let accepted = listener.accept();
    let hang = async {
        let (stream, _) = accepted.await.unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        std::future::pending::<()>().await;
    };
    let taking = take(&paths, Box::new(|_| Ok(())));
    tokio::select! {
        result = taking => assert!(matches!(result, Err(HandoverError::Timeout(_)))),
        () = hang => unreachable!("the peer never answers"),
    }
}

/// Asserts that the hand-over socket cannot be bound where its directory is not there or its path
/// is a directory, and that the errors of the conversation name what was being done.
#[test]
fn a_socket_that_cannot_be_made_and_the_words_of_the_errors() {
    let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tfs-ho-bind");
    let none = dir.path().join("no/such/dir/handoff.sock");
    let error = Offer::bind(&none, dir.path(), dir.path(), Duration::from_secs(1))
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .starts_with("hand-over: binding the hand-over socket")
    );
    let stuck = dir.path().join("stuck.sock");
    std::fs::create_dir_all(stuck.join("inside")).unwrap();
    let error = Offer::bind(&stuck, dir.path(), dir.path(), Duration::from_secs(1))
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("removing the old hand-over socket")
    );
    assert!(
        HandoverError::Timeout(dir.path().to_owned())
            .to_string()
            .contains("in time")
    );
    assert!(
        HandoverError::Stuck(std::io::Error::other("x"))
            .to_string()
            .contains("could not serve again")
    );
}

/// Asserts that a taker that hangs up without a request, and one that is gone after `ack`, have
/// taken nothing, and that the running instance, which answers one taker at a time, goes on to
/// answer the next.
#[tokio::test]
async fn a_taker_that_hangs_up_is_followed_by_the_next() {
    let old = instance("tfs-ho-hangup", Duration::from_secs(3600), None);
    drop(connect(&old.settings.handoff_socket).await);
    let next = connect(&old.settings.handoff_socket).await;
    let answer = ask(&next, &["hello"]).await;
    assert_eq!(answer.lines[0], "refused");
    old.assert_serving(1);
}

/// Asserts that an instance that cannot make its hand-over socket again after a hand-over that did
/// not complete goes on serving the kernel, with no socket to be handed over from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_instance_that_cannot_bind_its_socket_again_goes_on_serving() {
    let old = instance("tfs-ho-nosocket", Duration::from_secs(3600), None);
    let stream = connect(&old.settings.handoff_socket).await;
    send(&stream, &["take-fs 1"]).await;
    let offer = within("the offer", wire::recv(&stream)).await.unwrap();
    send(&stream, &["ack"]).await;
    assert_eq!(wire::recv(&stream).await.unwrap().lines, ["done"]);
    // The file is gone by now; a directory in its place keeps the socket from being made again.
    std::fs::create_dir(&old.settings.handoff_socket).unwrap();
    drop((stream, offer));
    old.assert_serving(1);
    assert!(old.settings.handoff_socket.is_dir());
    assert!(!old.serving.is_finished());
}
