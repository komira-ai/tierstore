//! The run of the NFS front on macOS: the neutral start (`start.rs`), the NFS and MOUNT protocols
//! on a loopback socket, the mount, and the wait for what ends it. The `fs` interface of the one
//! process starts and leaves this front (`interface.rs`), with a core that calls the node's cache;
//! the tests also run it alone, through `serve_with`, with the wait for SIGTERM or SIGINT.
//!
//! A start, in order: the mount user's number is found; what can fail is checked
//! (`Setup::preflight`); the neutral start is done (the table of tree numbers of this boot
//! reopened if there is one, the attachments made again, the control socket bound or taken from
//! the image before), with a core that counts no lookups, since NFS never forgets; the socket is
//! the one the image before handed over or is bound; the protocols are served; and the export is
//! mounted at `fs.mountpoint`, as the mount user, unless something is mounted there already (a
//! mount outlives the server: a restart, or the image this one replaces, left it, and its clients
//! wait); a mount there that was made from another address than this instance serves on reaches no
//! one, and is unmounted first. launchd needs no notification of readiness.
//!
//! The process unmounts before it stops serving, on every way out it controls, by one function,
//! [`Running::leave`]: SIGTERM and SIGINT (the process then ends with status 0); a start that fails
//! after the mount was made; and a server that stops by itself (its accept failed, or it
//! panicked: the task that serves catches the panic and keeps the listening socket until the
//! unmount is done). The leave ends the hand-over to the next image, so none takes the mount while
//! it is unmounted; unmounts (`umount`, `diskutil unmount force`, `unmount(2)` with
//! `MNT_FORCE`, each within `fs.nfs.unmount_wait`, see `mount.rs`) while the server still answers;
//! and only then ends the serving. If every step fails, the process says why, goes on serving and
//! tries again: it never exits while its mount is up (if launchd kills it, the mount's dead
//! timeout ends the mount). A hand-over ends without unmounting: the mount is the next image's.
//!
//! On a Mac the one process hands over to the next image of its own program
//! (`docs/design/one-service.md` section 3.4, `handoff/exec.rs`): [`Running::hand_to_image`] stops
//! accepting (the listening socket stays open, so new connections wait in its backlog), answers the
//! requests it has read, shuts down the write side of each connection and waits for the clients to
//! close, for at most `fs.nfs.drain_wait`; then it holds the core (no
//! table or attachment write), pauses the control socket, and gives the listening socket and the
//! control socket's to the offer. The mount is left as it is: its client waits. The next image
//! starts from them ([`Running::start_handed`]): it serves on the socket it was handed, answers on
//! the control socket it was handed, reopens the table of this boot, and finds its mount.

use std::os::fd::AsFd as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use nfsserve::tcp::{NFSTcp as _, NFSTcpListener};
use tokio::sync::{Notify, oneshot};

