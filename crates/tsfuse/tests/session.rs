//! The session (5.2): INIT, resume without INIT, RESEND, readers that stop and leave every device
//! open, non-blocking devices, the routing of each request, and replies on the reader that read
//! them with several readers. Ported from the fork's `src/session.rs` (7 of its 9 tests; the two
//! about passthrough are dropped) and extended to N readers.
//!
//! The device is one end of a `SOCK_SEQPACKET` socket pair: like `/dev/fuse` it delivers one
//! request per read and takes one reply per write, and it reads end-of-file once the other end,
//! the "kernel" the test holds, is closed. Several readers are several pairs, as clones are
//! several descriptors of one connection.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;

use std::io::{Read as _, Write as _};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Barrier, Mutex};

use common::{getattr, init_request, request};
use tsfuse::abi::{InitConfig, InitFlags, InterruptIn, UNIQUE_RESEND};
use tsfuse::{
    DevFd, Errno, Fs, Handled, ReaderEnd, Replier, Request, Session, SessionError, Stopper,
};

/// The kernel's end of a fake device.
struct Kernel(UnixStream);

impl Kernel {
    /// Queues one request.
    fn send(&self, bytes: &[u8]) -> std::io::Result<()> {
        let n = (&self.0).write(bytes)?;
        assert_eq!(n, bytes.len(), "a packet is written whole");
        Ok(())
    }

    /// Takes the next reply.
    fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&self.0).read(buf)
    }
}

/// A device and the kernel's end of it.
fn fake_device() -> (DevFd, Kernel) {
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    let (device, kernel) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    (DevFd::from_fd(device), Kernel(UnixStream::from(kernel)))
}

/// A descriptor that polls readable and fails every read (`ENOTCONN`): a listening socket with a
/// connection waiting. The listener and the connection are returned to be kept alive with it.
fn unreadable() -> (DevFd, UnixListener, UnixStream) {
    use std::os::linux::net::SocketAddrExt as _;
    let name = format!(
        "tsfuse-unreadable-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    );
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name).unwrap();
    let listener = UnixListener::bind_addr(&addr).unwrap();
    let client = UnixStream::connect_addr(&addr).unwrap();
    let fd = rustix::io::fcntl_dupfd_cloexec(&listener, 0).unwrap();
    (DevFd::from_fd(fd), listener, client)
}

/// `(error, unique, len)` of the next message the session wrote.
fn read_reply(kernel: &Kernel) -> (i32, u64, usize) {
    let mut buf = [0u8; 4096];
    let n = kernel.recv(&mut buf).unwrap();
    let len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(len as usize, n, "the header's length is the message's");
    let error = i32::from_le_bytes(buf[4..8].try_into().unwrap());
    let unique = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    (error, unique, n)
}

/// Whether the kernel's end has a message waiting.
fn pending(kernel: &Kernel) -> bool {
    kernel.0.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 64];
    let got = kernel.recv(&mut buf).is_ok();
    kernel.0.set_nonblocking(false).unwrap();
    got
}

const RESEND_AND_PAGES: u64 =
    InitFlags::HAS_RESEND.0 | InitFlags::MAX_PAGES.0 | InitFlags::INIT_EXT.0;

/// A session initialised by protocol 7.40 offering `offered`.
fn initialised(offered: u64) -> Session {
    let (device, kernel) = fake_device();
    kernel.send(&init_request(7, 40, offered)).unwrap();
    let session = Session::init(device, &InitConfig::tree()).unwrap();
    assert_eq!(read_reply(&kernel).0, 0);
    session
}

/// Answers GETATTR with `EBADF` and records whether each was resent; parks LOOKUP for the test
/// to complete; panics on READLINK.
#[derive(Default)]
struct TestFs {
    resends: Mutex<Vec<bool>>,
    parked: Mutex<Vec<Replier>>,
}

impl Fs for TestFs {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        match req.header.opcode {
            3 => {
                self.resends
                    .lock()
                    .unwrap()
                    .push(req.header.unique & UNIQUE_RESEND != 0);
                let _ = replier.error(Errno(9));
                Handled::Done
            }
            1 => {
                self.parked.lock().unwrap().push(replier);
                Handled::Parked
            }
            5 => panic!("READLINK"),
            _ => {
                let _ = replier.error(Errno::ENOSYS);
                Handled::Done
            }
        }
    }
}

