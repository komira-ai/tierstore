//! Takeover against fake devices (5.2, item 5): the handover a supervisor keeps, passed to the
//! next server by `SCM_RIGHTS` or kept as a list of descriptors; the resume that sends RESEND
//! before any reader runs; the hand-over that leaves parked requests unanswered for the next
//! server instead of answering them `EIO`; and every way a handover is refused.
//!
//! Each fake device has its own queue, so the "kernel" a test holds plays both of the kernel's
//! parts: it takes the RESEND, and it delivers each resent request (bit 63 set) to a device, as
//! the real kernel does from the connection's input queue to whichever clone reads it. The real
//! kernel's part is `tests/takeover_integration.rs`.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, Mutex};

use common::fake::{Kernel, fake_device, pending, read_reply};
use common::{init_request, request};
use tsfuse::abi::{InitConfig, InitFlags, Negotiated, UNIQUE_RESEND};
use tsfuse::{
    Completion, DevFd, Fs, Handled, Handover, HandoverError, MAX_DEVICES, ReaderEnd, Replier,
    Request, Session,
};

/// The terms of a session initialised by protocol 7.45 offering `HAS_RESEND`.
fn terms() -> Negotiated {
    let (device, kernel) = fake_device();
    let flags = InitFlags::HAS_RESEND.0 | InitFlags::MAX_PAGES.0 | InitFlags::INIT_EXT.0;
    kernel.send(&init_request(7, 45, flags)).unwrap();
    let session = Session::init(device, &InitConfig::tree()).unwrap();
    assert_eq!(read_reply(&kernel).0, 0);
    *session.negotiated()
}

/// `n` fake devices and their kernel ends.
fn devices(n: usize) -> (Vec<DevFd>, Vec<Kernel>) {
    (0..n).map(|_| fake_device()).unzip()
}

fn handover(devices: Vec<DevFd>) -> Handover {
    Handover::new(terms(), devices.into_iter().map(OwnedFd::from).collect()).unwrap()
}

/// A connected `SOCK_SEQPACKET` pair: the supervisor's end and the next server's.
fn socket_pair() -> (OwnedFd, OwnedFd) {
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap()
}

/// A LOOKUP of `x` under the root.
fn lookup(unique: u64) -> Vec<u8> {
    request(1, unique, 1, b"x\0")
}

/// The old server: parks every request it is given.
#[derive(Default)]
struct Parks(Mutex<Vec<Replier>>);

impl Fs for Parks {
    fn handle(&self, _req: Request<'_>, replier: Replier) -> Handled {
        self.0.lock().unwrap().push(replier);
        Handled::Parked
    }
}

impl Parks {
    fn take(&self) -> Vec<Replier> {
        let mut parked = std::mem::take(&mut *self.0.lock().unwrap());
        parked.sort_by_key(Replier::unique);
        parked
    }
}

/// The next server: answers every request with `new` and records its unique.
#[derive(Default)]
struct Answers(Mutex<Vec<u64>>);

impl Fs for Answers {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        self.0.lock().unwrap().push(req.header.unique);
        let _ = replier.ok(&[b"new"]);
        Handled::Done
    }
}

/// Starts `session` with the old server and parks one LOOKUP, `100 + i`, on each device `i`.
fn parked_on_every_device(session: Session, kernels: &[Kernel]) -> (tsfuse::Serving, Arc<Parks>) {
    let tokens = session.tokens().clone();
    let old = Arc::new(Parks::default());
    let serving = session.serve(old.clone()).unwrap();
    for (i, kernel) in kernels.iter().enumerate() {
        kernel.send(&lookup(100 + i as u64)).unwrap();
    }
    while tokens.registered() < kernels.len() {
        std::thread::yield_now();
    }
    (serving, old)
}

