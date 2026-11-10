//! The service manager's side of `tierstore fs` (systemd's `sd_notify` protocol and descriptor
//! store, `docs/design/virtual-build-fs.md` section 7.2): messages to `NOTIFY_SOCKET`, the
//! descriptors the manager hands a restarted process (`LISTEN_PID`, `LISTEN_FDS`,
//! `LISTEN_FDNAMES`), and taking one of those descriptors as an owned one.
//!
//! The variables are set by the service manager for a unit of `Type=notify` with a descriptor
//! store: platform-set markers, not configuration (AGENTS.md S2), read in [`environment`] and
//! nowhere else. Nothing here is `unsafe`: the messages are `sendmsg` with `SCM_RIGHTS`
//! (`rustix`), and a descriptor the manager passed is taken by `pidfd_getfd` on this process
//! itself, which makes an owned duplicate of it without a raw-descriptor constructor.

use std::ffi::{OsStr, OsString};
use std::io::{self, IoSlice, Read as _};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::Path;

use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
use rustix::pipe::{PipeFlags, pipe_with};
use rustix::process::{PidfdFlags, PidfdGetfdFlags, getpid, pidfd_getfd, pidfd_open};

/// The name the descriptor of the FUSE connection is stored under.
pub(super) const FUSE_NAME: &str = "fuse";

/// The first descriptor the service manager passes (`SD_LISTEN_FDS_START`).
const LISTEN_FDS_START: RawFd = 3;

/// The most descriptors one message carries to the manager: the kernel's limit per message.
const MOST_FDS: usize = 253;

/// What the service manager put in the environment of the process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Environment {
    /// `NOTIFY_SOCKET`: the datagram socket to send to; a leading `@` is an abstract name.
    pub notify_socket: Option<OsString>,
    /// `LISTEN_PID`: the process the descriptors are for.
    pub listen_pid: Option<String>,
    /// `LISTEN_FDS`: how many descriptors there are, from 3.
    pub listen_fds: Option<String>,
    /// `LISTEN_FDNAMES`: their names, separated by colons.
    pub listen_fdnames: Option<String>,
}

/// A descriptor the service manager passed, with the name it was stored under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stored {
    /// The `FDNAME` it was stored under (`unknown` when the manager gave no names).
    pub name: String,
    /// Its number in this process.
    pub fd: RawFd,
}

/// The variables of the service manager, read once.
#[expect(
    clippy::disallowed_methods,
    reason = "NOTIFY_SOCKET and LISTEN_* are set by the service manager for a Type=notify unit with a descriptor store, platform-set markers (AGENTS.md S2), not configuration"
)]
pub(super) fn environment() -> Environment {
    Environment::read(|name| std::env::var_os(name))
}

impl Environment {
    /// The variables as `get` gives them: a value that is not text is none.
    fn read(get: impl Fn(&str) -> Option<OsString>) -> Self {
        let text = |name: &str| get(name).and_then(|value| value.into_string().ok());
        Self {
            notify_socket: get("NOTIFY_SOCKET"),
            listen_pid: text("LISTEN_PID"),
            listen_fds: text("LISTEN_FDS"),
            listen_fdnames: text("LISTEN_FDNAMES"),
        }
    }

    /// The descriptors the manager passed to the process `pid`: none when `LISTEN_PID` names
    /// another process (the variables were inherited by a child of the unit's process, which
    /// they are not for) or is missing, or `LISTEN_FDS` is not a number.
    pub(super) fn stored(&self, pid: u32) -> Vec<Stored> {
        let for_us = self
            .listen_pid
            .as_deref()
            .and_then(|text| text.parse().ok())
            == Some(pid);
        let count: usize = self
            .listen_fds
            .as_deref()
            .and_then(|text| text.parse().ok())
            .unwrap_or(0);
        if !for_us {
            return Vec::new();
        }
        let names: Vec<&str> = self
            .listen_fdnames
            .as_deref()
            .map_or_else(Vec::new, |text| text.split(':').collect());
        (0..count)
            .filter_map(|index| {
                Some(Stored {
                    name: (*names.get(index).unwrap_or(&"unknown")).to_owned(),
                    fd: LISTEN_FDS_START.checked_add(i32::try_from(index).ok()?)?,
                })
            })
            .collect()
    }