#[cfg(test)]
use super::boot::BootSource;
use super::error::NfsError;
use super::fs::{Patience, TreeNfs};
use super::mount::{Mounter, Runner};
use crate::server::handoff::{Front, Handed, Handing, Socket};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::fills::Calls;
#[cfg(test)]
use crate::server::treefs::start::Signals;
use crate::server::treefs::start::{Identity, Setup, io};

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
pub(super) fn handle<'a>(
    listener: &'a NFSTcpListener<TreeNfs>,
    ending: BoxFuture<'static, ()>,
) -> BoxFuture<'a, std::io::Result<()>> {
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
    /// The serving ended: the next image was handed the listening socket.
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
    /// The path of the control socket, which the next image binds again.
    control: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
    /// The first step of a leave; answered once the next image can no longer be handed the
    /// socket.
    closing: Option<oneshot::Sender<oneshot::Sender<()>>>,
    /// Ends the serving, the last step of a leave. A hand-over ends it by itself.
    stopping: Arc<Notify>,
    /// Says why the server stopped by itself; it keeps the listening socket until `stopping`.
    failed: oneshot::Receiver<String>,
    /// Whether the next image is given the listening socket, which ended the server: the mount is
    /// not unmounted.
    handed: Arc<AtomicBool>,
    /// A copy of the listening socket, for the next image of the process.
    kept: std::net::TcpListener,
    /// Ends the serving for the next image of the process, which gets the listening socket.
    next: Option<oneshot::Sender<()>>,
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
    #[cfg(test)]
    pub(super) async fn start(
        (settings, reach): (&FsSettings, &Calls),
        runner: Arc<dyn Runner>,
        euid: u32,
        patience: Option<Patience>,
        epoch: u32,
    ) -> Result<Self, NfsError> {
        Self::start_serving((settings, reach), runner, euid, patience, epoch, handle).await
    }

    /// [`Running::start`] of the next image of a process that handed over to it: it serves on the
    /// listening socket and the control socket of `handed`, and finds the mount of the image
    /// before it.
    ///
    /// # Errors
    /// As [`Running::start`].
    #[cfg(test)]
    pub(super) async fn start_handed(
        (settings, reach): (&FsSettings, &Calls),
        runner: Arc<dyn Runner>,
        euid: u32,
        patience: Option<Patience>,
        epoch: u32,
        handed: Handed,
    ) -> Result<Self, NfsError> {
        let ways = (runner, euid, patience, epoch);
        Self::start_from((settings, reach), ways, handle, Some(handed)).await
    }

    /// [`Running::start`], with `serve` as what serves the listening socket.
    ///
    /// # Errors
    /// As [`Running::start`].
    #[cfg(test)]
    pub(super) async fn start_serving(
        (settings, reach): (&FsSettings, &Calls),
        runner: Arc<dyn Runner>,
        euid: u32,
        patience: Option<Patience>,
        epoch: u32,
        serve: Serve,
    ) -> Result<Self, NfsError> {
        let ways = (runner, euid, patience, epoch);
        Self::start_from((settings, reach), ways, serve, None).await
    }

    /// [`Running::start_serving`], with a core that calls the node's cache and tree store of
    /// `reach`, and from what `handed` holds when the image before this one handed it over. It
    /// runs on a thread that may block: the attachments are made again through the cache, whose
    /// answers are worked out on the runtime.
    ///
    /// # Errors
    /// As [`Running::start`].
    pub(super) async fn start_from(
        (settings, reach): (&FsSettings, &Calls),
        (runner, euid, patience, epoch): (Arc<dyn Runner>, u32, Option<Patience>, u32),
        serve: Serve,
        handed: Option<Handed>,
    ) -> Result<Self, NfsError> {
        let nfs = &settings.nfs;
        let mounter = Mounter::new(runner, nfs, settings.mountpoint.clone(), euid);
        // A mount user the machine does not know fails the start here, before anything is bound.
        mounter.user_id()?;
        Setup::preflight(settings)?;
        // The image before this one, when it executed this one, handed over what it held.
        let (taken, control) = match handed {
            Some(handed) => (Some(handed.inherited), handed.control),
            None => (None, None),
        };
        let setup = Setup::start((settings, reach), false, Identity::Boot(epoch), control)?;
        let reopened = setup.reopened;
        let listen = |source| NfsError::Listen {
            address: nfs.listen,
            source,
        };
        let listener = match taken.and_then(|mut taken| taken.nfs.pop()) {
            Some(listener) => listener,
            None => tokio::net::TcpListener::bind(nfs.listen)
                .await
                .map_err(listen)?,
        };
        let address = listener.local_addr().map_err(listen)?;
        let control = settings.control_socket.clone();
        let kept =
            std::net::TcpListener::from(listener.as_fd().try_clone_to_owned().map_err(listen)?);
        let fs = TreeNfs::new(
            Arc::clone(&setup.core),
            nfs,
            patience.unwrap_or_else(|| timer(nfs.jukebox_after)),
            epoch,
        );
        let listener = NFSTcpListener::from_listener(listener, fs).map_err(listen)?;
        let stopping = Arc::new(Notify::new());
        let (closing, closed) = oneshot::channel();
        let (failing, failed) = oneshot::channel();
        let (next, imaged) = oneshot::channel();
        let handed = Arc::new(AtomicBool::new(false));
        let server = tokio::spawn(serve_until(
            Served {
                listener,
                serve,
                closing: closed,
                imaged,
                stop: Arc::clone(&stopping),
                failed: failing,
            },
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
            control,
            server,
            closing: Some(closing),
            stopping,
            failed,
            handed,
            kept,
            next: Some(next),
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

    /// Resolves when the serving ends by itself: [`Leaving::Ended`] when the next image has been
    /// handed the listening socket and this instance has answered what it owed, and
    /// [`Leaving::ServerStopped`] when the server stopped (it keeps the listening socket until
    /// [`Running::leave`] has unmounted). Awaited once.
    pub(super) async fn ended(&mut self) -> Leaving {
        tokio::select! {
            Ok(why) = &mut self.failed => Leaving::ServerStopped(why),
            _ = &mut self.server => Leaving::Ended,
        }
    }

    /// The step of the hand-over to the next image of the process (one-service.md section 3.4):
    /// the serving ends (no connection is accepted, the
    /// listening socket stays open with its backlog, every request read is answered, the write side
    /// of each connection is shut down, and the clients are waited for, for at most
    /// `fs.nfs.drain_wait`), the control socket stops accepting (its connections wait in its
    /// backlog) and is left to the next image, and the core is held (no table or attachment
    /// write). What it gives is the offer of the listening socket and of the control socket; the
    /// mount is left as it is.
    ///
    /// # Errors
    /// [`NfsError::Start`] when the control socket's descriptor cannot be copied.
    pub(super) async fn hand_to_image(mut self) -> Result<Handing, NfsError> {
        // Taken once: this consumes the instance.
        let _ = self.next.take().map(|next| next.send(()));
        let ended = self.ended().await;
        tracing::info!("tierstore fs: the NFS front is left to the next image ({ended})");
        let control = &mut self._setup._control;
        control.pause();
        control.leave_path();
        let copy = control
            .listener_fd()
            .map_err(io("copying the control socket"))?;
        let core = Arc::clone(&self._setup.core);
        // Every request read is answered: the node is told of the opens, the requests and the tree
        // lists of this image.
        let held = tokio::task::spawn_blocking(move || {
            core.hold(true);
            core.report_opens();
            core.report_requests();
            core.report_tree_lists();
        })
        .await;
        held.map_err(|why| io("holding the file system")(std::io::Error::other(why.to_string())))?;
        let kept = self
            .kept
            .try_clone()
            .map_err(io("copying the listening socket"))?;
        Ok(Handing {
            sockets: vec![Socket::new(Front::Nfs, kept)],
            control: Some((self.control.clone(), copy)),
            lock: None,
        })
    }

    /// How the process leaves, whatever the reason: ends the hand-over to the next image,
    /// unmounts, then ends the serving; unless the sockets were handed over before it ended, when
    /// the mount is the next image's. The unmount comes after the hand-over has ended, so that no
    /// next image is handed the socket (and the mount it would find) while it runs, and before the
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

/// What the task that serves holds.
struct Served {
    listener: NFSTcpListener<TreeNfs>,
    serve: Serve,
    /// Comes first in a leave: it is answered once the next image can no longer be handed the
    /// socket.
    closing: oneshot::Receiver<oneshot::Sender<()>>,
    /// Ends the serving for the next image of the process, which gets the listening socket.
    imaged: oneshot::Receiver<()>,
    /// Comes last in a leave: it ends the serving.
    stop: Arc<Notify>,
    /// Told why the server stopped by itself.
    failed: oneshot::Sender<String>,
}

/// Serves on the listener until `stop` is notified or the next image is handed the socket, then
/// lets the connections finish (`NFSTcp::handle_until`), for at most `drain_wait` after that. A
/// server that fails or panics is caught: the task says why on `failed` and keeps the listening
/// socket until `stop`, so that the leave unmounts before the socket closes.
async fn serve_until(served: Served, handed: Arc<AtomicBool>, drain_wait: Duration) {
    let Served {
        listener,
        serve,
        closing,
        mut imaged,
        stop,
        failed,
    } = served;
    let stopped = Arc::new(Notify::new());
    let signal = Arc::clone(&stopped);
    let ending_stop = Arc::clone(&stop);
    let ending = async move {
        let closed = tokio::select! {
            closed = closing => Some(closed),
            Ok(()) = &mut imaged => None,
        };
        match closed {
            // No next image is handed the socket from here on, so the stop may unmount; the stop
            // is told so, and then ends the serving.
            Some(closed) => {
                // A leave that is gone asks for no answer.
                let _ = closed.map(|ack| ack.send(()));
                ending_stop.notified().await;
            }
            None => {
                handed.store(true, Ordering::SeqCst);
                tracing::info!("tierstore fs: the listening socket was handed to the next image");
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
/// or a server that stops by itself; then leaves ([`Running::leave`]): the loop of the process
/// that served the mount alone, which the tests run to drive the start and the leave that the `fs`
/// interface of the one process (`interface.rs`) is made of.
///
/// # Errors
/// [`NfsError`]: why nothing was mounted, or why the unmount died.
#[cfg(test)]
pub(super) fn serve_with(
    (settings, reach): (&FsSettings, &Calls),
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
        let mut running = Running::start((settings, reach), runner, euid, None, epoch).await?;
        let leaving = tokio::select! {
            () = signals.wait() => Leaving::Signal,
            leaving = running.ended() => leaving,
        };
        running.leave(leaving).await
    })
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod exec_tests;
#[cfg(test)]
#[path = "handoff_tests.rs"]
mod handoff_tests;
// The `fs` interface of the one process (`interface.rs`), over the fake mount command of `tests`.
#[cfg(test)]
#[path = "interface_tests.rs"]
mod interface_tests;
#[cfg(test)]
#[path = "restart_tests.rs"]
mod restart_tests;
#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