#[test]
fn a_supervisor_keeps_every_device_and_the_next_server_answers_each_parked_request_once() {
    let (devs, kernels) = devices(3);
    let session = Session::resume(handover(devs)).unwrap();
    // The old session was itself resumed, so its RESEND went first.
    assert_eq!(read_reply(&kernels[0]), (7, 0, 16));
    // What the supervisor keeps: every device, the mount's and every clone, as descriptors.
    let kept = Handover::from_fds(session.handover().unwrap().into_fds().unwrap()).unwrap();
    assert_eq!(kept.devices(), 3);
    let (serving, old) = parked_on_every_device(session, &kernels);

    // The old server hands over and exits: its parked repliers are dropped and answer nothing.
    assert_eq!(serving.hand_over().unwrap(), vec![ReaderEnd::Stopped; 3]);
    drop(old.take());
    for kernel in &kernels {
        assert!(!pending(kernel), "no EIO from the server that handed over");
    }

    // The supervisor passes the handover; the next server resumes it and sends RESEND on the
    // first device only, before any reader runs.
    let (supervisor, next) = socket_pair();
    kept.send(supervisor.as_fd()).unwrap();
    let received = Handover::receive(next.as_fd()).unwrap();
    assert_eq!(received.devices(), 3);
    assert_eq!(received.negotiated(), &terms());
    let resumed = Session::resume(received).unwrap();
    assert_eq!(read_reply(&kernels[0]), (7, 0, 16));
    assert!(
        kernels.iter().all(|k| !pending(k)),
        "one RESEND, on device 0"
    );

    // The kernel resends each request with bit 63 set; each is answered once, under the unique
    // as read, on the device that read it.
    let new = Arc::new(Answers::default());
    let serving = resumed.serve(new.clone()).unwrap();
    for (i, kernel) in kernels.iter().enumerate() {
        kernel
            .send(&lookup((100 + i as u64) | UNIQUE_RESEND))
            .unwrap();
    }
    for (i, kernel) in kernels.iter().enumerate() {
        assert_eq!(
            read_reply(kernel),
            (0, (100 + i as u64) | UNIQUE_RESEND, 19)
        );
    }
    serving.stopper().stop();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped; 3]);
    assert!(kernels.iter().all(|k| !pending(k)), "each answered once");
    let mut seen = new.0.lock().unwrap().clone();
    seen.sort_unstable();
    assert_eq!(seen, [100, 101, 102].map(|u| u | UNIQUE_RESEND));
    // The supervisor still holds its own copy for the server after this one.
    assert_eq!(kept.devices(), 3);
}

#[test]
fn a_server_stopped_without_handing_over_answers_its_dropped_requests_eio() {
    let (devs, kernels) = devices(2);
    let session = Session::resume(handover(devs)).unwrap();
    assert_eq!(read_reply(&kernels[0]), (7, 0, 16));
    let (serving, old) = parked_on_every_device(session, &kernels);
    serving.stopper().stop();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped; 2]);
    drop(old.take());
    assert_eq!(read_reply(&kernels[0]), (-5, 100, 16));
    assert_eq!(read_reply(&kernels[1]), (-5, 101, 16));
}

#[test]
fn after_hand_over_a_parked_request_the_old_server_completes_is_still_written() {
    let (devs, kernels) = devices(2);
    let session = Session::resume(handover(devs)).unwrap();
    assert_eq!(read_reply(&kernels[0]), (7, 0, 16));
    let kept = session.handover().unwrap();
    let (serving, old) = parked_on_every_device(session, &kernels);
    assert_eq!(serving.hand_over().unwrap(), vec![ReaderEnd::Stopped; 2]);
    let mut parked = old.take().into_iter();
    // Completed after the hand-over, before the next server's RESEND: the request is answered
    // here, once, and the kernel resends nothing of it.
    let first = parked.next().unwrap();
    assert!(matches!(first.ok(&[b"old"]), Completion::Written));
    assert_eq!(read_reply(&kernels[0]), (0, 100, 19));
    // Dropped: nothing written; it is left for the next server.
    drop(parked);
    assert!(!pending(&kernels[1]));
    assert_eq!(kept.devices(), 2);
}

#[test]
fn a_handover_as_descriptors_is_its_terms_then_every_device_in_order() {
    let (devs, _kernels) = devices(3);
    let inodes: Vec<u64> = devs
        .iter()
        .map(|d| rustix::fs::fstat(d).unwrap().st_ino)
        .collect();
    let fds = handover(devs).into_fds().unwrap();
    assert_eq!(fds.len(), 4);
    let seals = rustix::fs::fcntl_get_seals(&fds[0]).unwrap();
    assert!(seals.contains(rustix::fs::SealFlags::WRITE), "{seals:?}");
    let back = Handover::from_fds(fds).unwrap();
    assert_eq!(back.negotiated(), &terms());
    let again: Vec<u64> = back
        .into_fds()
        .unwrap()
        .iter()
        .skip(1)
        .map(|fd| rustix::fs::fstat(fd).unwrap().st_ino)
        .collect();
    assert_eq!(again, inodes);
}

#[test]
fn a_handover_needs_one_device_and_holds_at_most_max_devices() {
    let err = Handover::new(terms(), Vec::new()).unwrap_err();
    assert!(matches!(err, HandoverError::NoDevice), "{err:?}");
    assert_eq!(
        err.to_string(),
        "a FUSE handover needs at least one device descriptor"
    );
    let (one, _peer) = UnixDatagram::pair().unwrap();
    let many = |n: usize| -> Vec<OwnedFd> {
        (0..n)
            .map(|_| rustix::io::fcntl_dupfd_cloexec(&one, 0).unwrap())
            .collect()
    };
    let err = Handover::new(terms(), many(MAX_DEVICES + 1)).unwrap_err();
    assert_eq!(
        err.to_string(),
        "a FUSE handover holds at most 252 device descriptors, not 253"
    );

    // The most a handover holds passes in one message, with its terms.
    let full = Handover::new(terms(), many(MAX_DEVICES)).unwrap();
    let (supervisor, next) = socket_pair();
    full.send(supervisor.as_fd()).unwrap();
    assert_eq!(
        Handover::receive(next.as_fd()).unwrap().devices(),
        MAX_DEVICES
    );
}

