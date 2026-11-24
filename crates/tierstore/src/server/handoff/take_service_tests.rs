//! The one hand-over from the taker's side: the request for the mount and the fall back without
//! it, the whole offer checked before `ack` (the running instance's user, every path, every
//! descriptor's kind), and the mount, the control socket and the lock taken together with the
//! sockets.

use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UnixListener;

use crate::server::handoff::testing::{
    ask, connect, fake_mount, handoff, identity, listening, scratch, settle, socket, texts, told,
    within,
};
use crate::server::handoff::{Front, HandoffError, Offer, wire};
use crate::server::local_cache::Mode;
use crate::server::local_cache::testing::Rig;
use crate::server::lock::try_take;

use super::super::mount::{MountWant, Prepare};
use super::super::offer::{HOLDS_MOUNT, NO_MOUNT};
use super::{Ask, Inherited, Knobs, Request, Want, take, take_as, take_service};

/// The wait of the offer's `ready` step and of the mount's quiesce: only a hung test reaches it.
const WAIT: Duration = Duration::from_secs(3600);

/// An offer a taker refuses: its lines and descriptors, whether the taker's `prepare` succeeds,
/// the error of the take, and the answer the running instance hears, if any.
type Refused<'a> = (
    Vec<String>,
    Vec<BorrowedFd<'a>>,
    bool,
    String,
    Option<&'static str>,
);

/// The devices a `prepare` was given, each as its device and inode.
type Noted = Arc<Mutex<Vec<(u64, u64)>>>;

/// The mount a taker asks for at `mountpoint` and `control`, with a check of the device that
/// takes the fake device of [`fake_mount`], and a `prepare` that notes each device it is given.
fn want_mount<'a>(mountpoint: &'a Path, control: &'a Path) -> (MountWant<'a>, Noted) {
    let prepared = Arc::new(Mutex::new(Vec::new()));
    let noted = Arc::clone(&prepared);
    let prepare: Prepare<'a> = Box::new(move |device: &OwnedFd| {
        noted.lock().unwrap().push(identity(device.as_fd()));
        Ok(())
    });
    let want = MountWant {
        mountpoint,
        control,
        wait: WAIT,
        device: |_| Ok(()),
        prepare,
    };
    (want, prepared)
}

/// The settings' addresses: the one S3 socket at `addr`.
fn s3(addr: &[std::net::SocketAddr]) -> Want<'_> {
    Want {
        s3: addr,
        oci: &[],
        nfs: &[],
    }
}

