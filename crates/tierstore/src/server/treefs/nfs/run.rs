//! The process of `tierstore fs` on macOS: the neutral start (`start.rs`), the NFS and MOUNT
//! protocols on a loopback socket, the mount, and the wait for the signal that ends it.
//!
//! A start, in order: the mount user's number is found; if `fs.nfs.handoff_socket` is given, a
//! running instance is asked for its listening socket (`handoff`); the neutral start is done (the
//! table of tree numbers of this boot reopened if there is one, the attachments made again, the
//! control socket bound), with a core that counts no lookups, since NFS never forgets; the socket
//! is the running instance's or is bound; the protocols are served; and the export is mounted at
//! `fs.mountpoint`, as the mount user, unless something is mounted there already (a mount outlives
//! the server: a restart, or the instance this one replaces, left it, and its clients wait); a mount
//! there that was made from another address than this instance serves on reaches no one, and is
//! unmounted first. launchd needs no notification of readiness.
//!
//! The process unmounts before it stops serving, on every way out it controls, by one function,
//! [`Running::leave`]: SIGTERM and SIGINT (the process then ends with status 0); a start that fails
//! after the mount was made; and a server that stops by itself (its accept failed, or it
//! panicked: the task that serves catches the panic and keeps the listening socket until the
//! unmount is done). The leave ends the offer of the listening socket, so no successor takes the
//! mount while it is unmounted; unmounts (`umount`, `diskutil unmount force`, `unmount(2)` with
//! `MNT_FORCE`, each within `fs.nfs.unmount_wait`, see `mount.rs`) while the server still answers;
//! and only then ends the serving. If every step fails, the process says why, goes on serving and
//! tries again: it never exits while its mount is up (if launchd kills it, the mount's dead
//! timeout ends the mount). A hand-over ends without unmounting: the mount is the successor's.
//!
//! An instance with a control socket offers its listening socket to the next instance. When that
//! has taken it and says `ready` (the next instance does so when its start is done and it is about
//! to serve; until then this one goes on accepting and answering), this one stops accepting,
//! answers the requests it has read, shuts down the write side of each connection (the clients
//! reconnect to the new instance, which holds the socket), waits for them to close for at most
//! `fs.nfs.drain_wait`, and ends with status 0 without unmounting: the mount is the new
//! instance's now. If the next instance is gone before `ready`, or says it later than
//! `fs.nfs.handoff_ready_wait` after `done`, this one has handed nothing over: it serves on, takes
//! attaches again, and binds its control socket again where the next had replaced it.

use std::os::fd::AsFd as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use nfsserve::tcp::{NFSTcp as _, NFSTcpListener};
use tokio::sync::{Notify, oneshot};

use super::boot::BootSource;
use super::error::NfsError;
use super::fs::{Patience, TreeNfs};
use super::mount::{Mounter, Runner, System};
use crate::server::handoff::{
    Front, Hold, Offer, Socket, Want, check_control_path, take_with_ready,
};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::control::Control;
use crate::server::treefs::core::Core;
use crate::server::treefs::start::{Identity, Setup, Signals, io};

/// A patience that is a timer of `after`.
fn timer(after: Duration) -> Patience {
    Arc::new(move || Box::pin(tokio::time::sleep(after)))
}

/// What serves the listening socket until the future it is given resolves: [`handle`] (the
/// protocol's `NFSTcp::handle_until`), or a test's server that fails or panics.
pub(super) type Serve = for<'a> fn(
    &'a NFSTcpListener<TreeNfs>,
    BoxFuture<'static, ()>,
) -> BoxFuture<'a, std::io::Result<()>>;

/// The server of the protocol.
fn handle(
    listener: &NFSTcpListener<TreeNfs>,
    ending: BoxFuture<'static, ()>,
) -> BoxFuture<'_, std::io::Result<()>> {
    listener.handle_until(ending)
}

/// Why a serving instance leaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Leaving {
    /// SIGTERM or SIGINT.
    Signal,
    /// The server stopped by itself, for this reason.
    ServerStopped(String),
    /// A start failed after it made the mount, for this reason.
    StartFailed(String),
    /// The serving ended: a successor took the listening socket.
    Ended,
}