#[test]
fn a_handover_received_with_credentials_beside_it_is_taken() {
    let (devs, _kernels) = devices(2);
    let (supervisor, next) = socket_pair();
    rustix::net::sockopt::set_socket_passcred(&next, true).unwrap();
    handover(devs).send(supervisor.as_fd()).unwrap();
    assert_eq!(Handover::receive(next.as_fd()).unwrap().devices(), 2);
}

#[test]
fn a_full_handover_with_credentials_beside_it_does_not_fit_and_is_refused() {
    let (one, _peer) = UnixDatagram::pair().unwrap();
    let fds = (0..MAX_DEVICES)
        .map(|_| rustix::io::fcntl_dupfd_cloexec(&one, 0).unwrap())
        .collect();
    let (supervisor, next) = socket_pair();
    rustix::net::sockopt::set_socket_passcred(&next, true).unwrap();
    Handover::new(terms(), fds)
        .unwrap()
        .send(supervisor.as_fd())
        .unwrap();
    let err = Handover::receive(next.as_fd()).unwrap_err();
    assert!(matches!(err, HandoverError::Truncated), "{err:?}");
    assert_eq!(
        err.to_string(),
        "a FUSE handover message carried more descriptors than one handover holds"
    );
}

#[test]
fn descriptors_that_are_not_a_handover_are_refused_without_reading_a_device() {
    assert!(matches!(
        Handover::from_fds(Vec::new()).unwrap_err(),
        HandoverError::NoDevice
    ));
    // A device first: refused before any read, so the request queued on it stays there.
    let (device, kernel) = fake_device();
    kernel.send(&lookup(9)).unwrap();
    let fd = device.try_clone_fd().unwrap();
    let err = Handover::from_fds(vec![OwnedFd::from(device)]).unwrap_err();
    assert!(matches!(err, HandoverError::NotTerms), "{err:?}");
    let mut buf = [0u8; 64];
    assert_eq!(rustix::io::read(&fd, &mut buf).unwrap(), 42);
    // Terms that name two devices, followed by one.
    let (devs, _kernels) = devices(2);
    let mut fds = handover(devs).into_fds().unwrap();
    fds.pop();
    let err = Handover::from_fds(fds).unwrap_err();
    assert_eq!(
        err.to_string(),
        "the FUSE handover's terms name 2 devices and 1 came with them"
    );
}

#[test]
fn a_socket_closed_before_a_handover_or_a_message_that_is_not_one_is_refused() {
    let (supervisor, next) = socket_pair();
    drop(supervisor);
    let err = Handover::receive(next.as_fd()).unwrap_err();
    assert!(matches!(err, HandoverError::Closed), "{err:?}");
    assert_eq!(
        err.to_string(),
        "the socket of a FUSE handover closed before one was passed"
    );

    let (supervisor, next) = socket_pair();
    rustix::io::write(&supervisor, b"hello").unwrap();
    let err = Handover::receive(next.as_fd()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "a message of 5 bytes on the socket of a FUSE handover is not one"
    );

    // The magic with a device and no terms: the device is not read.
    let (supervisor, next) = socket_pair();
    let (device, _kernel) = fake_device();
    let fds = [device.as_fd()];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = rustix::net::SendAncillaryBuffer::new(&mut space);
    assert!(control.push(rustix::net::SendAncillaryMessage::ScmRights(&fds)));
    rustix::net::sendmsg(
        &supervisor,
        &[std::io::IoSlice::new(b"tsfuseHO")],
        &mut control,
        rustix::net::SendFlags::empty(),
    )
    .unwrap();
    let err = Handover::receive(next.as_fd()).unwrap_err();
    assert!(matches!(err, HandoverError::NotTerms), "{err:?}");
}

#[test]
fn a_descriptor_that_is_not_a_socket_is_a_socket_error_both_ways() {
    let file = rustix::fs::memfd_create("not-a-socket", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let (devs, _kernels) = devices(1);
    let err = handover(devs).send(file.as_fd()).unwrap_err();
    assert!(matches!(err, HandoverError::Socket(_)), "{err:?}");
    let err = Handover::receive(file.as_fd()).unwrap_err();
    assert!(
        err.to_string()
            .starts_with("passing a FUSE handover on its socket failed: "),
        "{err}"
    );
}