    /// The descriptors passed to this process.
    pub(super) fn stored_here(&self) -> Vec<Stored> {
        self.stored(getpid().as_raw_nonzero().get().unsigned_abs())
    }
}

/// An owned duplicate of the descriptor number `fd` of this process, which the service manager
/// passed. The original stays open (a process that is not going to exec anything loses nothing by
/// that).
///
/// # Errors
/// What the system said: `fd` is not open, or the system call is not allowed to the process.
pub(super) fn take(fd: RawFd) -> io::Result<OwnedFd> {
    let this = pidfd_open(getpid(), PidfdFlags::empty())?;
    Ok(pidfd_getfd(&this, fd, PidfdGetfdFlags::empty())?)
}

/// The channel to the service manager.
#[derive(Clone, Debug, Default)]
pub(super) struct Manager {
    socket: Option<OsString>,
}

impl Manager {
    /// The manager that listens at `socket` (the value of `NOTIFY_SOCKET`), or none.
    pub(super) fn new(socket: Option<&OsStr>) -> Self {
        Self {
            socket: socket.map(OsStr::to_owned),
        }
    }

    /// Whether there is a manager to tell.
    pub(super) fn present(&self) -> bool {
        self.socket.is_some()
    }

    /// Sends `message` (lines of `KEY=value`) to the manager, with `fds` attached. Nothing is sent
    /// where the process was not started by systemd with `Type=notify`.
    ///
    /// # Errors
    /// What the system said when the socket cannot be reached or written.
    pub(super) fn send(&self, message: &str, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        let Some(socket) = &self.socket else {
            return Ok(());
        };
        let datagram = connected(socket)?;
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MOST_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
            return Err(io::Error::other("too many descriptors for one message"));
        }
        let iov = [IoSlice::new(message.as_bytes())];
        let _ = sendmsg(&datagram, &iov, &mut control, SendFlags::empty())?;
        Ok(())
    }

    /// Tells the manager that the unit is ready.
    ///
    /// # Errors
    /// As [`Manager::send`].
    pub(super) fn ready(&self) -> io::Result<()> {
        self.send("READY=1", &[])
    }

    /// Gives the manager `fd` to keep for the unit under `name`, replacing one stored under it.
    ///
    /// # Errors
    /// As [`Manager::send`].
    pub(super) fn store(&self, name: &str, fd: BorrowedFd<'_>) -> io::Result<()> {
        self.send(&format!("FDSTORE=1\nFDNAME={name}"), &[fd])
    }

    /// Takes the descriptors stored under `name` out of the store.
    ///
    /// # Errors
    /// As [`Manager::send`].
    pub(super) fn unstore(&self, name: &str) -> io::Result<()> {
        self.send(&format!("FDSTOREREMOVE=1\nFDNAME={name}"), &[])
    }

    /// Returns when the manager has handled every message sent before this call: it is sent a
    /// pipe with `BARRIER=1`, and closes it when it gets there.
    ///
    /// # Errors
    /// As [`Manager::send`], and what the system said when the pipe cannot be waited on.
    pub(super) fn barrier(&self) -> io::Result<()> {
        if self.socket.is_none() {
            return Ok(());
        }
        let (read, write) = pipe_with(PipeFlags::CLOEXEC)?;
        self.send("BARRIER=1", &[write.as_fd()])?;
        drop(write);
        // The manager closes its end of the pipe when it gets there: the read then ends.
        std::fs::File::from(read).read_to_end(&mut Vec::new())?;
        Ok(())
    }
}

/// A datagram socket connected to the manager's: a path, or an abstract name after an `@`.
fn connected(socket: &OsStr) -> io::Result<UnixDatagram> {
    use std::os::linux::net::SocketAddrExt as _;
    use std::os::unix::ffi::OsStrExt as _;
    let datagram = UnixDatagram::unbound()?;
    match socket.as_bytes().strip_prefix(b"@") {
        Some(name) => {
            let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            datagram.connect_addr(&address)?;
        }
        None => datagram.connect(Path::new(socket))?,
    }
    Ok(datagram)
}

#[cfg(test)]
#[path = "sd_tests.rs"]
mod tests;