impl std::fmt::Display for Leaving {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signal => write!(f, "a signal to stop"),
            Self::ServerStopped(why) => write!(f, "the NFS server stopped ({why})"),
            Self::StartFailed(why) => write!(f, "the start failed after the mount ({why})"),
            Self::Ended => write!(f, "the listening socket was handed over"),
        }
    }
}

/// A started NFS frontend: the core, the socket it serves on, and the mount.
pub(super) struct Running {
    _setup: Setup,
    /// What the hold of a hand-off does, and the control socket it made again.
    _keeper: Arc<Keeper>,
    server: tokio::task::JoinHandle<()>,
    /// Ends the offer of the listening socket, the first step of a leave; answered once no
    /// successor can take the socket.
    closing: Option<oneshot::Sender<oneshot::Sender<()>>>,
    /// Ends the serving, the last step of a leave. A hand-over ends it by itself.
    stopping: Arc<Notify>,
    /// Says why the server stopped by itself; it keeps the listening socket until `stopping`.
    failed: oneshot::Receiver<String>,
    /// Whether a successor took the listening socket, which ended the server.
    handed: Arc<AtomicBool>,
    /// Where the protocols are served.
    #[cfg(test)]
    pub(super) address: std::net::SocketAddr,
    /// Set where the mount was made, so that it is unmounted.
    mounted: Option<Mounter>,
}

impl Running {
    /// Does what a start does, up to and including the mount. `epoch` is the boot's (`boot.rs`):
    /// a table of tree numbers of this epoch is reopened, and the handles carry it.
    ///
    /// # Errors
    /// [`NfsError`]: why nothing was served or mounted.
    pub(super) async fn start(
        settings: &FsSettings,
        runner: Arc<dyn Runner>,
        euid: u32,
        patience: Option<Patience>,
        epoch: u32,
    ) -> Result<Self, NfsError> {
        Self::start_serving(settings, runner, euid, patience, epoch, handle).await
    }

