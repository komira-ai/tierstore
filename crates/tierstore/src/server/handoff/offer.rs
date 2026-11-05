//! The running instance's side: the control socket, and the offer of the listening sockets to the
//! one taker that asks.

use std::net::TcpListener;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::server::{ACCEPT_PAUSE, pause_after};

use super::wire::{self, MAX_FDS};
use super::{Front, HandoffError, line};

/// A listening socket the instance offers: a copy of the descriptor the front serves from, so
/// that the offer outlives nothing but itself.
pub(crate) struct Socket {
    front: Front,
    listener: TcpListener,
}

impl Socket {
    /// The socket `listener` of `front`.
    pub(crate) fn new(front: Front, listener: TcpListener) -> Self {
        Self { front, listener }
    }

    /// The front of the socket.
    #[cfg(test)]
    pub(crate) fn front(&self) -> Front {
        self.front
    }

    /// The address of the socket.
    #[cfg(test)]
    pub(crate) fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }
}

/// The control socket of a running instance, and the sockets it offers.
pub(crate) struct Offer {
    /// The control socket: replaced when a taker that answered `ack` was gone before `done`, or
    /// none when it could not be made again.
    listener: std::sync::Mutex<Option<Arc<UnixListener>>>,
    path: PathBuf,
    sockets: Vec<Socket>,
    /// The user a taker must be.
    uid: u32,
    /// Held by the taker that is at it; a second taker finds it held and is refused.
    taking: Mutex<()>,
    /// Whether the control socket's file is still this instance's to remove.
    linked: AtomicBool,
    /// How long the loop waits after an accept that failed.
    accept_pause: Duration,
}

impl Offer {
    /// Makes the control socket at `path` (a file left by an instance that is gone is replaced;
    /// the caller has found no one answering there), with mode 0600, and offers `sockets`.
    ///
    /// # Errors
    /// More sockets than a message can carry, or the control socket cannot be made.
    pub(crate) fn bind(path: &Path, sockets: Vec<Socket>) -> Result<Self, HandoffError> {
        if sockets.len() > MAX_FDS {
            return Err(HandoffError::TooMany(sockets.len()));
        }
        let listener = control_socket(path)?;
        Ok(Self {
            listener: std::sync::Mutex::new(Some(Arc::new(listener))),
            path: path.to_owned(),
            sockets,
            uid: rustix::process::geteuid().as_raw(),
            taking: Mutex::new(()),
            linked: AtomicBool::new(true),
            accept_pause: ACCEPT_PAUSE,
        })
    }

    /// The same, waiting `pause` after an accept that failed ([`ACCEPT_PAUSE`] if this is not
    /// called).
    pub(crate) fn with_accept_pause(mut self, pause: Duration) -> Self {
        self.accept_pause = pause;
        self
    }

    /// The same, for a taker of the user `uid`: the tests give another one.
    #[cfg(test)]
    pub(crate) fn of_user(mut self, uid: u32) -> Self {
        self.uid = uid;
        self
    }

    /// Answers takers until one has taken the sockets: then the control socket is gone, and this
    /// returns, and the caller begins its drain. A taker that does not complete (it answers
    /// `nak`, or its connection ends) leaves the instance as it was, ready for the next. An
    /// accept that fails is said and followed by a pause, as the fronts' accept loops do.
    ///
    /// A taker that answered `ack` and was gone before `done` reached it has not taken the
    /// sockets: the instance makes its control socket again and goes on (if it cannot, it says so
    /// and goes on serving with none, so that it is never handed over and never drains for it).
    pub(crate) async fn serve(self) {
        let mut sessions = FuturesUnordered::new();
        loop {
            let control = self.control();
            tokio::select! {
                accepted = accept(control.as_deref()) => {
                    let failed = accepted.is_err();
                    let said = accepted.map_err(|why| {
                        tracing::warn!("tierstore: the hand-off socket cannot accept: {why}");
                    });
                    let accepted = said.ok().into_iter();
                    accepted.for_each(|(stream, _)| sessions.push(self.session(stream)));
                    pause_after(failed, self.accept_pause).await;
                }
                Some(ended) = sessions.next() => match ended {
                    Ended::Handed => {
                        // The path is the successor's now, even if another session made it
                        // this instance's again meanwhile: the drop must not remove it.
                        self.linked.store(false, Ordering::SeqCst);
                        return;
                    }
                    Ended::Kept => {}
                    Ended::Unlinked => self.remake(),
                },
            }
        }
    }

