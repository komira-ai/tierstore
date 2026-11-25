//! The `fs` interface of the one process on macOS (`docs/design/one-service.md`, sections 2.3, 3.4
//! and 3.5): the NFS front a build node's `tierstore` serves itself when its file has an
//! `[[interface]]` of type `fs`, with a core that calls the node's cache and tree store
//! (`in_process.rs`) instead of asking a fill socket. Linux serves the same interface by FUSE
//! (`interface.rs` of the parent).
//!
//! The start (`cache_cli.rs`) opens the cache and the tree store and starts the front before it
//! binds or takes a socket of a front, in the order of 2.3 with the NFS front in place of FUSE:
//! the table of tree numbers of this boot is reopened (or made), the attachments the file holds are
//! made again, the NFS listener is bound on `params.nfs.listen` and the control socket at
//! `params.control_socket` (or both are those the image before this one handed over), the
//! protocols are served, and the export is mounted at the mount point as `params.nfs.mount_user`,
//! unless the listing of `mount` shows it mounted there already, as a restart, a crash or the image
//! before this one left it: then it is not made again ([`Running::start_from`]). There is no
//! running instance to take anything from: launchd runs one process of the job.
//!
//! A stop drains the fronts first and serves the mount throughout, since a fill a job waits for may
//! need the upstream; then it unmounts while the NFS front still answers, and only then ends the
//! serving ([`Running::leave`]). A stop that hands over to the next image of the program (SIGUSR2)
//! drains the fronts the same way, and then ends the NFS connections as a hand-over does (every
//! request read is answered, the write side of each connection shut down, the clients waited for)
//! and leaves the mount as it is: the NFS listening socket and the control socket go into the offer
//! (`nfs` and `control` lines, [`Running::hand_to_image`]), and the next image serves on them.
//!
//! A start that fails once the front serves (a socket of a front cannot be bound) leaves the mount
//! where it is and ends the serving: launchd starts the job again, and that start finds the mount
//! and serves it on the same address. An NFS server that stops by itself stops the fronts as a
//! signal does, unmounts, and the process ends in failure.

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::Notify;

use super::boot::BootSource;
use super::error::NfsError;
use super::mount::Runner;
use super::run::{Leaving, Running, Serve};
use crate::server::handoff::{Handed, Handing};
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::trees::TreeStore;
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::fills::{Calls, Reach};

/// The `fs` interface of a build node's file, and what its NFS front runs on: the commands of the
/// mount, the user of the process, the boot, and the server of the protocol.
pub(in crate::server) struct NfsService {
    settings: FsSettings,
    runner: Arc<dyn Runner>,
    euid: u32,
    boot: BootSource,
    serve: Serve,
}

impl NfsService {
    /// The `fs` interface `settings`, on this machine.
    #[cfg(target_os = "macos")]
    pub(in crate::server) fn of_process(settings: FsSettings) -> Self {
        Self {
            settings,
            runner: Arc::new(super::mount::System),
            euid: rustix::process::geteuid().as_raw(),
            boot: BootSource::system(),
            serve: super::run::handle,
        }
    }

    /// The same, over `runner`, as `euid`, in the boot `boot` names, served by `serve`.
    #[cfg(test)]
    pub(super) fn on(
        settings: FsSettings,
        (runner, euid, boot): (Arc<dyn Runner>, u32, BootSource),
        serve: Serve,
    ) -> Self {
        Self {
            settings,
            runner,
            euid,
            boot,
            serve,
        }
    }

    /// The address the NFS front serves on: what a handed offer's `nfs` line must cover.
    pub(in crate::server) fn listen(&self) -> std::net::SocketAddr {
        self.settings.nfs.listen
    }

    /// The control socket: where a handed offer's `control` line must be.
    pub(in crate::server) fn control_socket(&self) -> &std::path::Path {
        &self.settings.control_socket
    }

    /// Starts the NFS front with a core that calls `cache` and `trees`, and mounts the export
    /// where it is not mounted yet; from the NFS listener and the control socket of `handed`, the
    /// file system's part of the offer the image before this one handed over, when it did.
    ///
    /// # Errors
    /// [`NfsError`]: why nothing is served; a front that served and could not record its mount
    /// has unmounted it.
    pub(in crate::server) async fn start(
        self,
        (cache, trees): (LocalCache, Option<TreeStore>),
        handed: Option<Handed>,
    ) -> Result<NfsServed, NfsError> {
        let Self {
            settings,
            runner,
            euid,
            boot,
            serve,
        } = self;
        let runtime = Handle::current();
        let reach = Reach::Calls(Calls {
            cache,
            trees,
            runtime: runtime.clone(),
        });
        // The start waits for the cache's answers (the attachments are made again through it),
        // which are worked out on the runtime: it runs on a thread of its own.
        let started = tokio::task::spawn_blocking(move || {
            let epoch = boot.epoch()?;
            let ways = (runner, euid, None, epoch);
            runtime.block_on(Running::start_from(
                (&settings, &reach),
                ways,
                serve,
                handed,
            ))
        })
        .await
        .unwrap_or_else(|panicked| std::panic::resume_unwind(panicked.into_panic()))?;
        Ok(NfsServed { running: started })
    }
}

/// The NFS front a start serves.
pub(in crate::server) struct NfsServed {
    running: Running,
}

impl NfsServed {
    /// Ends the serving and leaves the mount where it is, for the next start: what a start does
    /// when it cannot go on once the front serves (a socket of a front cannot be bound).
    pub(in crate::server) fn abandon(self) {
        tracing::warn!(
            "tierstore fs: the start failed after the NFS front served; the mount is left for the \
             next start"
        );
        drop(self.running);
    }

    /// Serves until `drained` is notified, when the fronts have drained; then hands the front
    /// over when `handing` says the stop is a hand-over to the next image, and otherwise unmounts
    /// while the front still answers and ends the serving. An NFS server that stops by itself
    /// before notifies `failed`, so that the fronts stop as at a signal, and is unmounted.
    ///
    /// # Errors
    /// [`NfsError::Stopped`] for a server that stopped by itself; what the hand-over or the
    /// unmount said.
    pub(in crate::server) async fn serve(
        self,
        drained: Arc<Notify>,
        failed: Arc<Notify>,
        handing: impl FnOnce() -> bool,
    ) -> Result<Option<Handing>, NfsError> {
        let Self { mut running } = self;
        let stopped = tokio::select! {
            () = drained.notified() => None,
            leaving = running.ended() => Some(leaving),
        };
        match stopped {
            None if handing() => running.hand_to_image().await.map(Some),
            None => running.leave(Leaving::Signal).await.map(|()| None),
            Some(leaving) => {
                failed.notify_one();
                let why = leaving.to_string();
                running.leave(leaving).await?;
                Err(NfsError::Stopped(why))
            }
        }
    }
}