    /// [`Running::start`], with `serve` as what serves the listening socket.
    ///
    /// # Errors
    /// As [`Running::start`].
    pub(super) async fn start_serving(
        settings: &FsSettings,
        runner: Arc<dyn Runner>,
        euid: u32,
        patience: Option<Patience>,
        epoch: u32,
        serve: Serve,
    ) -> Result<Self, NfsError> {
        let nfs = &settings.nfs;
        let mounter = Mounter::new(runner, nfs, settings.mountpoint.clone(), euid);
        // A mount user the machine does not know fails the start here, before a running instance
        // gives up its socket.
        mounter.user_id()?;
        let handoff = nfs.handoff.as_ref();
        if let Some(handoff) = handoff {
            check_control_path(&handoff.socket)?;
        }
        // What can fail is found before the running instance gives up its socket: once it has,
        // it ends, and a start that fails then leaves no one listening.
        Setup::preflight(settings)?;
        // The running instance, if there is one, hands its socket over before this start binds
        // the control socket or opens the table: it holds both until then.
        let taken = match handoff {
            Some(handoff) => {
                take_with_ready(
                    handoff,
                    &Want {
                        s3: &[],
                        oci: &[],
                        nfs: &[nfs.listen],
                    },
                )
                .await?
            }
            None => None,
        };
        let setup = Setup::start(settings, false, Identity::Boot(epoch))?;
        let reopened = setup.reopened;
        let listen = |source| NfsError::Listen {
            address: nfs.listen,
            source,
        };
        let mut inherited = taken;
        let listener = match inherited.as_mut().and_then(|taken| taken.nfs.pop()) {
            Some(listener) => listener,
            None => tokio::net::TcpListener::bind(nfs.listen)
                .await
                .map_err(listen)?,
        };
        let address = listener.local_addr().map_err(listen)?;
        let keeper = Arc::new(Keeper::new(&setup.core, settings));
        // A cold start's copy of the socket, which its own offer hands to the next instance.
        let cold = match (nfs.handoff.as_ref(), inherited.as_ref()) {
            (Some(_), None) => Some(std::net::TcpListener::from(
                listener.as_fd().try_clone_to_owned().map_err(listen)?,
            )),
            _ => None,
        };
        let fs = TreeNfs::new(
            Arc::clone(&setup.core),
            nfs,
            patience.unwrap_or_else(|| timer(nfs.jukebox_after)),
            epoch,
        );
        let listener = NFSTcpListener::from_listener(listener, fs).map_err(listen)?;
        // Everything that can fail before this instance serves is done: the running instance, if
        // it waits, may end now. A start that failed before this point dropped the connection,
        // and the running instance went on serving.
        if let Some(ready) = inherited.as_mut().and_then(|taken| taken.ready.take()) {
            ready.send().await?;
        }
        let hold = || -> Hold {
            let keeper = Arc::clone(&keeper);
            Arc::new(move |on| keeper.hold(on))
        };
        let offer = match (nfs.handoff.as_ref(), inherited, cold) {
            (Some(handoff), Some(taken), _) => {
                offer_after_takeover(&handoff.socket, taken.copies, nfs.ready_wait, hold())
            }
            (Some(handoff), None, Some(copy)) => Some(
                Offer::bind(&handoff.socket, vec![Socket::new(Front::Nfs, copy)])?
                    .with_ready_wait(nfs.ready_wait)
                    .with_hold(hold()),
            ),
            _ => None,
        };
        let stopping = Arc::new(Notify::new());
        let (closing, closed) = oneshot::channel();
        let (failing, failed) = oneshot::channel();
        let handed = Arc::new(AtomicBool::new(false));
        let server = tokio::spawn(serve_until(
            Served {
                listener,
                serve,
                closing: closed,
                stop: Arc::clone(&stopping),
                failed: failing,
            },
            offer,
            Arc::clone(&handed),
            nfs.drain_wait,
        ));
        let numbers = if reopened { "kept" } else { "made afresh" };
        tracing::info!(
            "tierstore fs: serving NFS on {address}, epoch {epoch:08x}, tree numbers {numbers}"
        );
        // The command, whether this process runs it or not: a mount made by hand (with
        // `fs.nfs.mount = false`) uses the same options.
        tracing::info!(
            "tierstore fs: the mount command is {}",
            mounter
                .mount_command(address.ip(), address.port())
                .join(" ")
        );
        let mut running = Self {
            _setup: setup,
            _keeper: keeper,
            server,
            closing: Some(closing),
            stopping,
            failed,
            handed,
            #[cfg(test)]
            address,
            mounted: None,
        };
        if nfs.mount {
            let task = mounter.clone();
            let runtime_dir = settings.runtime_dir.clone();
            let joined = tokio::task::spawn_blocking(move || -> Result<bool, NfsError> {
                if task.is_mounted()? {
                    match Mounter::made_from(&runtime_dir) {
                        // A hard mount of an address no one serves: everything that touches it
                        // waits forever. It is cleared, and the export mounted afresh.
                        Some(was) if was != address.to_string() => {
                            tracing::warn!(
                                "tierstore fs: {} is mounted from {was}, and this instance \
                                 serves on {address}: unmounting it",
                                task.mountpoint().display()
                            );
                            task.unmount()?;
                        }
                        _ => return Ok(false),
                    }
                }
                task.mount(address.ip(), address.port())?;
                Ok(true)
            })
            .await;
            let made = match joined {
                // A mount that failed, or a mount of another address that would not go: nothing
                // of this instance is mounted.
                Ok(made) => made?,
                // The command died: whether it mounted is not known, so the leave looks.
                Err(why) => {
                    let why = io("waiting for the mount")(std::io::Error::other(why.to_string()));
                    running.mounted = Some(mounter);
                    return Err(running.failed_start(why.into()).await);
                }
            };
            running.mounted = Some(mounter);
            let point = settings.mountpoint.display().to_string();
            if made {
                if let Err(why) = Mounter::record(&settings.runtime_dir, address) {
                    return Err(running.failed_start(why).await);
                }
                tracing::info!("tierstore fs: mounted at {point}");
            } else {
                tracing::info!(
                    "tierstore fs: {point} is mounted already: the clients' mount goes on"
                );
            }
        }
        Ok(running)
    }

    /// Leaves after a start that failed once the mount may be there, and gives why it failed
    /// (or why the leave did, which is worse).
    async fn failed_start(self, why: NfsError) -> NfsError {
        self.leave(Leaving::StartFailed(why.to_string()))
            .await
            .err()
            .unwrap_or(why)
    }

    /// Resolves when the serving ends by itself: [`Leaving::Ended`] when a successor has taken
    /// the listening socket and this instance has answered what it owed, and
    /// [`Leaving::ServerStopped`] when the server stopped (it keeps the listening socket until
    /// [`Running::leave`] has unmounted). Awaited once.
    pub(super) async fn ended(&mut self) -> Leaving {
        tokio::select! {
            Ok(why) = &mut self.failed => Leaving::ServerStopped(why),
            _ = &mut self.server => Leaving::Ended,
        }
    }

