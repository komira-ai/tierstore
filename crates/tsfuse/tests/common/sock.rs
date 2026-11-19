//! A fake `/dev/fuse` descriptor: one end of a `SOCK_SEQPACKET` socket pair. Like the device it
//! delivers one request per read and takes one reply per write, and it reads end-of-file once the
//! other end, the "kernel" the test holds, is closed. Several readers are several pairs, as clones
//! are several descriptors of one connection.

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

/// `(error, unique, payload)` of the next message the session wrote.
pub fn read_message(kernel: &Kernel) -> (i32, u64, Vec<u8>) {
    let mut buf = [0u8; 4096];
    let n = kernel.recv(&mut buf).unwrap();
    let len = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    assert_eq!(len as usize, n, "the header's length is the message's");
    let error = i32::from_le_bytes(buf[4..8].try_into().unwrap());
    let unique = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    (error, unique, buf[16..n].to_vec())
}

/// `(error, unique, len)` of the next message the session wrote.
pub fn read_reply(kernel: &Kernel) -> (i32, u64, usize) {
    let (error, unique, payload) = read_message(kernel);
    (error, unique, 16 + payload.len())
}

/// Whether the kernel's end has a message waiting.
pub fn pending(kernel: &Kernel) -> bool {
    kernel.0.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 64];
    let got = kernel.recv(&mut buf).is_ok();
    kernel.0.set_nonblocking(false).unwrap();
    got
}