/// Asserts the whole hand-over of an instance that holds the mount and a cache: the offer is one
/// frame with the S3 socket's line, then `fuse`, `control` and `lock`, and their four descriptors;
/// the taker gets the very socket, device, control socket and lock file, `prepare` ran with the
/// device before `ack`, the mount was quiesced at `ack` and given up only at `ready`, and the old
/// cache is read-only from `ready` on. It would catch an offer that splits the hand-over, a taker
/// that takes a part of it, or an old instance that gives the mount up before the successor said
/// it serves.
#[tokio::test]
async fn an_offer_with_the_mount_hands_sockets_mount_control_and_lock_together() {
    let rig = Rig::new("handoff-service-whole");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (dir, path) = scratch();
    let (mount, steps, _kernel) = fake_mount(dir.path());
    let device = identity((mount.share)().unwrap().0.as_fd());
    let (listener, addr) = listening();
    let offer = Offer::bind(&path, vec![socket(Front::S3, &listener)])
        .unwrap()
        .with_ready_wait(WAIT)
        .with_mount(mount)
        .with_lock(old.clone());
    let serving = tokio::spawn(offer.serve());

    // The frame, as a taker of this release reads it, before it answers.
    let raw = connect(&path).await;
    let offered = ask(&raw, &["take 1", "ready", "mount"]).await;
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("fs-control.sock"));
    assert_eq!(
        texts(&offered.lines),
        [
            "offer".to_owned(),
            format!("s3 {addr}"),
            format!("fuse {}", mountpoint.display()),
            format!("control {}", control.display()),
            format!("lock {}", lock.display()),
        ]
    );
    assert_eq!(offered.fds.len(), 4);
    crate::server::handoff::testing::send(&raw, &["nak"]).await;
    drop(raw);

    let addrs = [addr];
    let (want, prepared) = want_mount(&mountpoint, &control);
    let ask = Ask {
        mount: Some(want),
        lock: Some(&lock),
    };
    let taken = within(
        "the take",
        take_service(&handoff(path.clone()), &s3(&addrs), ask),
    )
    .await
    .unwrap()
    .unwrap();
    let Inherited {
        s3: sockets,
        mount,
        lock: received,
        ready,
        ..
    } = taken;
    assert_eq!(identity(sockets[0].as_fd()), identity(listener.as_fd()));
    let mount = mount.expect("the mount");
    assert_eq!(identity(mount.device.as_fd()), device);
    let offered_control = std::os::unix::net::UnixListener::bind(dir.path().join("probe.sock"));
    drop(offered_control);
    assert_eq!(
        mount.control.local_addr().unwrap().as_pathname(),
        Some(control.as_path()),
        "the control socket listening at its path"
    );
    assert_eq!(*prepared.lock().unwrap(), [device], "prepared before ack");
    assert_eq!(told(&steps), ["quiesce"], "quiesced at ack");
    assert!(received.expect("the lock").is_current());
    assert_eq!(old.mode(), Mode::ReadWrite, "the old writes until ready");
    settle().await;
    assert!(!serving.is_finished(), "nothing handed before ready");
    let predecessor = within("bye", ready.expect("the step").send_keeping())
        .await
        .unwrap();
    within("the end of the offer", serving).await.unwrap();
    assert_eq!(told(&steps), ["quiesce", "give up"]);
    assert_eq!(old.mode(), Mode::ReadOnly, "no write from ready on");
    assert!(try_take(&lock).unwrap().is_none());
    drop(predecessor);
}

/// Asserts that a lock line whose descriptor is of another file than the taker's lock path, or
/// that names another path, is refused before `ack`: the take is an error that says so, the fake
/// running instance is told `nak`, and nothing was prepared. It would catch a successor that
/// takes the write role with a lock of a file the other process does not hold.
#[tokio::test]
async fn a_lock_descriptor_of_another_file_is_refused_before_ack() {
    let (dir, path) = scratch();
    let lock = dir.path().join("cache.lock");
    std::fs::write(&lock, b"").unwrap();
    let other = dir.path().join("other.lock");
    std::fs::write(&other, b"").unwrap();
    let (listener, addr) = listening();
    let cases = [
        (
            format!("lock {}", lock.display()),
            other.clone(),
            format!(
                "hand-off: checking the offered lock: the lock descriptor is not the file at {}",
                lock.display()
            ),
        ),
        (
            format!("lock {}", other.display()),
            other.clone(),
            format!(
                "hand-off: the running instance listens on [\"lock {}\"], these settings name \
             [\"lock {}\"]",
                other.display(),
                lock.display()
            ),
        ),
    ];
    for (line, file, error) in cases {
        let file = std::fs::File::open(&file).unwrap();
        let lines = vec!["offer".to_owned(), format!("s3 {addr}"), line];
        let answer = fake(&path, lines, vec![listener.as_fd(), file.as_fd()]);
        let addrs = [addr];
        let ask = Ask {
            mount: None,
            lock: Some(&lock),
        };
        let taken = take_service(&handoff(path.clone()), &s3(&addrs), ask).await;
        assert_eq!(taken.err().unwrap().to_string(), error);
        assert_eq!(within("the answer", answer).await.unwrap(), ["nak"]);
    }
}