    /// How the process leaves, whatever the reason: ends the offer of the listening socket,
    /// unmounts, then ends the serving; unless the sockets were handed over before the offer
    /// ended, when the mount is the successor's. The unmount comes after the offer has ended, so
    /// that no successor takes the socket (and the mount it found) while it runs, and before the
    /// serving ends, while the server still answers the clients: a server that has stopped leaves
    /// a busy mount that only a forced unmount clears. An unmount whose every step fails is said
    /// and tried again ([`Mounter::unmount_or_retry`]) while the process serves on: this returns
    /// only once the mount is gone.
    ///
    /// # Errors
    /// [`NfsError::Start`] if the unmount itself died (its command panicked); the serving is ended
    /// all the same.
    pub(super) async fn leave(mut self, leaving: Leaving) -> Result<(), NfsError> {
        tracing::info!("tierstore fs: leaving: {leaving}");
        let (ack, acked) = oneshot::channel();
        // A server that has ended (handed over, or stopped) drops the answer unsent.
        let _ = self.closing.take().map(|closing| closing.send(ack));
        let _ = acked.await;
        let unmounted = match self.mounted.take() {
            Some(mounter) if !self.handed.load(Ordering::SeqCst) => {
                tracing::info!(
                    "tierstore fs: unmounting {} before the serving ends",
                    mounter.mountpoint().display()
                );
                match tokio::task::spawn_blocking(move || mounter.unmount_or_retry()).await {
                    Ok(()) => Ok(()),
                    Err(why) => Err(io("waiting for the unmount")(std::io::Error::other(
                        why.to_string(),
                    ))
                    .into()),
                }
            }
            _ => Ok(()),
        };
        self.stopping.notify_one();
        unmounted
    }
}

impl Drop for Running {
    /// A start that fails after the socket is served, and a stop, end the server.
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// What an instance does about its own state while a successor is taking its socket, and when
/// that does not complete.
///
/// The hold makes the core refuse attaches and detaches from the `ack` on (the successor reads
/// the state after `done`). If the hand-off does not complete (the successor is gone, or does
/// not say `ready`), the hold is let go, and the control socket is made again where a successor
/// that began to start had replaced it: the successor binds `fs.control_socket` in its start,
/// and one that died there leaves a file no one listens on, so without this an instance that
/// goes on serving could not be asked to attach a tree again until it restarted.
struct Keeper {
    core: Arc<Core>,
    control: std::path::PathBuf,
    group: u32,
    /// The device and inode of the control socket's file as this instance's start left it.
    file: Option<(u64, u64)>,
    /// The control socket made again, kept so that it serves.
    revived: std::sync::Mutex<Option<Control>>,
}

impl Keeper {
    /// The keeper of `core`, which serves the control socket of `settings`.
    fn new(core: &Arc<Core>, settings: &FsSettings) -> Self {
        Self {
            core: Arc::clone(core),
            control: settings.control_socket.clone(),
            group: settings.client_group,
            file: file_of(&settings.control_socket),
            revived: std::sync::Mutex::new(None),
        }
    }

