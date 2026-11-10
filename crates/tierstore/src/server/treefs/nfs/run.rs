//! The process of `tierstore fs` on macOS: the neutral start (`start.rs`), the NFS and MOUNT
//! protocols on a loopback socket, the mount, and the wait for the signal that ends it.
//!
//! A start, in order: the mount user's number is found; if `fs.nfs.handoff_socket` is given, a
//! running instance is asked for its listening socket (`handoff`); the neutral start is done (the
//! table of tree numbers of this boot reopened if there is one, the attachments made again, the
//! control socket bound), with a core that counts no lookups, since NFS never forgets; the socket
//! is the running instance's or is bound; the protocols are served; and the export is mounted at
//! `fs.mountpoint`, as the mount user, unless something is mounted there already (a mount outlives
//! the server: a restart, or the instance this one replaces, left it, and its clients wait).
//! SIGTERM and SIGINT unmount it and end the process with status 0. launchd needs no notification
//! of readiness.
//!
//! An instance with a control socket offers its listening socket to the next instance. When that
//! takes it, this one stops accepting, answers the requests it has read, shuts down the write side
//! of each connection (the clients reconnect to the new instance, which holds the socket), waits
//! for them to close for at most `fs.nfs.drain_wait`, and ends with status 0 without unmounting:
//! the mount is the new instance's now.

use std::collections::HashSet;
use std::os::fd::AsFd as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nfsserve::tcp::{NFSTcp as _, NFSTcpListener};
use tokio::sync::{Notify, oneshot};

use super::boot::BootSource;
use super::error::NfsError;
use super::fs::{Patience, TreeNfs};
use super::mount::{Mounter, Runner, System};
use crate::server::handoff::{Front, Offer, Socket, Want, check_control_path, take};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::start::{Identity, Setup, Signals, io};

/// A patience that is a timer of `after`.
fn timer(after: Duration) -> Patience {
    Arc::new(move || Box::pin(tokio::time::sleep(after)))
}

