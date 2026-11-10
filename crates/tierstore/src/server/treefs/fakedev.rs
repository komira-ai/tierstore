//! A stand-in for the kernel's end of `/dev/fuse`, for the tests of a process that takes a
//! connection over: a datagram socket pair delivers one request per read and takes one reply per
//! write, as the device does. The tests speak the FUSE protocol on it by hand, so that a process
//! can be started, stopped and replaced over it with no mount and no privilege.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::time::Duration;

use fuser::{InitFlags, InitOutcome, Version};

use super::config::FsSettings;
use super::handover::{HandoverError, Offer, Paths};
use super::init_record::{self, Record};
use super::ino;
use super::run::{Prepared, Running};
use super::testing::{Rig, sample};

const LOOKUP: u32 = 1;
const GETATTR: u32 = 3;
const OPEN: u32 = 14;
const READ: u32 = 15;
const RELEASE: u32 = 18;

/// The kernel's end of the pair.
pub(in crate::server::treefs) struct FakeDevice {
    kernel: UnixDatagram,
}

/// A reply, or a notification, as the kernel's end gets it.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::server::treefs) struct Message {
    /// The request it answers; 0 for a notification.
    pub unique: u64,
    /// The error field: minus an errno, 0 for success, the code of a notification.
    pub error: i32,
    /// The bytes after the header.
    pub body: Vec<u8>,
}

impl Message {
    /// The file handle in the body of an answer to an open.
    pub(in crate::server::treefs) fn handle(&self) -> u64 {
        u64::from_ne_bytes(self.body[..8].try_into().unwrap())
    }
}

/// The device end for a process, and the kernel's end for the test.
pub(in crate::server::treefs) fn pair() -> (OwnedFd, FakeDevice) {
    let (device, kernel) = UnixDatagram::pair().unwrap();
    // A test that would wait for a reply that never comes fails instead of hanging.
    kernel
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    (OwnedFd::from(device), FakeDevice { kernel })
}

/// What a handshake of a kernel of Linux 6.9 or later, which offers `resend`, set up.
pub(in crate::server::treefs) fn outcome(resend: bool) -> InitOutcome {
    let flags = if resend {
        InitFlags::FUSE_HAS_RESEND
    } else {
        InitFlags::empty()
    };
    InitOutcome {
        kernel_abi: Version(7, 40),
        capabilities: flags,
        flags,
        max_readahead: 131_072,
        max_write: 1_048_576,
        time_gran: Duration::from_nanos(1),
        max_stack_depth: 0,
    }
}

impl FakeDevice {
    /// Queues a request with the header of the user of the test.
    fn send(&self, opcode: u32, unique: u64, nodeid: u64, body: &[u8]) {
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        let len = u32::try_from(40 + body.len()).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&len.to_ne_bytes());
        bytes.extend_from_slice(&opcode.to_ne_bytes());
        bytes.extend_from_slice(&unique.to_ne_bytes());
        bytes.extend_from_slice(&nodeid.to_ne_bytes());
        bytes.extend_from_slice(&uid.to_ne_bytes());
        bytes.extend_from_slice(&gid.to_ne_bytes());
        bytes.extend_from_slice(&1u32.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(body);
        let _ = self.kernel.send(&bytes).unwrap();
    }

    /// `GETATTR` of `ino`.
    pub(in crate::server::treefs) fn getattr(&self, unique: u64, ino: u64) {
        self.send(GETATTR, unique, ino, &[0; 16]);
    }

    /// `LOOKUP` of `name` in the directory `parent`.
    pub(in crate::server::treefs) fn lookup(&self, unique: u64, parent: u64, name: &str) {
        let mut body = name.as_bytes().to_vec();
        body.push(0);
        self.send(LOOKUP, unique, parent, &body);
    }

    /// `OPEN` of `ino` for reading.
    pub(in crate::server::treefs) fn open(&self, unique: u64, ino: u64) {
        self.send(OPEN, unique, ino, &[0; 8]);
    }

    /// `READ` of `size` bytes at `offset` of the open `fh` of `ino`.
    pub(in crate::server::treefs) fn read(
        &self,
        unique: u64,
        ino: u64,
        (fh, offset, size): (u64, u64, u32),
    ) {
        let mut body = Vec::new();
        body.extend_from_slice(&fh.to_ne_bytes());
        body.extend_from_slice(&offset.to_ne_bytes());
        body.extend_from_slice(&size.to_ne_bytes());
        body.extend_from_slice(&[0; 20]);
        self.send(READ, unique, ino, &body);
    }

    /// `RELEASE` of the open `fh` of `ino`.
    pub(in crate::server::treefs) fn release(&self, unique: u64, ino: u64, fh: u64) {
        let mut body = Vec::new();
        body.extend_from_slice(&fh.to_ne_bytes());
        body.extend_from_slice(&[0; 16]);
        self.send(RELEASE, unique, ino, &body);
    }

    /// The next reply or notification; the test fails if none comes.
    pub(in crate::server::treefs) fn reply(&self) -> Message {
        let mut bytes = vec![0u8; 1 << 20];
        let got = self.kernel.recv(&mut bytes).unwrap();
        let len = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, got);
        Message {
            error: i32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            unique: u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            body: bytes[16..got].to_vec(),
        }
    }

    /// Asserts that no reply is waiting now.
    pub(in crate::server::treefs) fn assert_silent(&self) {
        self.kernel.set_nonblocking(true).unwrap();
        let mut bytes = [0u8; 64];
        let heard = self.kernel.recv(&mut bytes);
        self.kernel.set_nonblocking(false).unwrap();
        assert_eq!(
            heard.unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "a reply is waiting"
        );
    }
}

/// A rig, and a running instance over a stand-in for the kernel, that serves the offer.
pub(in crate::server::treefs) struct Instance {
    pub rig: Rig,
    pub settings: FsSettings,
    pub running: Arc<Running>,
    pub kernel: FakeDevice,
    pub serving: tokio::task::JoinHandle<Result<(), HandoverError>>,
}

pub(in crate::server::treefs) fn instance(
    name: &str,
    wait: Duration,
    uid: Option<u32>,
) -> Instance {
    let rig = Rig::new(name, sample());
    let mut settings = rig.settings();
    settings.handoff_wait = wait;
    let record = Record::first(outcome(false), "mount-1".to_owned());
    init_record::write(&rig.runtime, &record).unwrap();
    let (device, kernel) = pair();
    let prepared = Prepared::load(&settings).unwrap();
    let running = Arc::new(Running::adopt(&settings, prepared, device, (None, false)).unwrap());
    let mut offer = Offer::bind(
        &settings.handoff_socket,
        &settings.mountpoint,
        &settings.control_socket,
        wait,
    )
    .unwrap();
    if let Some(uid) = uid {
        offer = offer.of_user(uid);
    }
    let serving = tokio::spawn(offer.serve(Arc::clone(&running)));
    Instance {
        rig,
        settings,
        running,
        kernel,
        serving,
    }
}

impl Instance {
    pub(in crate::server::treefs) fn paths(&self) -> Paths<'_> {
        Paths {
            socket: &self.settings.handoff_socket,
            mountpoint: &self.settings.mountpoint,
            control: &self.settings.control_socket,
            wait: Duration::from_secs(3600),
        }
    }

    /// Asserts that the running instance still answers the kernel.
    pub(in crate::server::treefs) fn assert_serving(&self, unique: u64) {
        self.kernel.getattr(unique, ino::ROOT);
        let reply = self.kernel.reply();
        assert_eq!((reply.unique, reply.error), (unique, 0));
    }
}
