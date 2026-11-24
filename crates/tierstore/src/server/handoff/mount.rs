//! The mount in the one hand-over (`docs/design/one-service.md` section 3.1): what an instance that
//! holds it offers and does at each step, what a taker asks for, and the check of the kind of each
//! descriptor a taker is offered.
//!
//! The offer of the mount is two lines and two descriptors after the listening sockets:
//! `fuse <mountpoint>` with the `/dev/fuse` descriptor, and `control <path>` with the control
//! socket's listening socket. A taker of a release before them skips both, as lines of a front it
//! does not know; an instance that holds the mount never offers to a taker that did not ask for it
//! (`mount`, the third line of the request), so the mount is never left with an instance that is
//! about to end.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// A step of the instance that holds the mount, which may block.
pub(crate) type Step = Arc<dyn Fn() + Send + Sync>;

/// What an instance that holds the mount offers of it, and does at each step of a hand-over that
/// takes it.
pub(crate) struct Mount {
    /// The mount point, the `fuse` line.
    pub(crate) mountpoint: PathBuf,
    /// The control socket's path, the `control` line.
    pub(crate) control: PathBuf,
    /// Copies of the `/dev/fuse` descriptor and of the control socket's listening socket.
    pub(crate) share: Arc<dyn Fn() -> io::Result<(OwnedFd, OwnedFd)> + Send + Sync>,
    /// At `ack`: each reader answers the request it holds and stops, the control socket is
    /// paused, no table or attachment is written, the put directory's commits end. From here the
    /// instance reads nothing of the device until [`resume`](Self::resume).
    pub(crate) quiesce: Step,
    /// When the taker did not reach `ready`: the instance reads the device and accepts on the
    /// control socket again. One that cannot serve again ends, so that it is started again from
    /// its descriptor store.
    pub(crate) resume: Step,
    /// At `ready`: the mount is the successor's for good. The instance removes its descriptor
    /// store's entry and leaves the control socket's path, and never reads the device again.
    pub(crate) give_up: Step,
}

/// The lines of an offer of the mount at `mountpoint` and of the control socket at `control`.
pub(super) fn lines(mountpoint: &Path, control: &Path) -> [String; 2] {
    [
        format!("{FUSE} {}", mountpoint.display()),
        format!("{CONTROL} {}", control.display()),
    ]
}

/// The name of the line of the `/dev/fuse` descriptor.
pub(super) const FUSE: &str = "fuse";
/// The name of the line of the control socket.
pub(super) const CONTROL: &str = "control";

/// What a taker does before `ack` with the device of an offer it is about to take: it checks
/// that its runtime state loads and puts the device in its descriptor store. What it says when
/// it cannot is the reason of the `nak`.
pub(crate) type Prepare<'a> = Box<dyn FnMut(&OwnedFd) -> Result<(), String> + Send + 'a>;

/// What a check of the `/dev/fuse` descriptor says: `Ok` for the FUSE device.
pub(crate) type DeviceCheck = fn(BorrowedFd<'_>) -> io::Result<()>;

/// The mount a taker asks for: the paths its own settings name, how long the running instance may
/// take to quiesce, and what it does before it answers `ack`.
pub(crate) struct MountWant<'a> {
    /// The mount point of its settings.
    pub(crate) mountpoint: &'a Path,
    /// The control socket of its settings.
    pub(crate) control: &'a Path,
    /// How long from `ack` to `done`: the running instance's quiesce, which waits for each
    /// reader's request and each commit of the put directory.
    pub(crate) wait: Duration,
    /// The check of the device's kind (the FUSE device, `treefs/handover.rs`).
    pub(crate) device: DeviceCheck,
    /// Run with the device once the offer is checked, before `ack`.
    pub(crate) prepare: Prepare<'a>,
}

/// The mount taken from a running instance.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the start of the one process serves the mount it took (one-service.md section 6, P3)"
    )
)]
pub(crate) struct TakenMount {
    /// The `/dev/fuse` descriptor. The taker reads it only after the running instance's `bye`.
    pub(crate) device: OwnedFd,
    /// The control socket's listening socket.
    pub(crate) control: std::os::unix::net::UnixListener,
}

/// Whether `fd` is a listening stream socket: what a listening socket of an offer, or a control
/// socket, must be.
///
/// # Errors
/// `InvalidData` when it is not.
pub(super) fn check_listening(fd: BorrowedFd<'_>, what: &str) -> io::Result<()> {
    use rustix::net::{SocketType, sockopt};
    // A descriptor that is no socket has no type: the same refusal.
    let stream = sockopt::socket_type(fd).is_ok_and(|kind| kind == SocketType::STREAM);
    if stream && listening(fd) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the descriptor of {what} is not a listening socket"),
        ))
    }
}

/// Whether `fd` is a listening Unix socket: what an offered control socket must be.
///
/// # Errors
/// `InvalidData` when it is not.
pub(crate) fn check_control(fd: BorrowedFd<'_>) -> io::Result<()> {
    use rustix::net::{AddressFamily, getsockname};
    let unix = getsockname(fd).is_ok_and(|addr| addr.address_family() == AddressFamily::UNIX);
    if unix && check_listening(fd, "the control socket").is_ok() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the descriptor of the control socket is not a listening Unix socket",
        ))
    }
}

/// Whether the stream socket `fd` listens.
#[cfg(not(target_vendor = "apple"))]
fn listening(fd: BorrowedFd<'_>) -> bool {
    rustix::net::sockopt::socket_acceptconn(fd).unwrap_or(false)
}

/// Whether the stream socket `fd` listens: macOS has no `SO_ACCEPTCONN`, and a stream socket that
/// listens is the one that has no peer.
#[cfg(target_vendor = "apple")]
fn listening(fd: BorrowedFd<'_>) -> bool {
    rustix::net::getpeername(fd).is_err()
}

#[cfg(test)]
#[path = "mount_tests.rs"]
mod tests;
