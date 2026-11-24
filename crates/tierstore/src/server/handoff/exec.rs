//! The hand-over to the next image of the same process (`docs/design/one-service.md` section 3.4):
//! how a Mac, whose one launchd job has one process, updates without a second instance.
//!
//! The process that hands over has drained its fronts and let its cache writes end. It writes the
//! offer of the one hand-over (`offer`, then a `<front> <address>` line for each listening socket,
//! `control <path>` for the file system's control socket, `lock <path>` for the cache lock, each
//! line with its descriptor) into one end of a `socketpair`, as one frame of [`wire`], and executes
//! its own program path with its own arguments and `--handed 0` ([`Image`]). The other end of the
//! pair is the new image's standard input, descriptor 0: every other descriptor of the process is
//! close-on-exec, so it is the one that survives the exec; the descriptors of the offer ride in
//! it, and so does the `flock` of the cache lock, which belongs to the open file description.
//!
//! The new image reads the frame from its standard input ([`handed_input`], [`read`]) with the
//! taker's checks: the addresses cover its own settings, the control socket and the lock are at
//! its own paths, each descriptor is of its kind. There is no `ack`, `done`, `ready` or `bye`: the
//! image that wrote the offer is gone. An offer the new image refuses is closed whole, descriptor
//! by descriptor, which frees the sockets and the lock; the start fails, launchd starts the job
//! again (without `--handed`), and that start binds the same addresses itself.
//!
//! Descriptor 0 is where the offer rides because a descriptor number from the command line can be
//! made a descriptor only by `unsafe` code, which this crate does not have (AGENTS.md R2); the
//! standard library places a descriptor at 0 for an exec (`Command::stdin`), and standard input
//! can be read, and then pointed at `/dev/null` again, without it.

use std::ffi::OsString;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustix::io::{FdFlags, fcntl_setfd};
use tokio::net::UnixStream;

use super::mount::{CONTROL, check_control};
use super::offer::LOCK;
use super::take::{Parts, adopt, duplicate, take_lock};
use super::wire::{self, Frame, MAX_FDS};
use super::{HandoffError, Inherited, Socket, Want, line};

/// The flag that tells a start it is the next image of a process that handed over, and on which
/// descriptor the offer is.
pub(crate) const HANDED: &str = "--handed";

/// The descriptor the offer is handed on: standard input. `--handed` names no other.
pub(crate) const HANDED_FD: &str = "0";

/// What an image hands over to the next.
pub(crate) struct Handing {
    /// The listening sockets, in the order the fronts serve them.
    pub(crate) sockets: Vec<Socket>,
    /// The file system's control socket: its path and a copy of its listening socket.
    pub(crate) control: Option<(PathBuf, OwnedFd)>,
    /// The cache lock: its path and a descriptor of its open file description.
    pub(crate) lock: Option<(PathBuf, OwnedFd)>,
}

impl Handing {
    /// The lines of the offer and the descriptors that go with them, in order.
    fn frame(&self) -> std::io::Result<(Vec<String>, Vec<BorrowedFd<'_>>)> {
        let mut lines = vec!["offer".to_owned()];
        let mut fds = Vec::new();
        for socket in &self.sockets {
            lines.push(line(socket.front, socket.listener.local_addr()?));
            fds.push(socket.listener.as_fd());
        }
        for (name, held) in [(CONTROL, &self.control), (LOCK, &self.lock)] {
            if let Some((path, fd)) = held {
                lines.push(format!("{name} {}", path.display()));
                fds.push(fd.as_fd());
            }
        }
        Ok((lines, fds))
    }
}

/// The next image of this process, its offer written: the program to execute, its arguments, and
/// the end of the `socketpair` it reads the offer from.
#[derive(Debug)]
pub(crate) struct Image {
    program: PathBuf,
    args: Vec<OsString>,
    handed: OwnedFd,
}

impl Image {
    /// Writes the offer of `handing` into one end of a new `socketpair` and closes that end: the
    /// frame and its descriptors wait in the other end, the image's, for the next image to read.
    /// `program` is what is executed, and `args` (the program name first, as `std::env::args_os`
    /// gives them) its arguments, less a `--handed` they hold, and with `--handed 0`.
    ///
    /// # Errors
    /// More descriptors than a message carries ([`HandoffError::TooMany`]); the pair cannot be
    /// made or written ([`HandoffError::Io`]).
    pub(crate) async fn prepare(
        handing: &Handing,
        program: PathBuf,
        args: impl IntoIterator<Item = OsString>,
    ) -> Result<Self, HandoffError> {
        let io = HandoffError::io;
        let (lines, fds) = handing.frame().map_err(io("naming the handed sockets"))?;
        if fds.len() > MAX_FDS {
            return Err(HandoffError::TooMany(fds.len()));
        }
        // Both ends are close-on-exec; `exec` places the image's at descriptor 0, which is not.
        let (near, far) =
            std::os::unix::net::UnixStream::pair().map_err(io("making the hand-over's pair"))?;
        near.set_nonblocking(true)
            .map_err(io("making the hand-over's pair"))?;
        let near = UnixStream::from_std(near).map_err(io("making the hand-over's pair"))?;
        wire::send(&near, &lines, &fds)
            .await
            .map_err(io("writing the offer to the next image"))?;
        Ok(Self {
            program,
            args: next_args(args),
            handed: far.into(),
        })
    }

    /// Executes the next image: this process's program path, with the arguments of
    /// [`prepare`](Self::prepare), and the end of the pair as its standard input. It returns only
    /// when the exec failed.
    pub(crate) fn exec(self) -> std::io::Error {
        self.command().exec()
    }

