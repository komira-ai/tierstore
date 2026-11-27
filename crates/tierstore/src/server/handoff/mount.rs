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
//!
//! An instance of the one process offers its mount on two conversations: this one, and `take-fs 1`
//! on the runtime directory's socket (`treefs/handover.rs`), which a taker of the previous release
//! and the `tierstore fs` of the two-process layout ask (`docs/design/one-service.md` section 4.2).
//! The two share a [`Claim`], so that one taker at a time takes the mount and, once it is given up
//! on either, neither offers it again.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;

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
    /// How long the quiesce from `ack` to `done` and the taker's `ready` after `done` may last
    /// (the `fs` interface's `params.handoff_wait`): a reader may hold a request that waits for a
    /// fill, and the successor makes its state of the mount between `done` and `ready`.
    pub(crate) wait: Duration,
    /// Which conversation has the mount, shared with the instance's `take-fs 1` offer.
    pub(crate) claim: Arc<Claim>,
}

/// Where the mount of an instance is, as the conversations that offer it see it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Holding {
    /// The instance holds it, and no taker is at it.
    #[default]
    Here,
    /// A taker of one of the conversations is at it: it is offered, or quiesced for that taker.
    Taking,
    /// It was given up: a successor has it for good.
    Gone,
}

/// Which of the conversations that offer an instance's mount has it: one taker at a time takes it,
/// and once it is given up on one of them, neither offers it again.
#[derive(Debug, Default)]
pub(crate) struct Claim {
    holding: Mutex<Holding>,
    gone: Notify,
}

impl Claim {
    /// Where the mount is now.
    pub(crate) fn holding(&self) -> Holding {
        *self.holding.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claims the mount for one taker: where it was, and it is [`Holding::Taking`] now only if it
    /// was [`Holding::Here`].
    pub(crate) fn claim(&self) -> Holding {
        let mut holding = self.holding.lock().unwrap_or_else(PoisonError::into_inner);
        let was = *holding;
        if was == Holding::Here {
            *holding = Holding::Taking;
        }
        was
    }

    /// The taker that claimed the mount did not take it: the instance holds it again.
    pub(crate) fn release(&self) {
        let mut holding = self.holding.lock().unwrap_or_else(PoisonError::into_inner);
        if *holding == Holding::Taking {
            *holding = Holding::Here;
        }
    }

    /// The mount was given up: a successor has it for good.
    pub(crate) fn give_up(&self) {
        *self.holding.lock().unwrap_or_else(PoisonError::into_inner) = Holding::Gone;
        self.gone.notify_waiters();
    }

    /// Waits until the mount is given up, on either conversation.
    pub(crate) async fn given_up(&self) {
        loop {
            // Made before the look, so that a give-up between the two is not missed.
            let told = self.gone.notified();
            if self.holding() == Holding::Gone {
                return;
            }
            told.await;
        }
    }
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

/// Whether the stream socket `fd` listens. macOS answers no `getsockopt` of `SO_ACCEPTCONN`, so
/// the socket's peer is asked for: a listening socket has none and was never connected, and macOS
/// says `ENOTCONN` of it. A socket whose peer is gone (the other end of a pair closed, a
/// connection ended) has both its sides shut, and macOS says `EINVAL` of it instead; a connected
/// one names its peer. A stream socket that is bound and neither listens nor ever connected also
/// says `ENOTCONN`: no image or instance offers one, as every socket of an offer is one it serves.
#[cfg(target_vendor = "apple")]
fn listening(fd: BorrowedFd<'_>) -> bool {
    rustix::net::getpeername(fd).err() == Some(rustix::io::Errno::NOTCONN)
}

#[cfg(test)]
#[path = "mount_tests.rs"]
mod tests;