/// A fake running instance at `path`, for one taker: it answers the request with `lines` and
/// `fds`, and returns the taker's answer.
fn fake(
    path: &Path,
    lines: Vec<String>,
    fds: Vec<BorrowedFd<'_>>,
) -> tokio::task::JoinHandle<Vec<String>> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).unwrap();
    let owned: Vec<OwnedFd> = fds
        .iter()
        .map(|fd| fd.try_clone_to_owned().unwrap())
        .collect();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        let borrowed: Vec<BorrowedFd<'_>> = owned.iter().map(|fd| fd.as_fd()).collect();
        wire::send(&stream, &lines, &borrowed).await.unwrap();
        wire::recv(&stream)
            .await
            .map(|frame| frame.lines)
            .unwrap_or_default()
    })
}

/// Asserts that each descriptor of an offer is checked for its kind before `ack`: a socket line
/// whose descriptor does not listen, a device the device check refuses, a control socket that
/// does not listen, and a lock that is a directory are each refused with words that name it, the
/// running instance is told `nak`, and `prepare` never ran. It would catch a successor that
/// stores a descriptor of the wrong kind as its mount, or serves on a socket that accepts nothing.
#[tokio::test]
async fn a_descriptor_of_the_wrong_kind_is_refused_before_ack() {
    let (dir, path) = scratch();
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("fs-control.sock"));
    let lock = dir.path().join("cache.lock");
    std::fs::write(&lock, b"").unwrap();
    let (listener, addr) = listening();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let control_socket = std::os::unix::net::UnixListener::bind(&control).unwrap();
    let not_listening = std::fs::File::open(&lock).unwrap();
    let directory = std::fs::File::open(dir.path()).unwrap();
    let mount_lines = [
        format!("fuse {}", mountpoint.display()),
        format!("control {}", control.display()),
    ];
    let lines = |extra: &[String]| -> Vec<String> {
        let mut lines = vec!["offer".to_owned(), format!("s3 {addr}")];
        lines.extend_from_slice(extra);
        lines
    };
    struct Case<'a> {
        lines: Vec<String>,
        fds: Vec<BorrowedFd<'a>>,
        device: fn(BorrowedFd<'_>) -> std::io::Result<()>,
        error: &'static str,
    }
    let refuse_device: fn(BorrowedFd<'_>) -> std::io::Result<()> = |_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not the FUSE device",
        ))
    };
    let lock_line = format!("lock {}", lock.display());
    let cases = [
        Case {
            lines: lines(&mount_lines),
            fds: vec![udp.as_fd(), not_listening.as_fd(), control_socket.as_fd()],
            device: |_| Ok(()),
            error: "hand-off: taking over a socket: the descriptor of the s3 socket is not a \
                    listening socket",
        },
        Case {
            lines: lines(&mount_lines),
            fds: vec![
                listener.as_fd(),
                not_listening.as_fd(),
                control_socket.as_fd(),
            ],
            device: refuse_device,
            error: "hand-off: checking the offered descriptors: not the FUSE device",
        },
        Case {
            lines: lines(&mount_lines),
            fds: vec![
                listener.as_fd(),
                not_listening.as_fd(),
                not_listening.as_fd(),
            ],
            device: |_| Ok(()),
            error: "hand-off: checking the offered descriptors: the descriptor of the control \
                    socket is not a listening Unix socket",
        },
        Case {
            lines: lines(&[
                mount_lines[0].clone(),
                mount_lines[1].clone(),
                lock_line.clone(),
            ]),
            fds: vec![
                listener.as_fd(),
                not_listening.as_fd(),
                control_socket.as_fd(),
                directory.as_fd(),
            ],
            device: |_| Ok(()),
            error: "",
        },
    ];
    for case in cases {
        let answer = fake(&path, case.lines, case.fds);
        let addrs = [addr];
        let (mut want, prepared) = want_mount(&mountpoint, &control);
        want.device = case.device;
        let ask = Ask {
            mount: Some(want),
            lock: Some(&lock),
        };
        let error = take_service(&handoff(path.clone()), &s3(&addrs), ask)
            .await
            .err()
            .unwrap()
            .to_string();
        if case.error.is_empty() {
            let named = format!(
                "hand-off: checking the offered lock: the lock descriptor for {} is not a regular \
                 file",
                lock.display()
            );
            assert_eq!(error, named);
        } else {
            assert_eq!(error, case.error);
        }
        assert_eq!(within("the answer", answer).await.unwrap(), ["nak"]);
        assert!(prepared.lock().unwrap().is_empty(), "nothing prepared");
    }
}