    /// The command [`exec`](Self::exec) executes: the program, its arguments, and the end of the
    /// pair as its standard input, which the standard library places at descriptor 0, not
    /// close-on-exec; every other descriptor of this process is close-on-exec.
    fn command(self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args).stdin(Stdio::from(self.handed));
        command
    }

    /// The program the image is.
    #[cfg(test)]
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    /// The arguments it is given.
    #[cfg(test)]
    pub(crate) fn args(&self) -> &[OsString] {
        &self.args
    }

    /// The end of the pair it reads the offer from.
    #[cfg(test)]
    pub(crate) fn into_handed(self) -> OwnedFd {
        self.handed
    }

    /// The number of that end in this process.
    #[cfg(test)]
    fn handed_number(&self) -> i32 {
        use std::os::fd::AsRawFd as _;
        self.handed.as_raw_fd()
    }
}

/// The arguments of the next image: `args` without the program name and without a `--handed`
/// (with its value, as `--handed 0` or `--handed=0`) a hand-over before this one gave, then
/// `--handed 0`.
fn next_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut next = Vec::new();
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == HANDED {
            let _ = args.next();
        } else if !arg
            .to_str()
            .is_some_and(|text| text.starts_with(&format!("{HANDED}=")))
        {
            next.push(arg);
        }
    }
    next.extend([HANDED, HANDED_FD].map(OsString::from));
    next
}

/// What a start's own settings name of what an offer may hold besides the sockets.
pub(crate) struct Own<'a> {
    /// The file system's control socket, when the start serves one.
    pub(crate) control: Option<&'a Path>,
    /// The cache lock, when the start has a cache.
    pub(crate) lock: Option<&'a Path>,
}

/// What the next image took from the offer it was handed.
pub(crate) struct Handed {
    /// The listening sockets, by front, with their addresses and copies; and the cache lock, when
    /// it was offered and the image has a cache: it is the lock file of [`Own::lock`].
    pub(crate) inherited: Inherited,
    /// The control socket's listening socket, when it was offered: it is bound at
    /// [`Own::control`].
    pub(crate) control: Option<std::os::unix::net::UnixListener>,
}

/// The descriptor of standard input, where the image that executed this one left its offer, with
/// standard input itself pointed at `/dev/null` again (what launchd gives a job).
///
/// # Errors
/// What the system said of the copy, of `/dev/null` or of the `dup2`.
pub(crate) fn handed_input() -> std::io::Result<OwnedFd> {
    let handed = std::io::stdin().as_fd().try_clone_to_owned()?;
    let null = std::fs::File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(&null)?;
    Ok(handed)
}

/// Reads the offer handed on `handed` and takes it, with the checks of a taker: the sockets must be
/// the ones `want` names (or, for a port of 0, on its hosts), a control socket must be at
/// `own.control` and listen, the lock must be the file at `own.lock`; a lock offered to an image
/// without a cache is closed. Every descriptor taken is made close-on-exec, and so is `handed`.
///
/// # Errors
/// What makes the image refuse the offer; every descriptor it came with is closed then:
/// [`HandoffError::Mismatch`] for other addresses or paths, [`HandoffError::Protocol`] for a frame
/// that is not an offer or holds a line no image is handed (a FUSE device), [`HandoffError::Io`]
/// for a descriptor not of its kind or a frame that cannot be read.
pub(crate) async fn read(
    handed: OwnedFd,
    want: &Want<'_>,
    own: &Own<'_>,
) -> Result<Handed, HandoffError> {
    let reading = HandoffError::io("reading the handed offer");
    let frame = async {
        fcntl_setfd(&handed, FdFlags::CLOEXEC)?;
        let stream = std::os::unix::net::UnixStream::from(handed);
        stream.set_nonblocking(true)?;
        wire::recv(&UnixStream::from_std(stream)?).await
    };
    let Frame { lines, fds } = frame.await.map_err(reading)?;
    if lines.first().map(String::as_str) != Some("offer") {
        return Err(HandoffError::Protocol(
            "a handed frame that is not an offer",
        ));
    }
    let rest = &lines[1..];
    let mut parts = Parts::of(rest, fds)?;
    parts.cover(want, rest)?;
    let control = control_of(&mut parts, own.control)?;
    let lock = take_lock(&mut parts, own.lock)?;
    if !parts.named.is_empty() {
        return Err(HandoffError::Protocol(
            "a handed offer of a FUSE device, which no image is handed",
        ));
    }
    let mut inherited = adopt(parts.sockets.into_iter(), duplicate)
        .map_err(HandoffError::io("taking over a handed socket"))?;
    inherited.lock = lock;
    Ok(Handed { inherited, control })
}

/// The control socket of an offer, when it holds one: it must be at `own`, the image's own, and be
/// a listening Unix socket.
///
/// # Errors
/// An offer of a control socket at another path, or to an image that serves none
/// ([`HandoffError::Mismatch`]); a descriptor that is not a listening Unix socket.
fn control_of(
    parts: &mut Parts,
    own: Option<&Path>,
) -> Result<Option<std::os::unix::net::UnixListener>, HandoffError> {
    let Some((offered, fd)) = parts.take(CONTROL) else {
        return Ok(None);
    };
    let offered = format!("{CONTROL} {offered}");
    let wanted = own.map(|path| format!("{CONTROL} {}", path.display()));
    if wanted.as_ref() != Some(&offered) {
        return Err(HandoffError::Mismatch {
            offered: vec![offered],
            wanted: wanted.into_iter().collect(),
        });
    }
    let checked = check_control(fd.as_fd()).and_then(|()| Ok(fcntl_setfd(&fd, FdFlags::CLOEXEC)?));
    checked.map_err(HandoffError::io("checking the handed control socket"))?;
    Ok(Some(fd.into()))
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
