//! A fake device shared by the tsfuse tests.
#![allow(dead_code, reason = "each test binary uses a part of the helpers")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tsfuse::ReplyDevice;

/// A decoded reply message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
    pub payload: Vec<u8>,
}

/// Records every reply as the concatenation of its parts; can be told to fail writes.
#[derive(Default)]
pub struct FakeDevice {
    msgs: Mutex<Vec<Vec<u8>>>,
    fail: AtomicBool,
}

impl FakeDevice {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn failing() -> Arc<Self> {
        let d = Self::new();
        d.fail.store(true, Ordering::SeqCst);
        d
    }

    pub fn raw(&self) -> Vec<Vec<u8>> {
        self.msgs.lock().unwrap().clone()
    }

    pub fn msgs(&self) -> Vec<Msg> {
        self.raw()
            .into_iter()
            .map(|m| Msg {
                len: u32::from_le_bytes(m[0..4].try_into().unwrap()),
                error: i32::from_le_bytes(m[4..8].try_into().unwrap()),
                unique: u64::from_le_bytes(m[8..16].try_into().unwrap()),
                payload: m[16..].to_vec(),
            })
            .collect()
    }
}

impl ReplyDevice for FakeDevice {
    fn send(&self, parts: &[&[u8]]) -> io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::other("device gone"));
        }
        self.msgs.lock().unwrap().push(parts.concat());
        Ok(())
    }
}

/// A request: `fuse_in_header` (uid, gid and pid 0) then `body`.
pub fn request(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
    let header = tsfuse::abi::InHeader {
        len: (tsfuse::abi::InHeader::SIZE + body.len()) as u32,
        opcode,
        unique,
        nodeid,
        ..tsfuse::abi::InHeader::default()
    };
    [header.as_bytes(), body].concat()
}

/// An INIT request (unique 1) of protocol `major.minor` offering `flags` (the high half in
/// `flags2`), with a readahead of 64 KiB.
pub fn init_request(major: u32, minor: u32, flags: u64) -> Vec<u8> {
    let init = tsfuse::abi::InitIn {
        major,
        minor,
        max_readahead: 65536,
        flags: flags as u32,
        flags2: (flags >> 32) as u32,
        ..tsfuse::abi::InitIn::default()
    };
    request(26, 1, 0, init.as_bytes())
}

/// A GETATTR of the root.
pub fn getattr(unique: u64) -> Vec<u8> {
    request(3, unique, 1, &[0; 16])
}

/// A fake `/dev/fuse` descriptor: one end of a `SOCK_SEQPACKET` socket pair, which like the
/// device delivers one request per read and takes one reply per write, and reads end-of-file once
/// the other end, the "kernel" the test holds, is closed.
#[cfg(target_os = "linux")]
pub mod fake {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

    use tsfuse::DevFd;

    /// The kernel's end of a fake device.
    pub struct Kernel(pub UnixStream);

    impl Kernel {
        /// Queues one request.
        pub fn send(&self, bytes: &[u8]) -> std::io::Result<()> {
            let n = (&self.0).write(bytes)?;
            assert_eq!(n, bytes.len(), "a packet is written whole");
            Ok(())
        }

        /// Takes the next reply.
        pub fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            (&self.0).read(buf)
        }
    }

    /// A device and the kernel's end of it.
    pub fn fake_device() -> (DevFd, Kernel) {
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

    /// `(error, unique, len)` of the next message the session wrote.
    pub fn read_reply(kernel: &Kernel) -> (i32, u64, usize) {
        let mut buf = [0u8; 4096];
        let n = kernel.recv(&mut buf).unwrap();
        let len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(len as usize, n, "the header's length is the message's");
        let error = i32::from_le_bytes(buf[4..8].try_into().unwrap());
        let unique = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        (error, unique, n)
    }

    /// Whether the kernel's end has a message waiting (not the end-of-file of a device whose
    /// every descriptor is closed).
    pub fn pending(kernel: &Kernel) -> bool {
        kernel.0.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 64];
        let got = kernel.recv(&mut buf).is_ok_and(|n| n > 0);
        kernel.0.set_nonblocking(false).unwrap();
        got
    }
}