/// A started NFS frontend: the core, the socket it serves on, and the mount.
pub(super) struct Running {
    _setup: Setup,
    server: tokio::task::JoinHandle<()>,
    /// Ends the server: a stop. A hand-over ends it by itself.
    stopping: Option<oneshot::Sender<()>>,
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
        let nfs = &settings.nfs;
        let mounter = Mounter::new(runner, nfs, settings.mountpoint.clone(), euid);
        let user = mounter.user_id()?;
        let handoff = nfs.handoff.as_ref();
        if let Some(handoff) = handoff {
            check_control_path(&handoff.socket)?;
        }
        // The running instance, if there is one, hands its socket over before this start binds
        // the control socket or opens the table: it holds both until then.
        let taken = match handoff {
            Some(handoff) => {
                take(
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
        let allowed: HashSet<u32> = nfs
            .allowed_uids
            .clone()
            .unwrap_or_else(|| vec![user, settings.owner.0])
            .into_iter()
            .collect();
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
        let offer = match (nfs.handoff.as_ref(), inherited) {
            (Some(handoff), Some(taken)) => offer_after_takeover(&handoff.socket, taken.copies),
            (Some(handoff), None) => {
                let copy = std::net::TcpListener::from(
                    listener.as_fd().try_clone_to_owned().map_err(listen)?,
                );
                Some(Offer::bind(
                    &handoff.socket,
                    vec![Socket::new(Front::Nfs, copy)],
                )?)
            }
            (None, _) => None,
        };
        let fs = TreeNfs::new(
            Arc::clone(&setup.core),
            nfs,
            allowed,
            patience.unwrap_or_else(|| timer(nfs.jukebox_after)),
            epoch,
        );
        let listener = NFSTcpListener::from_listener(listener, fs).map_err(listen)?;
        let (stopping, stop) = oneshot::channel();
        let handed = Arc::new(AtomicBool::new(false));
        let server = tokio::spawn(serve_until(
            listener,
            offer,
            stop,
            Arc::clone(&handed),
            nfs.drain_wait,
        ));
        let numbers = if reopened { "kept" } else { "made afresh" };
        tracing::info!(
            "tierstore fs: serving NFS on {address}, epoch {epoch:08x}, tree numbers {numbers}"
        );
        let mut running = Self {
            _setup: setup,
            server,
            stopping: Some(stopping),
            handed,
            #[cfg(test)]
            address,
            mounted: None,
        };
        if nfs.mount {
            let task = mounter.clone();
            let made = tokio::task::spawn_blocking(move || {
                if task.is_mounted()? {
                    return Ok(false);
                }
                task.mount(address.ip(), address.port()).map(|()| true)
            })
            .await
            .map_err(|why| io("waiting for the mount")(std::io::Error::other(why.to_string())))?;
            let point = settings.mountpoint.display().to_string();
            if made? {
                tracing::info!("tierstore fs: mounted at {point}");
            } else {
                tracing::info!(
                    "tierstore fs: {point} is mounted already: the clients' mount goes on"
                );
            }
            running.mounted = Some(mounter);
        }
        Ok(running)
    }

    /// Resolves once a successor has taken the listening socket and this instance has answered
    /// what it owed; never if the server ended otherwise.
    pub(super) async fn handed_over(&mut self) {
        let _ = (&mut self.server).await;
        if !self.handed.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }

    /// Stops serving, and unmounts: unless the sockets were handed over, when the mount is the
    /// successor's.
    ///
    /// # Errors
    /// [`NfsError::Unmount`] for an unmount that failed.
    pub(super) async fn stop(mut self) -> Result<(), NfsError> {
        if let Some(stopping) = self.stopping.take() {
            let _ = stopping.send(());
        }
        if self.handed.load(Ordering::SeqCst) {
            return Ok(());
        }
        match self.mounted.take() {
            Some(mounter) => tokio::task::spawn_blocking(move || mounter.unmount())
                .await
                .map_err(|why| {
                    io("waiting for the unmount")(std::io::Error::other(why.to_string()))
                })?,
            None => Ok(()),
        }
    }
}

impl Drop for Running {
    /// A start that fails after the socket is served, and a stop, end the server.
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The control socket of a process that took its listening socket from the one it replaces: the
/// sockets it took are what it offers to its own successor. A process that cannot make the control
/// socket says so and serves without one, since the update has happened and cannot be undone.
fn offer_after_takeover(socket: &std::path::Path, copies: Vec<Socket>) -> Option<Offer> {
    Offer::bind(socket, copies)
        .map_err(|why| {
            tracing::warn!(
                "tierstore fs: {why}; this instance serves without a control socket and \
                 cannot be handed over until it restarts"
            );
        })
        .ok()
}

/// Serves on `listener` until `stop` resolves or `offer` is taken by a successor, then lets the
/// connections finish (`NFSTcp::handle_until`), for at most `drain_wait` after that.
async fn serve_until(
    listener: NFSTcpListener<TreeNfs>,
    offer: Option<Offer>,
    stop: oneshot::Receiver<()>,
    handed: Arc<AtomicBool>,
    drain_wait: Duration,
) {
    let stopped = Arc::new(Notify::new());
    let signal = Arc::clone(&stopped);
    let ending = async move {
        match offer {
            Some(offer) => tokio::select! {
                _ = stop => {}
                () = offer.serve() => {
                    handed.store(true, Ordering::SeqCst);
                    tracing::info!("tierstore fs: the listening socket was taken");
                }
            },
            None => {
                let _ = stop.await;
            }
        }
        signal.notify_one();
    };
    let serving = listener.handle_until(ending);
    let patience = async {
        stopped.notified().await;
        tokio::time::sleep(drain_wait).await;
    };
    tokio::select! {
        served = serving => {
            if let Err(why) = served {
                tracing::error!("tierstore fs: the NFS server stopped: {why}");
            }
        }
        () = patience => {}
    }
}

/// Starts over `runner`, in the boot `boot` names, and waits for SIGTERM or SIGINT; then unmounts.
///
/// # Errors
/// [`NfsError`]: why nothing was mounted, or why the mount could not be unmounted.
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
        tokio::select! {
            () = signals.wait() => {}
            () = running.handed_over() => {}
        }
        running.stop().await
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
#[path = "restart_tests.rs"]
mod restart_tests;
#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