/// Serves `session` with `fs` on its own threads; the stopper and the join of every reader.
fn serve(
    session: Session,
    fs: Arc<TestFs>,
) -> (
    Stopper,
    std::thread::JoinHandle<Result<Vec<ReaderEnd>, SessionError>>,
) {
    let serving = session.serve(fs).unwrap();
    let stopper = serving.stopper();
    (stopper, std::thread::spawn(move || serving.join()))
}

#[test]
fn init_runs_the_handshake_and_reports_its_outcome() {
    let (device, kernel) = fake_device();
    kernel.send(&init_request(7, 40, RESEND_AND_PAGES)).unwrap();
    let session = Session::init(device, &InitConfig::tree()).unwrap();

    assert_eq!(read_reply(&kernel), (0, 1, 16 + 64));
    let n = session.negotiated();
    assert_eq!(n.minor, 40);
    assert_eq!(n.offered, InitFlags(RESEND_AND_PAGES));
    assert_eq!(
        n.granted,
        InitFlags(RESEND_AND_PAGES),
        "every flag offered is asked for"
    );
    // The session runs without the required flags the kernel lacks, and names them.
    assert_eq!(
        n.missing,
        tsfuse::abi::MountKind::Tree
            .required()
            .difference(InitFlags(RESEND_AND_PAGES))
    );
    assert_eq!(n.reply.max_readahead, 65536);
    assert_eq!(session.readers(), 1);
    assert_eq!(session.tokens().registered(), 0);
}