/// Asserts that a taker refuses a running instance of another user before it asks for anything:
/// the take is an error that names the user, the instance was asked nothing (it goes on, and a
/// taker of its own user takes its sockets after). It would catch a successor that takes a mount
/// or a lock from a process that is not the service's.
#[tokio::test]
async fn a_taker_refuses_an_offer_from_another_user() {
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &listener)])
            .unwrap()
            .serve(),
    );
    let me = rustix::process::geteuid().as_raw();
    let knobs = Knobs {
        copy: super::duplicate,
        uid: me.wrapping_add(1),
    };
    let addrs = [addr];
    let mut ask = Ask {
        mount: None,
        lock: None,
    };
    let refused = take_as(
        &handoff(path.clone()),
        &s3(&addrs),
        Request::Plain,
        &mut ask,
        knobs,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        refused.to_string(),
        format!(
            "hand-off: checking the running instance's user: the running instance is the user {me}"
        )
    );
    assert!(!serving.is_finished());
    let taken = take(&handoff(path), &s3(&addrs)).await.unwrap();
    assert!(taken.is_some());
    within("the end of the offer", serving).await.unwrap();
}

/// Asserts that an instance that holds no mount refuses a request for it by name, and that a
/// taker asks it again without the mount: it is handed the sockets and the lock with the `ready`
/// step, and no mount. It would catch a taker that fails the switch of section 4.2 of the design,
/// where the old S3 process holds no mount.
#[tokio::test]
async fn an_instance_without_a_mount_refuses_the_mount_line_and_is_asked_again_without_it() {
    let rig = Rig::new("handoff-service-nomount");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (dir, path) = scratch();
    let (listener, addr) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &listener)])
            .unwrap()
            .with_ready_wait(WAIT)
            .with_lock(old.clone())
            .serve(),
    );
    let refused = ask(&connect(&path).await, &["take 1", "ready", "mount"]).await;
    assert_eq!(texts(&refused.lines), ["refused", NO_MOUNT]);
    assert!(refused.fds.is_empty());
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("fs-control.sock"));
    let (want, prepared) = want_mount(&mountpoint, &control);
    let addrs = [addr];
    let ask = Ask {
        mount: Some(want),
        lock: Some(&lock),
    };
    let taken = take_service(&handoff(path), &s3(&addrs), ask)
        .await
        .unwrap()
        .unwrap();
    assert!(taken.mount.is_none());
    assert!(prepared.lock().unwrap().is_empty());
    assert!(taken.lock.is_some());
    let ready = taken.ready.expect("the step");
    let _predecessor = ready.send_keeping().await.unwrap();
    within("the end of the offer", serving).await.unwrap();
    assert_eq!(old.mode(), Mode::ReadOnly);
}

/// Asserts that an instance that holds the mount refuses, by name, a taker that does not ask for
/// it, with or without the `ready` step, and was told nothing of the hand-over: the mount is not
/// quiesced, and the instance goes on. It would catch a mount left with an instance that is about
/// to end, under the rollback of the design's section 4.2.
#[tokio::test]
async fn an_instance_with_the_mount_refuses_a_taker_that_does_not_ask_for_it() {
    let (dir, path) = scratch();
    let (mount, steps, _kernel) = fake_mount(dir.path());
    let (listener, addr) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &listener)])
            .unwrap()
            .with_ready_wait(WAIT)
            .with_mount(mount)
            .serve(),
    );
    for request in [&["take 1"][..], &["take 1", "ready"]] {
        let refused = ask(&connect(&path).await, request).await;
        assert_eq!(texts(&refused.lines), ["refused", HOLDS_MOUNT]);
    }
    let addrs = [addr];
    let refused = take(&handoff(path), &s3(&addrs)).await.err().unwrap();
    assert_eq!(
        refused.to_string(),
        format!("hand-off: the running instance refused: {HOLDS_MOUNT}")
    );
    assert!(told(&steps).is_empty());
    settle().await;
    assert!(!serving.is_finished());
    serving.abort();
}

