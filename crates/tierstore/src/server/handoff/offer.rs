//! The running instance's side: the control socket, and the offer of the listening sockets (with
//! the mount and the cache lock, where it holds them) to the one taker that asks.

use std::net::TcpListener;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, MutexGuard};

use crate::server::local_cache::LocalCache;
use crate::server::{ACCEPT_PAUSE, pause_after};

use super::mount::{self, Mount};
use super::wire::{self, MAX_FDS};
use super::written::after_ready;
use super::{Front, HandoffError, line};

/// The first line of a request.
pub(super) const TAKE: &str = "take 1";
/// The second line of a request that asks for the `ready` step, and the taker's last frame.
pub(super) const READY: &str = "ready";
/// The running instance's answer to `ready`, after which it ends.
pub(super) const BYE: &str = "bye";
/// The third line of a request that asks for the mount as well (`take 1`, `ready`, `mount`).
pub(super) const MOUNT: &str = "mount";
/// The name of the line of the cache lock in an offer.
pub(super) const LOCK: &str = "lock";
/// Why a request is refused when it is not one this instance understands: a taker that asked for
/// the `ready` step then asks again without it.
pub(super) const NOT_UNDERSTOOD: &str = "the request is not understood";
/// Why a request for the mount is refused by an instance that holds none: the taker asks again
/// without it, for the sockets alone.
pub(super) const NO_MOUNT: &str = "this instance holds no mount";
/// Why an instance that holds the mount refuses a taker that does not ask for it: the mount must
/// not be left with an instance about to end.
pub(super) const HOLDS_MOUNT: &str = "this instance holds the mount, and a taker must ask for it";

/// A listening socket the instance offers: a copy of the descriptor the front serves from, so
/// that the offer outlives nothing but itself.
pub(crate) struct Socket {
    pub(super) front: Front,
    pub(super) listener: TcpListener,
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

/// What an instance does about its own writes while a taker takes its sockets: called with `true`
/// once a taker has answered `ack`, before `done` is sent (the taker starts on the state the
/// instance keeps only after `done`), and with `false` if the hand-off then does not complete and
/// the instance goes on. It may block.
pub(crate) type Hold = Arc<dyn Fn(bool) + Send + Sync>;

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
    /// Told when a taker has answered `ack`, and when that taker did not complete.
    hold: Option<Hold>,
    /// How long after `done` a taker that asked for the `ready` step has to say it; none: this
    /// instance does not take part in the step and refuses a request that names it.
    ready_wait: Option<Duration>,
    /// The mount this instance holds, offered to a taker that asks for it.
    mount: Option<Mount>,
    /// The cache whose lock is offered, where it holds one, to a taker that asks for the `ready`
    /// step.
    cache: Option<LocalCache>,
}

/// What a request asks for.
struct Asked {
    /// How long the taker has to say `ready` after `done`, if it asked for the step.
    ready: Option<Duration>,
    /// Whether it asked for the mount.
    mount: bool,
}

