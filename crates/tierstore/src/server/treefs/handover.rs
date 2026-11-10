//! The hand-over of the mount from a running `tierstore fs` to its successor, `handoff-fs-v1`
//! (`docs/design/virtual-build-fs.md` section 7.3). It is built on the frames of the hand-off of
//! the listening sockets (`server/handoff/wire.rs`) and is a conversation of its own, on the
//! running instance's own socket, `fs.handoff_socket`:
//!
//! ```text
//! new                                                       running
//!  | take-fs 1 ─────────────────────────────────────────▶    |
//!  | ◀── offer-fs / fuse <mountpoint> / control <path>   (+ the /dev/fuse and the control socket)
//!  | the paths are those of its own settings?                |
//!  | the state can be read, the descriptor is in the store   |
//!  | ack ───────────────────────────────────────────────▶    |
//!  |                                  each reader answers the request it holds and stops;
//!  |                                  the control socket stops accepting; the counts are written
//!  | ◀── done                                                |
//!  | serves the connection from the state                    |
//!  | ready ─────────────────────────────────────────────▶    | ends with status 0
//! ```
//!
//! Or `refused` and a reason, for a request not understood or a taker that is not the user of the
//! running instance; `nak` from the taker, at any point before `ack`, leaves the running instance
//! as it was. A taker that is gone before `ready` (or does not send it
//! within `fs.handoff_wait`) has not taken the mount: the running instance reads requests again,
//! accepts on its control socket again and binds its hand-over socket again. The running instance
//! gives up nothing before `done` and the successor holds the descriptor in the service manager's
//! store before `ack`, so at every moment someone holds the connection.
//!
//! The running instance's socket file is removed when it says `done`; the successor binds its own
//! after `ready`. Mode 0600; a taker of another user is refused.

use std::io::ErrorKind;
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener as StdListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};

use crate::server::handoff::wire::{self, Frame};
use crate::server::{ACCEPT_PAUSE, pause_after};

use super::run::Running;

/// What the frames of a conversation are made of.
const TAKE: &str = "take-fs 1";

/// Why a hand-over did not happen.
#[derive(Debug, thiserror::Error)]
pub(in crate::server) enum HandoverError {
    /// A socket could not be used.
    #[error("hand-over: {what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },
    /// The running instance did not take the request.
    #[error("hand-over: the running instance refused: {0}")]
    Refused(String),
    /// The running instance sent something that is not the protocol.
    #[error("hand-over: the running instance sent {0}")]
    Protocol(&'static str),
    /// The running instance serves other paths than these settings name.
    #[error("hand-over: the running instance serves {offered:?}, these settings name {wanted:?}")]
    Mismatch {
        /// What the running instance offered.
        offered: Vec<String>,
        /// What these settings name.
        wanted: Vec<String>,
    },
    /// What the successor had to do before it could take the mount could not be done.
    #[error("hand-over: not taken: {0}")]
    Unprepared(String),
    /// The running instance did not finish in time.
    #[error("hand-over: no end of the hand-over at {} in time", .0.display())]
    Timeout(PathBuf),
    /// The running instance could not serve again after a hand-over that did not complete.
    #[error("hand-over: the instance could not serve again: {0}")]
    Stuck(std::io::Error),
}

impl HandoverError {
    /// A system error, with what was being done.
    fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> Self {
        move |source| Self::Io { what, source }
    }
}

/// Where the paths of the conversation are, and how long it may last.
pub(super) struct Paths<'a> {
    /// The running instance's hand-over socket.
    pub socket: &'a Path,
    /// `fs.mountpoint`.
    pub mountpoint: &'a Path,
    /// `fs.control_socket`.
    pub control: &'a Path,
    /// `fs.handoff_wait`.
    pub wait: Duration,
}

/// The lines of an offer of the mount at `mountpoint` and the control socket at `control`.
fn offer_lines(mountpoint: &Path, control: &Path) -> Vec<String> {
    vec![
        "offer-fs".to_owned(),
        format!("fuse {}", mountpoint.display()),
        format!("control {}", control.display()),
    ]
}

/// The mount taken from a running instance: its descriptors, and the connection to it.
pub(super) struct Taken {
    /// The `/dev/fuse` descriptor.
    pub device: OwnedFd,
    /// The control socket's listening socket.
    pub control: StdListener,
    /// The connection to the running instance.
    pub notice: Notice,
}

/// The connection to the instance that handed the mount over.
pub(super) struct Notice(UnixStream);

impl Notice {
    /// Tells the running instance that this one serves the mount: it ends.
    ///
    /// # Errors
    /// What the system said.
    pub(super) async fn ready(&self) -> Result<(), HandoverError> {
        wire::send(&self.0, &["ready".to_owned()], &[])
            .await
            .map_err(HandoverError::io("telling the running instance it can end"))
    }
}

