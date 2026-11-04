//! The running instance's side: the control socket, and the offer of the listening sockets to the
//! one taker that asks.

use std::net::TcpListener;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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
}

/// The control socket of a running instance, and the sockets it offers.
pub(crate) struct Offer {
    listener: UnixListener,
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
        Ok(Self {
            listener,
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
    pub(crate) async fn serve(self) {
        let mut sessions = FuturesUnordered::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let failed = accepted.is_err();
                    let said = accepted.map_err(|why| {
                        tracing::warn!("tierstore: the hand-off socket cannot accept: {why}");
                    });
                    let accepted = said.ok().into_iter();
                    accepted.for_each(|(stream, _)| sessions.push(self.session(stream)));
                    pause_after(failed, self.accept_pause).await;
                }
                Some(handed) = sessions.next() => {
                    if handed {
                        return;
                    }
                }
            }
        }
    }

    /// One taker's conversation; whether it ended with the sockets handed over.
    async fn session(&self, stream: UnixStream) -> bool {
        match self.converse(&stream).await {
            Ok(handed) => handed,
            Err(why) => {
                tracing::warn!("tierstore: a hand-off request failed: {why}");
                false
            }
        }
    }

    /// Reads the request, offers the sockets, and on `ack` gives up the control socket.
    async fn converse(&self, stream: &UnixStream) -> std::io::Result<bool> {
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
        // if `done` cannot be sent, this instance goes on without one, and the next update's
        // taker finds no file and fails on the ports, which says so.
        self.linked.store(false, Ordering::SeqCst);
        std::fs::remove_file(&self.path)?;
        wire::send(stream, &["done".to_owned()], &[]).await?;
        Ok(true)
    }
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