/// What this conversation offered besides the sockets, for the steps after `ack`.
#[derive(Default)]
struct Offered {
    /// The mount.
    mount: bool,
    /// The cache lock.
    lock: bool,
    /// Whether the taker asked for the `ready` step.
    ready: bool,
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
            hold: None,
            ready_wait: None,
            mount: None,
            cache: None,
        })
    }

    /// The same, taking part in the `ready` step: a taker that asks for it (`take 1 ready`) has
    /// `wait` after `done` to say `ready`, and until it does this instance has not handed
    /// anything over: if the taker ends the conversation first, sends something else, or is
    /// silent for `wait`, this instance makes its control socket again, lets the hold go and
    /// goes on, as for a taker that was gone before `done`.
    pub(crate) fn with_ready_wait(mut self, wait: Duration) -> Self {
        self.ready_wait = Some(wait);
        self
    }

    /// The same, telling `hold` when a taker has answered `ack` and when it did not complete.
    pub(crate) fn with_hold(mut self, hold: Hold) -> Self {
        self.hold = Some(hold);
        self
    }

    /// The same, holding `mount`: it is offered to a taker that asks for it (`take 1`, `ready`,
    /// `mount`; the `ready` step needs [`with_ready_wait`](Self::with_ready_wait)), and a taker
    /// that does not ask for it is refused. At `ack` the mount is quiesced, at `ready` given up;
    /// a taker gone between the two leaves it resumed.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the start of the fs interface offers its mount (one-service.md section 6, P3)"
        )
    )]
    pub(crate) fn with_mount(mut self, mount: Mount) -> Self {
        self.mount = Some(mount);
        self
    }

    /// The same, offering the lock of `cache`, while it holds one, to a taker that asks for the
    /// `ready` step. At `ready` the cache starts no write any more; the instance says `written`
    /// once its writes in flight have ended, and unlocks if the successor is gone before this
    /// process ends (`written.rs`).
    pub(crate) fn with_lock(mut self, cache: LocalCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Tells the hold, if there is one, on a thread that may block.
    async fn held(&self, on: bool) {
        if let Some(hold) = self.hold.clone() {
            let _ = tokio::task::spawn_blocking(move || hold(on)).await;
        }
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
    /// A taker that answered `ack` and was gone before `done` reached it, or (when it asked for
    /// the `ready` step) did not say `ready` in time, has not taken the sockets: the instance
    /// makes its control socket again and goes on (if it cannot, it says so and goes on serving
    /// with none, so that it is never handed over and never drains for it).
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
        let mut state = State::default();
        let ended = match self.converse(&stream, &mut state).await {
            Ok(Some((offered, _taking))) => {
                // Held through the steps of `ready`: a taker that connected before the socket's
                // file was removed is refused, and is never offered the mount given up here.
                self.hand(stream, &offered).await;
                return Ended::Handed;
            }
            Ok(None) => Ended::Kept,
            Err(why) => {
                tracing::warn!("tierstore: a hand-off request failed: {why}");
                if state.unlinked {
                    Ended::Unlinked
                } else {
                    Ended::Kept
                }
            }
        };
        if let Some(held) = self.mount.as_ref().filter(|_| state.quiesced) {
            run(&held.resume).await;
        }
        if state.held {
            self.held(false).await;
        }
        ended
    }

    /// What this instance does once the taker has the sockets (after `done`, or after `ready` for
    /// a taker that asked for the step): it gives the mount up and starts no cache write, if it
    /// offered them, and answers `bye` to a `ready`; a lock it offered is then followed until the
    /// successor is gone (`written.rs`). From `ready` on, the instance has handed over for good:
    /// a `bye` that cannot be sent changes nothing, as the taker has the descriptors.
    async fn hand(&self, stream: UnixStream, offered: &Offered) {
        if let Some(held) = self.mount.as_ref().filter(|_| offered.mount) {
            run(&held.give_up).await;
        }
        let cache = self.cache.as_ref().filter(|_| offered.lock);
        if let Some(cache) = cache {
            cache.hand_writes();
        }
        if offered.ready
            && let Err(why) = wire::send(&stream, &[BYE.to_owned()], &[]).await
        {
            tracing::warn!("tierstore: hand-off: bye was not sent: {why}");
        }
        if let Some(cache) = cache {
            drop(tokio::spawn(after_ready(stream, cache.clone())));
        }
    }

    /// What a request asks for, if this instance understands it.
    fn asked(&self, lines: &[String]) -> Option<Asked> {
        let ready = self.ready_wait;
        match lines {
            [take] if take == TAKE => Some(Asked {
                ready: None,
                mount: false,
            }),
            [take, step] if take == TAKE && step == READY => ready.map(|wait| Asked {
                ready: Some(wait),
                mount: false,
            }),
            [take, step, mount] if take == TAKE && step == READY && mount == MOUNT => {
                ready.map(|wait| Asked {
                    ready: Some(wait),
                    mount: true,
                })
            }
            _ => None,
        }
    }

    /// The lines and descriptors offered besides the sockets for `asked`: the mount's, if it was
    /// asked for, and the cache lock's, to a taker that asked for the `ready` step, where the
    /// cache holds the lock.
    fn extras(&self, asked: &Asked) -> std::io::Result<(Vec<String>, Vec<OwnedFd>, Offered)> {
        let (mut lines, mut fds, mut offered) = (Vec::new(), Vec::new(), Offered::default());
        if let Some(held) = self.mount.as_ref().filter(|_| asked.mount) {
            let (device, control) = (held.share)()?;
            lines.extend(mount::lines(&held.mountpoint, &held.control));
            fds.extend([device, control]);
            offered.mount = true;
        }
        let cache = self.cache.as_ref().filter(|_| asked.ready.is_some());
        if let Some((path, fd)) = cache.map(LocalCache::lock_to_hand).transpose()?.flatten() {
            lines.push(format!("{LOCK} {}", path.display()));
            fds.push(fd);
            offered.lock = true;
        }
        offered.ready = asked.ready.is_some();
        Ok((lines, fds, offered))
    }

    /// Reads the request, offers the sockets, and on `ack` gives up the control socket; what was
    /// offered when the taker has them. Notes in `state` what this conversation changed, so that
    /// a failure after a step is told from one before it by this conversation alone.
    async fn converse(
        &self,
        stream: &UnixStream,
        state: &mut State,
    ) -> std::io::Result<Option<(Offered, MutexGuard<'_, ()>)>> {
        let request = wire::recv(stream).await?;
        let Some(asked) = self.asked(&request.lines) else {
            return refuse(stream, NOT_UNDERSTOOD).await;
        };
        if stream.peer_cred()?.uid() != self.uid {
            return refuse(stream, "the taker is not the user of this instance").await;
        }
        match (asked.mount, self.mount.is_some()) {
            (true, false) => return refuse(stream, NO_MOUNT).await,
            (false, true) => return refuse(stream, HOLDS_MOUNT).await,
            _ => {}
        }
        let Ok(taking) = self.taking.try_lock() else {
            return refuse(stream, "another taker is taking the sockets").await;
        };
        let mut lines = vec!["offer".to_owned()];
        for socket in &self.sockets {
            lines.push(line(socket.front, socket.listener.local_addr()?));
        }
        let (extra_lines, extra_fds, offered) = self.extras(&asked)?;
        if self.sockets.len() + extra_fds.len() > MAX_FDS {
            return refuse(stream, "more descriptors than a message can carry").await;
        }
        lines.extend(extra_lines);
        let sockets = self.sockets.iter().map(|s| s.listener.as_fd());
        let fds: Vec<BorrowedFd<'_>> = sockets.chain(extra_fds.iter().map(AsFd::as_fd)).collect();
        wire::send(stream, &lines, &fds).await?;
        // The taker has its own copies now.
        drop(extra_fds);
        if wire::recv(stream).await?.lines != ["ack"] {
            return Ok(None);
        }
        // Whatever this instance writes is written before the taker reads it, which it does after
        // `done`.
        state.held = true;
        self.held(true).await;
        if let Some(held) = self.mount.as_ref().filter(|_| offered.mount) {
            // The taker holds the device in its store: from here this instance reads nothing of
            // it, until a resume.
            state.quiesced = true;
            run(&held.quiesce).await;
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
        state.unlinked = true;
        wire::send(stream, &["done".to_owned()], &[]).await?;
        if let Some(wait) = asked.ready {
            awaits_ready(stream, wait).await?;
        }
        Ok(Some((offered, taking)))
    }
}

/// Runs a step of the mount on a thread that may block.
async fn run(step: &mount::Step) {
    let step = Arc::clone(step);
    let _ = tokio::task::spawn_blocking(move || step()).await;
}

/// What one conversation changed, for the undoing of a taker that did not complete.
#[derive(Default)]
struct State {
    /// The control socket's file was removed.
    unlinked: bool,
    /// The hold was told `true`.
    held: bool,
    /// The mount was quiesced.
    quiesced: bool,
}

/// Reads the taker's `ready`, which it has `wait` to send after `done`. The answer, `bye`, is
/// sent once the instance has handed over ([`Offer::hand`]).
///
/// # Errors
/// The taker ended the conversation, sent something else, or said nothing within `wait`.
async fn awaits_ready(stream: &UnixStream, wait: Duration) -> std::io::Result<()> {
    let timed_out = |_| std::io::Error::new(std::io::ErrorKind::TimedOut, "no ready in time");
    let frame = tokio::time::timeout(wait, wire::recv(stream))
        .await
        .map_err(timed_out)??;
    if frame.lines == [READY] {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "something other than ready after done",
        ))
    }
}

/// How one taker's conversation ended.
enum Ended {
    /// The sockets are the taker's: the instance drains.
    Handed,
    /// Nothing changed: the instance goes on with its control socket.
    Kept,
    /// The taker answered `ack` and was gone before `done`, or did not say `ready`: the control
    /// socket's file is gone and must be made again.
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
async fn refuse<T>(stream: &UnixStream, why: &str) -> std::io::Result<Option<T>> {
    wire::send(stream, &["refused".to_owned(), why.to_owned()], &[]).await?;
    Ok(None)
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
#[path = "offer_ready_tests.rs"]
mod ready_tests;
#[cfg(test)]
#[path = "offer_service_tests.rs"]
mod service_tests;
#[cfg(test)]
#[path = "offer_tests.rs"]
mod tests;
