//! The hand-off of the listening sockets from a running instance to its successor, over a Unix
//! control socket.
//!
//! An instance that is given a [`Handoff`] listens on its control socket (a file, in the state
//! directory). A new instance of the same settings, started beside it, connects there and asks
//! for the sockets:
//!
//! 1. The taker sends `take 1`.
//! 2. The running instance answers `offer`, with a line `<front> <address>` for each listening
//!    socket and the sockets themselves (`SCM_RIGHTS`) on the same message, or `refused` and
//!    the reason: another taker is at it, the taker is another user, or the request is not
//!    understood.
//! 3. The taker checks that the addresses are the ones its own settings name, and answers `ack`
//!    (or `nak`, and the running instance goes on as if nothing had been asked).
//! 4. On `ack` the running instance gives up its control socket (the file is removed), says
//!    `done`, and begins its ordinary drain, as if its stop had resolved: from then on every
//!    answer says `Connection: close`, and it goes on accepting on the sockets it passed (the
//!    taker holds them too, so the kernel resets nothing) through `--drain-lb-wait`, then takes
//!    no new connection, finishes the requests in flight, and ends.
//! 5. After `done` the taker binds the control socket itself, so the next update asks it.
//!
//! A taker that serves sockets which nothing else can serve for it (`tierstore fs` on macOS, the
//! `nfs` front) asks for one more step, so that a taker that dies after `done` and before it
//! serves leaves the running instance serving: its request is `take 1` and a second line,
//! `ready`. The running instance, if it takes part (it is given a wait), does what it did, but
//! after `done` it has handed nothing over until the taker sends the frame `ready`: it keeps its
//! sockets and goes on serving through that time. If the taker ends the connection, sends
//! another frame, or says nothing for the wait, the running instance makes its control socket
//! again, lets go of the hold it took at `ack`, and serves on, as for a taker that was gone
//! before `done`. A running instance that does not know the step (a release before it, or one
//! given no wait) answers `refused` with "the request is not understood", and the taker asks
//! again with `take 1` alone and no `ready`; a taker of that release that asks with `take 1`
//! alone is handed over as before, with no step. So the step is additive: no release's request
//! or answer changes meaning.
//!
//! No port is added, no packet is redirected and no right beyond the user's own is needed. The
//! control socket is mode 0600 and a peer of another user is refused. Where nothing answers
//! (no file, or a file no one listens on) the taker starts cold, binding its sockets itself.
//! One that is not answered within [`Handoff::wait`] does not: the running instance may be
//! alive, so the start fails.
//!
//! A line that names a front this release does not know is skipped, with its descriptor closed,
//! so that a front added later (as `nfs` was) is no protocol error for a taker that predates it.
//!
//! The same code runs on Linux and macOS: `SCM_RIGHTS` and Unix sockets are POSIX.

mod offer;
mod take;
pub(crate) mod wire;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) use offer::{Hold, Offer, Socket, check_control_path};

/// The most listening sockets one instance can hand over: what one message carries.
pub(crate) const MAX_SOCKETS: usize = wire::MAX_FDS;
pub(crate) use take::{Inherited, Want, take, take_with_ready};

/// What an instance is told to do about a hand-off: where the control socket is, and how long a
/// taker waits for the running instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handoff {
    /// The control socket: a file in a directory only the instance's user may write.
    pub socket: PathBuf,
    /// How long taking the sockets of a running instance may last, from the connect to the
    /// running instance's `done`.
    pub wait: Duration,
}

/// The front a listening socket belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Front {
    /// The S3 front.
    S3,
    /// The OCI read front.
    Oci,
    /// The NFS front of `tierstore fs` (macOS).
    Nfs,
}

impl Front {
    /// The name the frames give the front.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Oci => "oci",
            Self::Nfs => "nfs",
        }
    }

    /// The front a frame's `name` names.
    pub(super) fn named(name: &str) -> Option<Self> {
        match name {
            "s3" => Some(Self::S3),
            "oci" => Some(Self::Oci),
            "nfs" => Some(Self::Nfs),
            _ => None,
        }
    }
}

/// Why a hand-off did not happen.
#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    /// The control socket or a socket could not be used.
    #[error("hand-off: {what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },
    /// The running instance did not take the request.
    #[error("hand-off: the running instance refused: {0}")]
    Refused(String),
    /// The running instance answered with something that is not the protocol.
    #[error("hand-off: the running instance sent {0}")]
    Protocol(&'static str),
    /// The running instance listens on other addresses than these settings name, so it cannot be
    /// replaced live.
    #[error(
        "hand-off: the running instance listens on {offered:?}, these settings name {wanted:?}"
    )]
    Mismatch {
        /// What the running instance offered, as `<front> <address>`.
        offered: Vec<String>,
        /// What these settings name, as `<front> <address>`.
        wanted: Vec<String>,
    },
    /// The running instance did not finish the hand-off in time.
    #[error("hand-off: no end of the hand-off at {} in time", .0.display())]
    Timeout(PathBuf),
    /// More listening sockets than a message can carry.
    #[error("hand-off: {0} sockets are more than a message can carry")]
    TooMany(usize),
    /// The directory of the control socket is missing, is not a directory, or may not be
    /// written by this user, so the control socket could not be made there.
    #[error("hand-off: the directory of the control socket {}: {source}", .dir.display())]
    ControlDirectory {
        /// The directory.
        dir: PathBuf,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },
    /// The path of the control socket is a directory, which the control socket cannot replace.
    #[error("hand-off: the control socket {}: is a directory, not a socket", .0.display())]
    ControlIsDirectory(PathBuf),
}

impl HandoffError {
    /// A system error, with what was being done.
    pub(super) fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> Self {
        move |source| Self::Io { what, source }
    }
}

/// The `<front> <address>` line of a socket.
pub(super) fn line(front: Front, addr: SocketAddr) -> String {
    format!("{} {addr}", front.name())
}

#[cfg(test)]
mod live_tests;
#[cfg(test)]
pub(crate) mod testing;
