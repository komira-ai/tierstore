//! One `/dev/fuse` descriptor of a connection (5.2): the descriptor the mount made, or a clone of
//! it (`FUSE_DEV_IOC_CLONE`). Every descriptor has its own processing queue in the kernel, so a
//! reply is written to the descriptor that read its request: [`DevFd`] is the [`ReplyDevice`] a
//! reader registers its requests with.

use std::io::{self, IoSlice};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags};

use crate::reply::ReplyDevice;

/// The path of the FUSE device.
pub const DEV_FUSE: &str = "/dev/fuse";

/// `FUSE_DEV_IOC_CLONE`: `_IOR(229, 0, uint32_t)` in the pinned `fuse.h`.
const CLONE: rustix::ioctl::Opcode = rustix::ioctl::opcode::read::<u32>(229, 0);

/// A `/dev/fuse` descriptor bound to a connection (or, before a mount, to none).
#[derive(Debug)]
pub struct DevFd {
    fd: OwnedFd,
}

impl DevFd {
    /// Takes `fd` as a FUSE device descriptor: one a mount made, one a supervisor held, or (in
    /// tests) one end of a datagram socket pair, which like the device delivers one message per
    /// read and takes one per write.
    #[must_use]
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Opens `/dev/fuse` for reading and writing, close-on-exec, in blocking mode.
    ///
    /// # Errors
    ///
    /// The `open(2)` error.
    pub fn open() -> io::Result<Self> {
        Self::open_at(Path::new(DEV_FUSE))
    }

    /// Opens the device at `path` (the crate's tests open a missing path).
    pub(crate) fn open_at(path: &Path) -> io::Result<Self> {
        let fd = rustix::fs::open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())?;
        Ok(Self { fd })
    }

    /// A new descriptor of this one's connection, with its own processing queue: `/dev/fuse`
    /// opened again and bound with `FUSE_DEV_IOC_CLONE`. The clone is in blocking mode.
    ///
    /// # Errors
    ///
    /// The `open(2)` error, or the ioctl's when this descriptor is not bound to a connection (no
    /// mount made it, or it is not a FUSE device): `EINVAL` in the upstream source of
    /// `fuse_dev_ioctl_clone`, `EPERM` as measured on the farm's Ubuntu kernel 7.0.0-42.
    pub fn clone_reader(&self) -> io::Result<Self> {
        self.clone_at(Path::new(DEV_FUSE))
    }

    /// [`Self::clone_reader`] with the device opened at `path`.
    pub(crate) fn clone_at(&self, path: &Path) -> io::Result<Self> {
        let new = Self::open_at(path)?;
        let source = u32::try_from(self.fd.as_raw_fd()).map_err(io::Error::other)?;
        clone_ioctl(&new.fd, source)?;
        Ok(new)
    }

    /// Another descriptor of the same open device (`dup(2)`): it shares this one's processing
    /// queue, so it is what a supervisor holds to keep this reader's in-flight requests alive
    /// across a restart (5.2, item 5).
    ///
    /// # Errors
    ///
    /// The `fcntl(F_DUPFD_CLOEXEC)` error.
    pub fn try_clone_fd(&self) -> io::Result<OwnedFd> {
        Ok(rustix::io::fcntl_dupfd_cloexec(&self.fd, 0)?)
    }

    /// Sets or clears `O_NONBLOCK` on the open device (shared with every `dup` of it, not with
    /// clones, which are other open files).
    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        let flags = rustix::fs::fcntl_getfl(&self.fd)?;
        let flags = match on {
            true => flags | OFlags::NONBLOCK,
            false => flags.difference(OFlags::NONBLOCK),
        };
        Ok(rustix::fs::fcntl_setfl(&self.fd, flags)?)
    }

    /// One `read(2)`: one request (the kernel never splits or joins them).
    pub(crate) fn read(&self, buf: &mut [u8]) -> rustix::io::Result<usize> {
        rustix::io::read(&self.fd, buf)
    }
}

/// The one `unsafe` call of the workspace's production code: the kernel offers no other way to
/// bind a second descriptor to a FUSE connection (5.2, "tokio-native dispatch with reply
/// affinity"), and no crate wraps this ioctl safely.
#[expect(
    unsafe_code,
    reason = "FUSE_DEV_IOC_CLONE has no safe wrapper; the call is checked below"
)]
fn clone_ioctl(new: &OwnedFd, source: u32) -> io::Result<()> {
    // SAFETY: `CLONE` is `_IOR(229, 0, uint32_t)` from the pinned uapi header, and the kernel
    // (`fuse_dev_ioctl_clone`) reads exactly one `u32` (`get_user`) through the pointer the
    // setter passes, which points at `source`, alive for the call. `new` is an open descriptor
    // borrowed for the call; the ioctl writes no memory of ours.
    unsafe {
        rustix::ioctl::ioctl(new, rustix::ioctl::Setter::<CLONE, u32>::new(source))?;
    }
    Ok(())
}

impl AsFd for DevFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// The descriptor, for a [`crate::Handover`].
impl From<DevFd> for OwnedFd {
    fn from(dev: DevFd) -> Self {
        dev.fd
    }
}

impl AsRawFd for DevFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl ReplyDevice for DevFd {
    /// One `writev(2)` of every part: the kernel takes a reply only as one write.
    fn send(&self, parts: &[&[u8]]) -> io::Result<()> {
        let slices: Vec<IoSlice<'_>> = parts.iter().map(|p| IoSlice::new(p)).collect();
        let want: usize = parts.iter().map(|p| p.len()).sum();
        let wrote = rustix::io::writev(&self.fd, &slices)?;
        match wrote == want {
            true => Ok(()),
            false => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("FUSE reply of {want} bytes was written short, {wrote} bytes"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "a unit test: a failed unwrap is the failure it reports"
    )]

    use std::os::unix::net::UnixDatagram;

    use super::*;

    #[test]
    fn a_missing_device_is_an_open_error() {
        let err = DevFd::open_at(Path::new("/nonexistent/tsfuse/dev/fuse")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_clone_of_a_device_that_cannot_be_opened_fails_before_the_ioctl() {
        let (a, _b) = UnixDatagram::pair().unwrap();
        let dev = DevFd::from_fd(OwnedFd::from(a));
        let err = dev
            .clone_at(Path::new("/nonexistent/tsfuse/dev/fuse"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_short_write_is_an_error_naming_both_lengths() {
        // A stream socket with a full buffer takes part of a large write.
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let dev = DevFd::from_fd(OwnedFd::from(a));
        assert_eq!(dev.as_fd().as_raw_fd(), dev.as_raw_fd());
        let big = vec![0u8; 16 << 20];
        let err = dev.send(&[&big]).unwrap_err();
        drop(b);
        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
        assert!(
            err.to_string()
                .starts_with("FUSE reply of 16777216 bytes was written short, "),
            "{err}"
        );
    }
}