/// Asserts the taker's refusals of an offer that is not what it asked for, each before `ack` with
/// a `nak` where the conversation is the protocol: a mount offered to a taker that did not ask for
/// it; none offered to one that did; a mount at other paths; a line named twice; and a `prepare`
/// that fails. It would catch a taker that acks an offer it cannot serve, which the running
/// instance would then quiesce for nothing or give up for good.
#[tokio::test]
async fn an_offer_that_is_not_what_was_asked_for_is_refused_before_ack() {
    let (dir, path) = scratch();
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("fs-control.sock"));
    let elsewhere = dir.path().join("elsewhere");
    let (listener, addr) = listening();
    let device = std::os::unix::net::UnixDatagram::pair().unwrap().0;
    let control_socket = std::os::unix::net::UnixListener::bind(&control).unwrap();
    let mount_lines = vec![
        "offer".to_owned(),
        format!("s3 {addr}"),
        format!("fuse {}", mountpoint.display()),
        format!("control {}", control.display()),
    ];
    let mount_fds = || vec![listener.as_fd(), device.as_fd(), control_socket.as_fd()];
    let addrs = [addr];

    // A mount to a taker that did not ask for it.
    let answer = fake(&path, mount_lines.clone(), mount_fds());
    let ask = Ask {
        mount: None,
        lock: None,
    };
    let error = take_service(&handoff(path.clone()), &s3(&addrs), ask)
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "hand-off: the running instance sent an offer of a mount that was not asked for"
    );
    assert_eq!(within("the answer", answer).await.unwrap(), ["nak"]);

    // No mount to a taker that asked for it; a mount elsewhere; a line twice; a failed prepare.
    let twice = [
        mount_lines.clone(),
        vec![format!("fuse {}", mountpoint.display())],
    ]
    .concat();
    let mut elsewhere_lines = mount_lines.clone();
    elsewhere_lines[2] = format!("fuse {}", elsewhere.display());
    let cases: [Refused<'_>; 4] = [
        (
            mount_lines[..2].to_vec(),
            vec![listener.as_fd()],
            true,
            "hand-off: the running instance sent an offer without the mount it was asked for"
                .to_owned(),
            Some("nak"),
        ),
        (
            elsewhere_lines,
            mount_fds(),
            true,
            format!(
                "hand-off: the running instance listens on [\"fuse {}\", \"control {}\"], these \
                 settings name [\"fuse {}\", \"control {}\"]",
                elsewhere.display(),
                control.display(),
                mountpoint.display(),
                control.display()
            ),
            Some("nak"),
        ),
        (
            twice,
            [mount_fds(), vec![device.as_fd()]].concat(),
            true,
            "hand-off: the running instance sent an offer that names a line twice".to_owned(),
            None,
        ),
        (
            mount_lines.clone(),
            mount_fds(),
            false,
            "hand-off: preparing to take the mount: the store is full".to_owned(),
            Some("nak"),
        ),
    ];
    for (lines, fds, prepares, error, answered) in cases {
        let answer = fake(&path, lines, fds);
        let (mut want, _) = want_mount(&mountpoint, &control);
        if !prepares {
            want.prepare = Box::new(|_| Err("the store is full".to_owned()));
        }
        let ask = Ask {
            mount: Some(want),
            lock: None,
        };
        let taken = take_service(&handoff(path.clone()), &s3(&addrs), ask).await;
        assert_eq!(taken.err().unwrap().to_string(), error);
        let said = within("the answer", answer).await.unwrap();
        match answered {
            Some(word) => assert_eq!(said, [word]),
            None => assert!(said.is_empty(), "no answer to what is not the protocol"),
        }
    }
}