    /// The control socket this instance listens on now, if it has one.
    fn control(&self) -> Option<Arc<UnixListener>> {
        self.listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Makes the control socket again after a taker that was gone before `done`, or says that
    /// it cannot and goes on with none.
    fn remake(&self) {
        let made = match control_socket(&self.path) {
            Ok(listener) => {
                self.linked.store(true, Ordering::SeqCst);
                Some(Arc::new(listener))
            }
            Err(why) => {
                tracing::warn!(
                    "tierstore: warning: the hand-off socket at {} cannot be made again ({why}); \
                     this instance goes on serving and cannot be handed over until it restarts",
                    self.path.display()
                );
                None
            }
        };
        *self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = made;
    }

    /// One taker's conversation, and how it ended.
    async fn session(&self, stream: UnixStream) -> Ended {
        let mut unlinked = false;
        match self.converse(&stream, &mut unlinked).await {
            Ok(true) => Ended::Handed,
            Ok(false) => Ended::Kept,
            Err(why) => {
                tracing::warn!("tierstore: a hand-off request failed: {why}");
                if unlinked {
                    Ended::Unlinked
                } else {
                    Ended::Kept
                }
            }
        }
    }

    /// Reads the request, offers the sockets, and on `ack` gives up the control socket. Sets
    /// `unlinked` once this conversation has removed the control socket's file, so that a
    /// failure after it is told from one before it by this conversation alone.
    async fn converse(&self, stream: &UnixStream, unlinked: &mut bool) -> std::io::Result<bool> {
        let request = wire::recv(stream).await?;
        if request.lines != ["take 1"] {
            return refuse(stream, "the request is not understood").await;
        }
        if stream.peer_cred()?.uid() != self.uid {
            return refuse(stream, "the taker is not the user of this instance").await;
        }
        let Ok(_taking) = self.taking.try_lock() else {
            return refuse(stream, "another taker is taking the sockets").await;
        };
        let mut lines = vec!["offer".to_owned()];
        for socket in &self.sockets {
            lines.push(line(socket.front, socket.listener.local_addr()?));
        }
        let fds: Vec<BorrowedFd<'_>> = self.sockets.iter().map(|s| s.listener.as_fd()).collect();
        wire::send(stream, &lines, &fds).await?;
        if wire::recv(stream).await?.lines != ["ack"] {
            return Ok(false);
        }
        // The taker binds the control socket once it reads `done`, so the file is gone first;
        // if `done` cannot be sent, the taker is gone and took nothing: the session ends in an
        // error with the file unlinked, and `serve` makes the control socket again.
        self.linked.store(false, Ordering::SeqCst);
        if let Err(why) = std::fs::remove_file(&self.path) {
            // The file is still this instance's control socket.
            self.linked.store(true, Ordering::SeqCst);
            return Err(why);
        }
        *unlinked = true;
        wire::send(stream, &["done".to_owned()], &[]).await?;
        Ok(true)
    }
}

/// How one taker's conversation ended.
enum Ended {
    /// The sockets are the taker's: the instance drains.
    Handed,
    /// Nothing changed: the instance goes on with its control socket.
    Kept,
    /// The taker answered `ack` and was gone before `done`: the control socket's file is gone
    /// and must be made again.
    Unlinked,
}

/// The next connection to `control`, or never when there is none.
async fn accept(
    control: Option<&UnixListener>,
) -> std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)> {
    match control {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

/// Checks, before a start asks the running instance anything, that a control socket can be made
/// at `path` once the hand-off is over: its directory is a directory this user may write and
/// search, and the path is not a directory. A file left at the path is no obstacle: it is
/// replaced.
///
/// # Errors
/// [`HandoffError::ControlDirectory`] or [`HandoffError::ControlIsDirectory`].
pub(crate) fn check_control_path(path: &Path) -> Result<(), HandoffError> {
    // The settings hold the path to an absolute path of a file, which has a directory.
    let dir = path.parent().unwrap_or(Path::new("/"));
    let refused = |source| HandoffError::ControlDirectory {
        dir: dir.to_owned(),
        source,
    };
    let meta = std::fs::metadata(dir).map_err(refused)?;
    if !meta.is_dir() {
        return Err(refused(std::io::Error::from(rustix::io::Errno::NOTDIR)));
    }
    let may = rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK;
    rustix::fs::access(dir, may).map_err(|errno| refused(errno.into()))?;
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
        return Err(HandoffError::ControlIsDirectory(path.to_owned()));
    }
    Ok(())
}

/// A control socket at `path`, mode 0600: a file left at the path (by an instance that is gone,
/// or the unlinked name of this one's) is replaced.
///
/// # Errors
/// The file at the path cannot be removed, or the socket cannot be bound or restricted.
fn control_socket(path: &Path) -> Result<UnixListener, HandoffError> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => {}
        Err(why) => return Err(HandoffError::io("removing the old control socket")(why)),
    }
    let listener =
        UnixListener::bind(path).map_err(HandoffError::io("binding the control socket"))?;
    let mode = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, mode)
        .map_err(HandoffError::io("restricting the control socket"))?;
    Ok(listener)
}

/// Answers `refused` with `why`; the conversation is over and nothing was handed.
async fn refuse(stream: &UnixStream, why: &str) -> std::io::Result<bool> {
    wire::send(stream, &["refused".to_owned(), why.to_owned()], &[]).await?;
    Ok(false)
}

impl Drop for Offer {
    /// An instance that ends without handing over removes its control socket, so that the next
    /// start finds no file; one that handed over left the path to its successor.
    fn drop(&mut self) {
        if self.linked.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[path = "offer_tests.rs"]
mod tests;