#[test]
fn init_refuses_a_connection_that_is_already_initialised() {
    let (device, kernel) = fake_device();
    kernel.send(&getattr(9)).unwrap();
    let err = Session::init(device, &InitConfig::tree()).unwrap_err();
    assert!(
        matches!(
            err,
            SessionError::AlreadyInitialised {
                opcode: 3,
                unique: 9
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "the FUSE connection is already initialised: its first request (unique 9) is opcode 3, not INIT"
    );
    assert_eq!(read_reply(&kernel), (-5, 9, 16));
}

#[test]
fn a_kernel_of_a_newer_major_is_answered_with_ours_and_its_next_init_is_read() {
    let (device, kernel) = fake_device();
    kernel.send(&init_request(8, 0, 0)).unwrap();
    kernel.send(&init_request(7, 45, RESEND_AND_PAGES)).unwrap();
    let session = Session::init(device, &InitConfig::tree()).unwrap();

    let mut buf = [0u8; 256];
    let n = kernel.recv(&mut buf).unwrap();
    let (ours, _) = tsfuse::abi::InitOut::read_prefix(&buf[16..n]).unwrap();
    assert_eq!((ours.major, ours.minor), (7, 45));
    assert_eq!(read_reply(&kernel), (0, 1, 80));
    assert_eq!(session.negotiated().minor, 45);
}

#[test]
fn a_kernel_too_old_is_answered_eproto() {
    let (device, kernel) = fake_device();
    kernel.send(&init_request(7, 22, 0)).unwrap();
    let err = Session::init(device, &InitConfig::tree()).unwrap_err();
    assert!(matches!(err, SessionError::Init(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "FUSE kernel protocol 7.22 is older than the 7.23 tsfuse needs"
    );
    assert_eq!(read_reply(&kernel), (-71, 1, 16));
}

#[test]
fn an_init_shorter_than_an_old_kernels_is_answered_eio() {
    let (device, kernel) = fake_device();
    kernel.send(&request(26, 1, 0, &[7, 0, 0, 0, 40])).unwrap();
    let err = Session::init(device, &InitConfig::tree()).unwrap_err();
    assert!(
        matches!(err, SessionError::MalformedInit { body_len: 5 }),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "the FUSE INIT request has a body of 5 bytes, too short for its arguments"
    );
    assert_eq!(read_reply(&kernel), (-5, 1, 16));
}

#[test]
fn init_on_a_device_whose_kernel_is_gone_ends_before_init() {
    let (device, kernel) = fake_device();
    drop(kernel);
    let err = Session::init(device, &InitConfig::tree()).unwrap_err();
    assert!(
        matches!(err, SessionError::EndedBeforeInit(ReaderEnd::Unmounted)),
        "{err:?}"
    );
}

#[test]
fn init_of_a_message_with_no_header_is_a_read_error() {
    let (device, kernel) = fake_device();
    kernel.send(&[1, 2, 3]).unwrap();
    let err = Session::init(device, &InitConfig::tree()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "reading the FUSE device failed: a FUSE request of 3 bytes has no valid header"
    );
}

#[test]
fn init_whose_reply_cannot_be_written_is_a_write_error() {
    let (device, kernel) = fake_device();
    kernel.send(&init_request(7, 40, 0)).unwrap();
    drop(kernel);
    let err = Session::init(device, &InitConfig::tree()).unwrap_err();
    assert!(matches!(err, SessionError::Write(_)), "{err:?}");
}

#[test]
fn init_on_a_descriptor_that_cannot_be_read_is_a_read_error() {
    let (device, _listener, _client) = unreadable();
    let err = Session::init(device, &InitConfig::tree()).unwrap_err();
    assert!(matches!(err, SessionError::Read(_)), "{err:?}");
}

#[test]
fn resume_serves_a_queued_request_without_init() {
    let negotiated = *initialised(RESEND_AND_PAGES).negotiated();
    let (device, kernel) = fake_device();
    // Queued before the session exists, as when a job calls while no server reads.
    kernel.send(&getattr(5)).unwrap();
    let session = Session::resume(vec![device], negotiated).unwrap();
    assert_eq!(session.negotiated(), &negotiated);
    let fs = Arc::new(TestFs::default());
    let (stopper, joined) = serve(session, fs.clone());

    assert_eq!(read_reply(&kernel), (-9, 5, 16));
    stopper.stop();
    assert_eq!(joined.join().unwrap().unwrap(), vec![ReaderEnd::Stopped]);
    assert_eq!(*fs.resends.lock().unwrap(), vec![false]);
}

#[test]
fn resume_needs_a_device() {
    let negotiated = *initialised(0).negotiated();
    let err = Session::resume(Vec::new(), negotiated).unwrap_err();
    assert!(matches!(err, SessionError::NoDevice), "{err:?}");
    assert_eq!(
        err.to_string(),
        "a FUSE session needs at least one device descriptor"
    );
}

#[test]
fn a_resent_request_is_marked_and_answered_under_its_id() {
    let negotiated = *initialised(0).negotiated();
    let (device, kernel) = fake_device();
    let fs = Arc::new(TestFs::default());
    let (stopper, joined) = serve(
        Session::resume(vec![device], negotiated).unwrap(),
        fs.clone(),
    );

    let unique = 7 | UNIQUE_RESEND;
    kernel.send(&getattr(unique)).unwrap();
    assert_eq!(read_reply(&kernel), (-9, unique, 16));
    kernel.send(&getattr(8)).unwrap();
    assert_eq!(read_reply(&kernel), (-9, 8, 16));

    stopper.stop();
    assert_eq!(joined.join().unwrap().unwrap(), vec![ReaderEnd::Stopped]);
    assert_eq!(*fs.resends.lock().unwrap(), vec![true, false]);
}

#[test]
fn resend_is_a_header_only_notification() {
    let negotiated = *initialised(0).negotiated();
    let (device, kernel) = fake_device();
    let session = Session::resume(vec![device], negotiated).unwrap();
    session.resend().unwrap();
    // Notifications have unique 0 and the notify code in the error field.
    assert_eq!(read_reply(&kernel), (7, 0, 16));
}

#[test]
fn a_stopper_ends_every_reader_and_leaves_every_device_open() {
    let negotiated = *initialised(0).negotiated();
    let pairs: Vec<(DevFd, Kernel)> = (0..3).map(|_| fake_device()).collect();
    let (devices, kernels): (Vec<DevFd>, Vec<Kernel>) = pairs.into_iter().unzip();
    let session = Session::resume(devices, negotiated).unwrap();
    assert_eq!(session.readers(), 3);
    let kept = session.dup_devices().unwrap();
    let serving = session.serve(Arc::new(TestFs::default())).unwrap();
    assert_eq!(serving.dup_devices().unwrap().len(), 3);

    let stopper = serving.stopper();
    stopper.stop();
    stopper.clone().stop();
    assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped; 3]);

    // No device was closed: the next server reads each one's next request, and each was left
    // non-blocking.
    for (fd, kernel) in kept.iter().zip(&kernels) {
        kernel.send(&getattr(3)).unwrap();
        let flags = rustix::fs::fcntl_getfl(fd).unwrap();
        assert!(flags.contains(rustix::fs::OFlags::NONBLOCK));
        let mut buf = [0u8; 64];
        assert_eq!(rustix::io::read(fd.as_fd(), &mut buf).unwrap(), 56);
    }
}

#[test]
fn a_non_blocking_device_waits_for_requests_and_destroy_ends_its_reader() {
    let (device, kernel) = fake_device();
    let fd = device.try_clone_fd().unwrap();
    let flags = rustix::fs::fcntl_getfl(&fd).unwrap();
    rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    // INIT on a non-blocking device waits for the request too.
    let init = std::thread::spawn(move || Session::init(device, &InitConfig::tree()));
    kernel.send(&init_request(7, 40, 0)).unwrap();
    let session = init.join().unwrap().unwrap();
    assert_eq!(read_reply(&kernel).0, 0);
    let (_stopper, joined) = serve(session, Arc::new(TestFs::default()));

    kernel.send(&getattr(4)).unwrap();
    assert_eq!(read_reply(&kernel), (-9, 4, 16));
    kernel.send(&request(38, 6, 0, &[])).unwrap();
    assert_eq!(read_reply(&kernel), (0, 6, 16));
    assert_eq!(joined.join().unwrap().unwrap(), vec![ReaderEnd::Destroyed]);
}

#[test]
fn a_reader_whose_kernel_is_gone_ends_unmounted() {
    let negotiated = *initialised(0).negotiated();
    let (device, kernel) = fake_device();
    let (_stopper, joined) = serve(
        Session::resume(vec![device], negotiated).unwrap(),
        Arc::new(TestFs::default()),
    );
    drop(kernel);
    assert_eq!(joined.join().unwrap().unwrap(), vec![ReaderEnd::Unmounted]);
}

#[test]
fn each_request_is_routed_by_its_opcode() {
    let negotiated = *initialised(0).negotiated();
    let (device, kernel) = fake_device();
    let fs = Arc::new(TestFs::default());
    let (stopper, joined) = serve(
        Session::resume(vec![device], negotiated).unwrap(),
        fs.clone(),
    );

    // FORGET and BATCH_FORGET are never answered.
    kernel
        .send(&request(2, 20, 5, &[1, 0, 0, 0, 0, 0, 0, 0]))
        .unwrap();
    kernel.send(&request(42, 21, 0, &[0; 8])).unwrap();
    // A second INIT is answered EIO; an opcode the library does not implement ENOSYS (SYMLINK).
    kernel.send(&init_request(7, 40, 0)).unwrap();
    assert_eq!(read_reply(&kernel), (-5, 1, 16));
    kernel.send(&request(6, 22, 1, b"a\0b\0")).unwrap();
    assert_eq!(read_reply(&kernel), (-38, 22, 16));
    // A header naming more bytes than were read is answered EIO under its id; fewer than 16
    // bytes have no id and are dropped.
    let mut long = getattr(23);
    long[0] = 200;
    kernel.send(&long).unwrap();
    assert_eq!(read_reply(&kernel), (-5, 23, 16));
    kernel.send(&[0; 12]).unwrap();
    // An INTERRUPT with a short body is not answered; one of an unknown request is answered
    // EAGAIN under its own id.
    kernel.send(&request(36, 24 | 1, 0, &[0; 4])).unwrap();
    let interrupt = InterruptIn { unique: 99 };
    kernel
        .send(&request(36, 25 | 1, 0, interrupt.as_bytes()))
        .unwrap();
    assert_eq!(read_reply(&kernel), (-11, 25 | 1, 16));
    // A request the file system answers is answered once.
    kernel.send(&getattr(26)).unwrap();
    assert_eq!(read_reply(&kernel), (-9, 26, 16));
    assert!(
        !pending(&kernel),
        "FORGET, BATCH_FORGET and the dropped messages got no reply"
    );

    stopper.stop();
    assert_eq!(joined.join().unwrap().unwrap(), vec![ReaderEnd::Stopped]);
}

#[test]
fn with_three_readers_a_reply_goes_to_the_reader_that_read_it_and_interrupt_to_its_owner() {
    let negotiated = *initialised(0).negotiated();
    let pairs: Vec<(DevFd, Kernel)> = (0..3).map(|_| fake_device()).collect();
    let (devices, kernels): (Vec<DevFd>, Vec<Kernel>) = pairs.into_iter().unzip();
    let fs = Arc::new(TestFs::default());
    let session = Session::resume(devices, negotiated).unwrap();
    let tokens = session.tokens().clone();
    let (stopper, joined) = serve(session, fs.clone());

    // A LOOKUP parks on each reader.
    for (i, kernel) in kernels.iter().enumerate() {
        kernel.send(&request(1, 100 + i as u64, 1, b"x\0")).unwrap();
    }
    while tokens.registered() < 3 {
        std::thread::yield_now();
    }
    // An INTERRUPT of reader 0's request, read by reader 2: EINTR on reader 0's device.
    kernels[2]
        .send(&request(
            36,
            200 | 1,
            0,
            InterruptIn { unique: 100 }.as_bytes(),
        ))
        .unwrap();
    assert_eq!(read_reply(&kernels[0]), (-4, 100, 16));
    // The other two complete from another task, out of order, each on its own reader's device.
    let mut parked = std::mem::take(&mut *fs.parked.lock().unwrap());
    parked.sort_by_key(|r| std::cmp::Reverse(r.unique()));
    let barrier = Arc::new(Barrier::new(2));
    let gate = barrier.clone();
    let completer = std::thread::spawn(move || {
        gate.wait();
        for replier in parked {
            let _ = replier.ok(&[b"done"]);
        }
    });
    barrier.wait();
    completer.join().unwrap();
    assert_eq!(read_reply(&kernels[2]), (0, 102, 20));
    assert_eq!(read_reply(&kernels[1]), (0, 101, 20));
    for kernel in &kernels {
        assert!(!pending(kernel), "each request was answered once");
    }

    stopper.stop();
    assert_eq!(joined.join().unwrap().unwrap(), vec![ReaderEnd::Stopped; 3]);
}

#[test]
fn a_reader_that_panics_answers_eio_and_the_join_names_it() {
    let negotiated = *initialised(0).negotiated();
    let (device, kernel) = fake_device();
    let (_stopper, joined) = serve(
        Session::resume(vec![device], negotiated).unwrap(),
        Arc::new(TestFs::default()),
    );
    kernel.send(&request(5, 30, 1, &[])).unwrap();
    // The replier dropped in the unwind answers EIO.
    assert_eq!(read_reply(&kernel), (-5, 30, 16));
    let err = joined.join().unwrap().unwrap_err();
    assert!(
        matches!(err, SessionError::Panicked { index: 0 }),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "FUSE reader 0 panicked");
}

#[test]
fn a_reader_that_cannot_read_its_device_fails_and_the_join_names_it() {
    let negotiated = *initialised(0).negotiated();
    let (good, kernel) = fake_device();
    drop(kernel);
    let (bad, _listener, _client) = unreadable();
    let session = Session::resume(vec![good, bad], negotiated).unwrap();
    let serving = session.serve(Arc::new(TestFs::default())).unwrap();
    let err = serving.join().unwrap_err();
    assert!(
        matches!(err, SessionError::Reader { index: 1, .. }),
        "{err:?}"
    );
    assert!(
        err.to_string().starts_with("FUSE reader 1 failed: "),
        "{err}"
    );
}

#[test]
fn a_device_the_runtime_cannot_register_fails_its_reader() {
    let negotiated = *initialised(0).negotiated();
    let dir = tierstore_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tsfuse-file");
    let file = std::fs::File::create(dir.path().join("not-a-device")).unwrap();
    // epoll refuses a regular file (EPERM).
    let session = Session::resume(vec![DevFd::from_fd(file.into())], negotiated).unwrap();
    let err = session
        .serve(Arc::new(TestFs::default()))
        .unwrap()
        .join()
        .unwrap_err();
    assert!(
        matches!(err, SessionError::Reader { index: 0, .. }),
        "{err:?}"
    );
}

#[test]
fn two_readers_of_one_queue_answer_each_request_once() {
    // As clones share the connection's pending queue, two descriptors of one socket share its
    // queue: a request wakes both, one reads it and the other finds the queue empty and waits.
    let negotiated = *initialised(0).negotiated();
    let (device, kernel) = fake_device();
    let second = DevFd::from_fd(device.try_clone_fd().unwrap());
    let fs = Arc::new(TestFs::default());
    let (stopper, joined) = serve(
        Session::resume(vec![device, second], negotiated).unwrap(),
        fs.clone(),
    );
    for unique in 40..50 {
        kernel.send(&getattr(unique)).unwrap();
        assert_eq!(read_reply(&kernel), (-9, unique, 16));
    }
    assert!(!pending(&kernel), "no request was answered twice");
    assert_eq!(fs.resends.lock().unwrap().len(), 10);
    stopper.stop();
    assert_eq!(joined.join().unwrap().unwrap(), vec![ReaderEnd::Stopped; 2]);
}