/// What a taker does with the descriptor before it says `ack`. It is not generic, so that the whole
/// exchange is one function, as the coverage of the branches of a generic one is counted for each
/// instantiation.
pub(super) type Prepare<'a> = Box<dyn FnMut(&OwnedFd) -> Result<(), String> + 'a>;

/// Takes the mount from the instance that answers at `paths.socket`. `prepare` runs once the offer
/// is checked and before the running instance is told to stop: it puts the descriptor in the
/// service manager's store and reads the state the successor starts from, and what it gives is
/// not returned. If it fails the running instance is told `nak` and goes on.
///
/// `Ok(None)` when no instance answers there: there is no file, or a file no one listens on.
///
/// # Errors
/// [`HandoverError`]: the running instance refused or did not keep to the protocol, serves other
/// paths than `paths` names, did not finish within `paths.wait`, or `prepare` failed.
pub(super) async fn take(
    paths: &Paths<'_>,
    prepare: Prepare<'_>,
) -> Result<Option<Taken>, HandoverError> {
    let exchanging = exchange(paths, prepare);
    match tokio::time::timeout(paths.wait, exchanging).await {
        Ok(taken) => taken,
        Err(_) => Err(HandoverError::Timeout(paths.socket.to_owned())),
    }
}

/// The whole exchange with the running instance.
async fn exchange(
    paths: &Paths<'_>,
    mut prepare: Prepare<'_>,
) -> Result<Option<Taken>, HandoverError> {
    let stream = match UnixStream::connect(paths.socket).await {
        Ok(stream) => stream,
        Err(why)
            if matches!(
                why.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(why) => return Err(HandoverError::io("connecting to the hand-over socket")(why)),
    };
    let io = HandoverError::io;
    wire::send(&stream, &[TAKE.to_owned()], &[])
        .await
        .map_err(io("asking for the mount"))?;
    let Frame { lines, mut fds } = wire::recv(&stream).await.map_err(io("reading the offer"))?;
    let (head, rest) = lines
        .split_first()
        .map_or(("", &[][..]), |(h, r)| (h.as_str(), r));
    if head == "refused" {
        return Err(HandoverError::Refused(rest.join(" ")));
    }
    if head != "offer-fs" || fds.len() != 2 {
        return Err(HandoverError::Protocol(
            "something that is not an offer of the mount",
        ));
    }
    let wanted = offer_lines(paths.mountpoint, paths.control);
    if rest != &wanted[1..] {
        nak(&stream).await?;
        return Err(HandoverError::Mismatch {
            offered: rest.to_vec(),
            wanted: wanted[1..].to_vec(),
        });
    }
    let control = fds.remove(1);
    let device = fds.remove(0);
    if let Err(why) = prepare(&device) {
        nak(&stream).await?;
        return Err(HandoverError::Unprepared(why));
    }
    wire::send(&stream, &["ack".to_owned()], &[])
        .await
        .map_err(io("accepting the offer"))?;
    let done = wire::recv(&stream)
        .await
        .map_err(io("waiting for the end of the hand-over"))?;
    if done.lines != ["done"] {
        return Err(HandoverError::Protocol(
            "something other than done after ack",
        ));
    }
    let taken = Taken {
        device,
        control: StdListener::from(control),
        notice: Notice(stream),
    };
    Ok(Some(taken))
}

/// Refuses the offer: the running instance goes on.
async fn nak(stream: &UnixStream) -> Result<(), HandoverError> {
    wire::send(stream, &["nak".to_owned()], &[])
        .await
        .map_err(HandoverError::io("refusing the offer"))
}

/// The running instance's side: its hand-over socket, and the offer of the mount to the one taker
/// at a time that asks.
pub(super) struct Offer {
    /// The socket, or none when it could not be made again after a hand-over that failed.
    listener: Option<UnixListener>,
    path: PathBuf,
    paths: (PathBuf, PathBuf),
    wait: Duration,
    /// The user a taker must be.
    uid: u32,
}

impl Offer {
    /// Makes the socket at `path` (a file left by an instance that is gone is replaced), mode
    /// 0600, to offer the mount at `mountpoint` and the control socket at `control`.
    ///
    /// # Errors
    /// What the system said when the socket cannot be made.
    pub(super) fn bind(
        path: &Path,
        mountpoint: &Path,
        control: &Path,
        wait: Duration,
    ) -> Result<Self, HandoverError> {
        Ok(Self {
            listener: Some(socket(path)?),
            path: path.to_owned(),
            paths: (mountpoint.to_owned(), control.to_owned()),
            wait,
            uid: rustix::process::geteuid().as_raw(),
        })
    }

    /// The same, for a taker of the user `uid`: the tests give another one.
    #[cfg(test)]
    pub(super) fn of_user(mut self, uid: u32) -> Self {
        self.uid = uid;
        self
    }

    /// Answers takers, one at a time, until one has taken the mount; then the socket file is gone
    /// and this returns. A taker that does not complete leaves the instance as it was.
    ///
    /// # Errors
    /// [`HandoverError::Stuck`]: a hand-over that did not complete left the instance unable to
    /// serve again, so that it must end and be started again from the descriptor store.
    pub(super) async fn serve(mut self, running: Arc<Running>) -> Result<(), HandoverError> {
        loop {
            let accepted = match &self.listener {
                Some(listener) => listener.accept().await,
                None => std::future::pending().await,
            };
            let failed = accepted.is_err();
            let said = accepted.map_err(|why| {
                tracing::warn!("tierstore fs: the hand-over socket cannot accept: {why}");
            });
            pause_after(failed, ACCEPT_PAUSE).await;
            if let Ok((stream, _)) = said
                && self.session(&stream, &running).await?
            {
                // The path is the successor's now: the drop must not remove it.
                self.listener = None;
                return Ok(());
            }
        }
    }

    /// One taker's conversation: whether it took the mount.
    async fn session(
        &mut self,
        stream: &UnixStream,
        running: &Arc<Running>,
    ) -> Result<bool, HandoverError> {
        let negotiated = tokio::time::timeout(self.wait, self.negotiate(stream, running)).await;
        match negotiated {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => return Ok(false),
            Ok(Err(why)) => {
                tracing::warn!("tierstore fs: a hand-over request failed: {why}");
                return Ok(false);
            }
            Err(_) => {
                tracing::warn!("tierstore fs: a hand-over request was not completed in time");
                return Ok(false);
            }
        }
        // The taker holds the descriptor in the service manager's store, and has said so.
        let quiescing = Arc::clone(running);
        tokio::task::spawn_blocking(move || quiescing.quiesce())
            .await
            .map_err(std::io::Error::other)
            .map_err(HandoverError::Stuck)?;
        // The successor binds the path once it serves; the file is gone first. A file that is not
        // there is no fault.
        let _ = std::fs::remove_file(&self.path);
        let confirmed = async {
            wire::send(stream, &["done".to_owned()], &[]).await?;
            wire::recv(stream).await
        };
        let ready = tokio::time::timeout(self.wait, confirmed).await;
        if matches!(ready, Ok(Ok(Frame { lines, .. })) if lines == ["ready"]) {
            running.give_up();
            return Ok(true);
        }
        tracing::warn!("tierstore fs: the taker did not say it serves the mount; serving again");
        self.resume(running).await?;
        Ok(false)
    }

    /// Reads requests and accepts clients again, and makes the hand-over socket again.
    async fn resume(&mut self, running: &Arc<Running>) -> Result<(), HandoverError> {
        let again = Arc::clone(running);
        tokio::task::spawn_blocking(move || again.resume())
            .await
            .map_err(std::io::Error::other)
            .and_then(|resumed| resumed)
            .map_err(HandoverError::Stuck)?;
        self.listener = match socket(&self.path) {
            Ok(listener) => Some(listener),
            Err(why) => {
                tracing::warn!("tierstore fs: the hand-over socket is not made again: {why}");
                None
            }
        };
        Ok(())
    }

    /// Reads the request, offers the mount, and returns whether the taker answered `ack`.
    async fn negotiate(
        &self,
        stream: &UnixStream,
        running: &Arc<Running>,
    ) -> std::io::Result<bool> {
        let request = wire::recv(stream).await?;
        if request.lines != [TAKE] {
            return refuse(stream, "the request is not understood").await;
        }
        if stream.peer_cred()?.uid() != self.uid {
            return refuse(stream, "the taker is not the user of this instance").await;
        }
        let (device, control) = running.share()?;
        let lines = offer_lines(&self.paths.0, &self.paths.1);
        wire::send(stream, &lines, &[device.as_fd(), control.as_fd()]).await?;
        Ok(wire::recv(stream).await?.lines == ["ack"])
    }
}

/// Answers `refused` with `why`; the conversation is over and nothing was handed.
async fn refuse(stream: &UnixStream, why: &str) -> std::io::Result<bool> {
    wire::send(stream, &["refused".to_owned(), why.to_owned()], &[]).await?;
    Ok(false)
}

/// A socket at `path`, mode 0600; a file at the path is replaced.
fn socket(path: &Path) -> Result<UnixListener, HandoverError> {
    match std::fs::remove_file(path) {
        Err(why) if why.kind() != ErrorKind::NotFound => {
            return Err(HandoverError::io("removing the old hand-over socket")(why));
        }
        _ => {}
    }
    let listener =
        UnixListener::bind(path).map_err(HandoverError::io("binding the hand-over socket"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(HandoverError::io("restricting the hand-over socket"))?;
    Ok(listener)
}

impl Drop for Offer {
    /// An instance that ends without handing over removes its socket file, so that the next start
    /// finds none; one that handed over has removed it already.
    fn drop(&mut self) {
        if self.listener.is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[path = "handover_tests.rs"]
mod tests;