    /// The hold of the hand-off: `on` at `ack`, off when the hand-off does not complete.
    fn hold(&self, on: bool) {
        self.core.hold(on);
        if on || file_of(&self.control) == self.file {
            return;
        }
        let made = Control::serve(Arc::clone(&self.core), &self.control, self.group);
        *self
            .revived
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = made
            .map_err(|why| {
                tracing::warn!(
                    "tierstore fs: the control socket at {} cannot be made again ({why}); this \
                     instance goes on serving and takes no attach until it restarts",
                    self.control.display()
                );
            })
            .ok();
    }
}

/// The device and inode of the file at `path`, if there is one.
fn file_of(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

/// The control socket of a process that took its listening socket from the one it replaces: the
/// sockets it took are what it offers to its own successor. A process that cannot make the control
/// socket says so and serves without one, since the update has happened and cannot be undone.
fn offer_after_takeover(
    socket: &std::path::Path,
    copies: Vec<Socket>,
    ready_wait: Duration,
    hold: Hold,
) -> Option<Offer> {
    Offer::bind(socket, copies)
        .map(|offer| offer.with_ready_wait(ready_wait).with_hold(hold))
        .map_err(|why| {
            tracing::warn!(
                "tierstore fs: {why}; this instance serves without a control socket and \
                 cannot be handed over until it restarts"
            );
        })
        .ok()
}

/// What the task that serves holds.
struct Served {
    listener: NFSTcpListener<TreeNfs>,
    serve: Serve,
    /// Comes first in a leave: it drops the offer, and is answered once no successor can take
    /// the socket any more.
    closing: oneshot::Receiver<oneshot::Sender<()>>,
    /// Comes last in a leave: it ends the serving.
    stop: Arc<Notify>,
    /// Told why the server stopped by itself.
    failed: oneshot::Sender<String>,
}

/// Serves on the listener until `stop` is notified or `offer` is taken by a successor, then lets
/// the connections finish (`NFSTcp::handle_until`), for at most `drain_wait` after that. A server
/// that fails or panics is caught: the task says why on `failed` and keeps the listening socket
/// until `stop`, so that the leave unmounts before the socket closes.
async fn serve_until(
    served: Served,
    offer: Option<Offer>,
    handed: Arc<AtomicBool>,
    drain_wait: Duration,
) {
    let Served {
        listener,
        serve,
        closing,
        stop,
        failed,
    } = served;
    let stopped = Arc::new(Notify::new());
    let signal = Arc::clone(&stopped);
    let ending_stop = Arc::clone(&stop);
    let ending = async move {
        let closed = match offer {
            Some(offer) => tokio::select! {
                closed = closing => Some(closed),
                () = offer.serve() => None,
            },
            None => Some(closing.await),
        };
        match closed {
            // The offer is gone (its socket closed with it): no successor takes the socket from
            // here on, so the stop may unmount; the stop is told so, and then ends the serving.
            Some(closed) => {
                if let Ok(ack) = closed {
                    let _ = ack.send(());
                }
                ending_stop.notified().await;
            }
            None => {
                handed.store(true, Ordering::SeqCst);
                tracing::info!("tierstore fs: the listening socket was taken");
            }
        }
        signal.notify_one();
    };
    let serving = std::panic::AssertUnwindSafe(serve(&listener, Box::pin(ending))).catch_unwind();
    let patience = async {
        stopped.notified().await;
        tokio::time::sleep(drain_wait).await;
    };
    let why = tokio::select! {
        served = serving => match served {
            Ok(Ok(())) => None,
            Ok(Err(why)) => Some(why.to_string()),
            Err(_) => Some("it panicked".to_owned()),
        },
        () = patience => None,
    };
    if let Some(why) = why {
        tracing::error!(
            "tierstore fs: the NFS server stopped: {why}; the mount is unmounted before the \
             listening socket closes"
        );
        let _ = failed.send(why);
        stop.notified().await;
    }
}

/// Starts over `runner`, in the boot `boot` names, and serves until SIGTERM or SIGINT, a hand-over,
/// or a server that stops by itself; then leaves ([`Running::leave`]).
///
/// # Errors
/// [`NfsError`]: why nothing was mounted, or why the unmount died.
pub(super) fn serve_with(
    settings: &FsSettings,
    runner: Arc<dyn Runner>,
    euid: u32,
    boot: &BootSource,
) -> Result<(), NfsError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io("starting the runtime"))?;
    let epoch = boot.epoch()?;
    runtime.block_on(async {
        // The handlers are in place before the mount is: a signal after it is never lost.
        let mut signals = Signals::install()?;
        let mut running = Running::start(settings, runner, euid, None, epoch).await?;
        let leaving = tokio::select! {
            () = signals.wait() => Leaving::Signal,
            leaving = running.ended() => leaving,
        };
        running.leave(leaving).await
    })
}

/// Serves the NFS frontend with the commands of this machine, until SIGTERM or SIGINT.
///
/// # Errors
/// As [`serve_with`].
pub(in crate::server) fn serve(settings: &FsSettings) -> Result<(), NfsError> {
    serve_with(
        settings,
        Arc::new(System),
        rustix::process::geteuid().as_raw(),
        &BootSource::system(),
    )
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod handoff_tests;
#[cfg(test)]
#[path = "ready_tests.rs"]
mod ready_tests;
#[cfg(test)]
#[path = "restart_tests.rs"]
mod restart_tests;
#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
