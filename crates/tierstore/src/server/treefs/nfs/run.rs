//! The process of `tierstore fs` on macOS: the neutral start (`start.rs`), the NFS and MOUNT
//! protocols on a loopback socket, the mount, and the wait for the signal that ends it.
//!
//! A start, in order: the mount user's number is found; the neutral start is done (a table of tree
//! numbers, the attachments made again, the control socket bound), with a core that counts no
//! lookups, since NFS never forgets; the socket is bound; the protocols are served; and the export
//! is mounted at `fs.mountpoint`, as the mount user. SIGTERM and SIGINT unmount it and end the
//! process with status 0. launchd needs no notification of readiness.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use nfsserve::tcp::{NFSTcp as _, NFSTcpListener};

use super::error::NfsError;
use super::fs::{Patience, TreeNfs};
use super::mount::{Mounter, Runner, System};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::start::{Setup, Signals, io};

/// A patience that is a timer of `after`.
fn timer(after: Duration) -> Patience {
    Arc::new(move || Box::pin(tokio::time::sleep(after)))
}

/// A started NFS frontend: the core, the socket it serves on, and the mount.
pub(super) struct Running {
    _setup: Setup,
    server: tokio::task::JoinHandle<()>,
    /// Set where the mount was made, so that it is unmounted.
    mounted: Option<Mounter>,
}

impl Running {
    /// Does what a start does, up to and including the mount.
    ///
    /// # Errors
    /// [`NfsError`]: why nothing was served or mounted.
    pub(super) async fn start(
        settings: &FsSettings,
        runner: Arc<dyn Runner>,
        euid: u32,
        patience: Option<Patience>,
    ) -> Result<Self, NfsError> {
        let nfs = &settings.nfs;
        let mounter = Mounter::new(runner, nfs, settings.mountpoint.clone(), euid);
        let user = mounter.user_id()?;
        let setup = Setup::start(settings, false)?;
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
        let listener = tokio::net::TcpListener::bind(nfs.listen)
            .await
            .map_err(listen)?;
        let address = listener.local_addr().map_err(listen)?;
        let fs = TreeNfs::new(
            Arc::clone(&setup.core),
            nfs,
            allowed,
            patience.unwrap_or_else(|| timer(nfs.jukebox_after)),
        );
        let listener = NFSTcpListener::from_listener(listener, fs).map_err(listen)?;
        let server = tokio::spawn(async move {
            if let Err(why) = listener.handle_forever().await {
                tracing::error!("tierstore fs: the NFS server stopped: {why}");
            }
        });
        tracing::info!("tierstore fs: serving NFS on {address}");
        let mut running = Self {
            _setup: setup,
            server,
            mounted: None,
        };
        if nfs.mount {
            let task = mounter.clone();
            let made =
                tokio::task::spawn_blocking(move || task.mount(address.ip(), address.port()))
                    .await
                    .map_err(|why| {
                        io("waiting for the mount")(std::io::Error::other(why.to_string()))
                    })?;
            made?;
            tracing::info!("tierstore fs: mounted at {}", settings.mountpoint.display());
            running.mounted = Some(mounter);
        }
        Ok(running)
    }

    /// Unmounts, and stops serving.
    ///
    /// # Errors
    /// [`NfsError::Unmount`] for an unmount that failed.
    pub(super) async fn stop(mut self) -> Result<(), NfsError> {
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

/// Starts over `runner`, and waits for SIGTERM or SIGINT; then unmounts.
///
/// # Errors
/// [`NfsError`]: why nothing was mounted, or why the mount could not be unmounted.
pub(super) fn serve_with(
    settings: &FsSettings,
    runner: Arc<dyn Runner>,
    euid: u32,
) -> Result<(), NfsError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io("starting the runtime"))?;
    runtime.block_on(async {
        // The handlers are in place before the mount is: a signal after it is never lost.
        let mut signals = Signals::install()?;
        let running = Running::start(settings, runner, euid, None).await?;
        signals.wait().await;
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
    )
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