/// Asserts the waits of a take of the mount, on the paused clock: an instance that never sends its
/// offer is given up after `server.handoff_wait`, though the mount's wait is longer, and the take
/// is a timeout that names the socket. It would catch a start that waits the mount's whole quiesce
/// for an instance that never answers.
#[tokio::test(start_paused = true)]
async fn an_instance_that_never_offers_is_given_up_after_the_wait_for_the_offer() {
    let (dir, path) = scratch();
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("fs-control.sock"));
    let listener = UnixListener::bind(&path).unwrap();
    let silent = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = wire::recv(&stream).await;
        std::future::pending::<()>().await;
        drop(stream);
    });
    let (want, _) = want_mount(&mountpoint, &control);
    let ask = Ask {
        mount: Some(want),
        lock: None,
    };
    let mut handoff = handoff(path.clone());
    handoff.wait = Duration::from_secs(10);
    let addrs: [std::net::SocketAddr; 0] = [];
    let error = take_service(&handoff, &s3(&addrs), ask)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HandoffError::Timeout(ref at) if *at == path));
    silent.abort();
}

/// Asserts that a request for the mount that is refused for another reason than not knowing the
/// line or holding no mount (another taker is at it) is not asked again: the take is the refusal.
/// It would catch a taker that asks for the sockets alone of an instance that holds the mount and
/// is busy, and leaves the mount behind.
#[tokio::test]
async fn a_mount_request_refused_for_another_reason_is_not_asked_again() {
    let (dir, path) = scratch();
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("fs-control.sock"));
    let listener = UnixListener::bind(&path).unwrap();
    // One answer, and then nobody listens: a second request would find nothing.
    let busy = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let request = wire::recv(&stream).await.unwrap().lines;
        let refused = [
            "refused".to_owned(),
            "another taker is taking the sockets".to_owned(),
        ];
        wire::send(&stream, &refused, &[]).await.unwrap();
        request
    });
    let (want, _) = want_mount(&mountpoint, &control);
    let ask = Ask {
        mount: Some(want),
        lock: None,
    };
    let addrs: [std::net::SocketAddr; 0] = [];
    let refused = take_service(&handoff(path), &s3(&addrs), ask).await;
    assert_eq!(
        refused.err().unwrap().to_string(),
        "hand-off: the running instance refused: another taker is taking the sockets"
    );
    assert_eq!(busy.await.unwrap(), ["take 1", "ready", "mount"]);
}

/// Asserts the whole wait of a take of the mount, on the paused clock: an instance that offers its
/// mount and, after `ack`, never says `done` is given up when the wait for the offer and the
/// mount's wait for its quiesce have passed, and the take is a timeout that names the socket. It
/// would catch a start that waits for ever on an instance that hangs in its quiesce.
#[tokio::test(start_paused = true)]
async fn an_instance_that_never_says_done_is_given_up_after_both_waits() {
    let (dir, path) = scratch();
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("fs-control.sock"));
    let device = std::os::unix::net::UnixDatagram::pair().unwrap().0;
    let control_socket = std::os::unix::net::UnixListener::bind(&control).unwrap();
    let lines = vec![
        "offer".to_owned(),
        format!("fuse {}", mountpoint.display()),
        format!("control {}", control.display()),
    ];
    let listener = UnixListener::bind(&path).unwrap();
    let hangs = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        let fds = [device.as_fd(), control_socket.as_fd()];
        wire::send(&stream, &lines, &fds).await.unwrap();
        assert_eq!(wire::recv(&stream).await.unwrap().lines, ["ack"]);
        // Quiescing for ever: no `done`.
        std::future::pending::<()>().await;
        drop(stream);
    });
    let (mut want, _) = want_mount(&mountpoint, &control);
    want.wait = Duration::from_secs(120);
    let ask = Ask {
        mount: Some(want),
        lock: None,
    };
    let mut handoff = handoff(path.clone());
    handoff.wait = Duration::from_secs(10);
    let addrs: [std::net::SocketAddr; 0] = [];
    let error = take_service(&handoff, &s3(&addrs), ask)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, HandoffError::Timeout(ref at) if *at == path));
    hangs.abort();
}
