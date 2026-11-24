//! The `fs` interface of the one process (`docs/design/one-service.md`, sections 2.3 and 3.5):
//! the mount a build node's `tierstore` serves itself when its file has an `[[interface]]` of type
//! `fs`, with a core that calls the node's cache and tree store (`in_process.rs`) instead of
//! asking a fill socket. Linux only: the mount is a FUSE mount.
//!
//! The start (`cache_cli.rs`) opens the cache and the tree store and then gets the mount, before it
//! binds or takes a socket of a front, in the order of `tierstore fs` (`launch.rs`):
//!
//! 1. **Take** it from a running instance that answers `take-fs 1` on `handoff.sock` in the
//!    runtime directory (`handover.rs`): a `tierstore fs` process of the two-process layout, or a
//!    process of this layout, which answers there too. In the switch from two processes to one,
//!    the start then takes the S3 and OCI sockets from the old `tierstore` with `take 1`, as any
//!    start of a build node does: the mount comes from the process that holds it.
//! 2. Else **adopt** the descriptor the service manager kept under the name `fuse`.
//! 3. Else **mount afresh**: a mount of this file system that the kernel's table of mounts shows
//!    at the mount point has no server, and is aborted and detached, with no look at the mount
//!    point.
//!
//! A running instance that accepts on the socket and does not answer fails the start after
//! `params.handoff_wait`, before anything is detached or mounted. The mount is served, and the
//! instance it was taken from told `ready`, before the start takes the sockets of the fronts; the
//! start then binds `handoff.sock` itself, and the service manager is told `READY=1` once the
//! fronts serve too.
//!
//! A start that fails once the mount is served (the hand-over socket, or a socket of a front, cannot
//! be bound or taken) stops reading the connection and leaves the mount where it is, for the next
//! start: it never unmounts it, since the instance it was taken from may serve it again.
//!
//! A stop drains the fronts first and serves the mount throughout, since a fill a job waits for may
//! need the upstream; then the readers stop and the mount is left to the service manager's store
//! (a process with no manager unmounts, as `tierstore fs` does). A process whose mount a successor
//! took goes on serving S3 and OCI until the successor takes their sockets, and leaves its state of
//! the mount to the end of the process. A mount that cannot be served on (the connection a restart
//! adopted was aborted because requests stayed unanswered, or a hand-over that did not complete
//! left the readers stopped) is let go at once, as `tierstore fs` lets it go, since no request can
//! be answered on it; it stops the fronts as a signal does, and the process ends in failure, so
//! that the unit starts again.

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::server::local_cache::LocalCache;
use crate::server::local_cache::trees::TreeStore;

use super::config::FsSettings;
use super::fills::{Calls, Reach};
use super::handover::Offer;
use super::launch::{self, End, Platform, Started, ended, hang, leave_to_kernel};
use super::run::Running;
use super::sd::Manager;
use super::start::StartError;
use super::steps::Steps;

/// What the start of a build node asks of the platform: its service manager, and the `fs`
/// interface of its file, if any.
pub(in crate::server) struct Service {
    fs: Option<FsSettings>,
    platform: Platform,
}

impl Service {
    /// The service manager this process was started by, if any, and the `fs` interface `fs`.
    pub(in crate::server) fn of_process(fs: Option<FsSettings>) -> Self {
        Self {
            fs,
            platform: Platform::of_process(),
        }
    }

    /// The same, on `platform`.
    #[cfg(test)]
    pub(super) fn on(fs: Option<FsSettings>, platform: Platform) -> Self {
        Self { fs, platform }
    }

    /// Gets the mount of the `fs` interface, if the file has one: taken, adopted or made (see the
    /// module), with a core that calls `cache` and `trees`, and serves it.
    ///
    /// # Errors
    /// [`StartError`]: why the mount is not served; nothing was detached or mounted when a running
    /// instance refused, did not keep to the protocol or did not answer in time.
    pub(in crate::server) async fn start(
        self,
        (cache, trees): (LocalCache, Option<TreeStore>),
    ) -> Result<Served, StartError> {
        let Self { fs, platform } = self;
        let manager = platform.manager.clone();
        let Some(settings) = fs else {
            return Ok(Served {
                manager,
                mount: None,
            });
        };
        let runtime = Handle::current();
        let reach = Reach::Calls(Calls {
            cache,
            trees,
            runtime: runtime.clone(),
        });
        let mine = settings.clone();
        // The start waits for the cache's answers (the attachments are made again through it),
        // which are worked out on the runtime: it runs on a thread of its own.
        let started = tokio::task::spawn_blocking(move || {
            let mut steps = Steps::new(Arc::clone(&platform.clock));
            runtime.block_on(launch::start((&mine, &reach), &platform, &mut steps))
        })
        .await
        .unwrap_or_else(|panicked| std::panic::resume_unwind(panicked.into_panic()))?;
        let Started {
            running,
            notice,
            hung,
        } = started;
        // A mount taken over is read only once the instance it came from has said `bye`, or
        // ended (a release before the step): it reads the device no more.
        let told = match &notice {
            Some(notice) => match notice.ready().await {
                Ok(()) => running
                    .start_reading()
                    .map_err(super::start::io("reading the connection taken over")),
                Err(why) => Err(StartError::from(why)),
            },
            None => Ok(()),
        };
        let offer = told.and_then(|()| {
            Offer::bind(
                &settings.handoff_socket,
                &settings.mountpoint,
                &settings.control_socket,
                settings.handoff_wait,
            )
            .map(|offer| offer.with_manager(manager.clone()))
            .map_err(StartError::from)
        });
        let offer = match offer {
            Ok(offer) => offer,
            Err(why) => {
                abandon(running).await;
                return Err(why);
            }
        };
        Ok(Served {
            manager,
            mount: Some(Mount {
                running,
                offer,
                hung,
            }),
        })
    }
}

/// Stops reading the connection of `running`, off the runtime's threads, and leaves the mount
/// where it is: to the service manager's store, which holds the descriptor, or, without a
/// manager, to the next start, which aborts and detaches a mount no one serves. It is never
/// unmounted: the instance it was taken from may serve it again (a taker gone before `ready`
/// leaves that instance serving). What a start does with a mount it served and cannot go on with.
async fn abandon(running: Arc<Running>) {
    let leaving = tokio::task::spawn_blocking(move || match Arc::try_unwrap(running) {
        Ok(alone) => alone.leave(),
        Err(_shared) => tracing::warn!("tierstore fs: the mount is still being handed over"),
    });
    leaving
        .await
        .unwrap_or_else(|panicked| std::panic::resume_unwind(panicked.into_panic()));
}

/// The mount a start serves.
struct Mount {
    running: Arc<Running>,
    /// The answer to a successor that asks for the mount (`take-fs 1`).
    offer: Offer,
    /// Receives when the watch of a restart has aborted the connection.
    hung: Option<tokio::sync::oneshot::Receiver<()>>,
}

/// What a start of a build node got from the platform: the service manager, and the mount of the
/// `fs` interface, if the file has one.
pub(in crate::server) struct Served {
    manager: Manager,
    mount: Option<Mount>,
}

impl Served {
    /// Tells the service manager that the unit is ready (`READY=1`); a manager that cannot be told
    /// is said, and the process goes on.
    pub(in crate::server) fn ready(&self) {
        if let Err(why) = self.manager.ready() {
            tracing::warn!("tierstore: cannot tell the service manager it is ready: {why}");
        }
    }

    /// Leaves the mount, as [`abandon`] does, when the start fails after it was served (a socket of
    /// a front that cannot be bound or taken); nothing without a mount.
    pub(in crate::server) async fn abandon(self) {
        if let Some(mount) = self.mount {
            drop(mount.offer);
            abandon(mount.running).await;
        }
    }

    /// Serves the mount until `drained` is notified, when the fronts have drained, and then leaves
    /// it (section 3.5); meanwhile answers a successor that asks for it. `failed` is notified when
    /// the mount cannot be served on, so that the fronts stop as at a signal. Returns at once
    /// without a mount.
    ///
    /// # Errors
    /// [`StartError::Aborted`] for a connection aborted because requests stayed unanswered;
    /// [`StartError::Handover`] for a hand-over that left the readers stopped; what the system said
    /// when the mount could not be unmounted.
    pub(in crate::server) async fn serve(
        self,
        drained: Arc<Notify>,
        failed: Arc<Notify>,
    ) -> Result<(), StartError> {
        let Self { manager, mount } = self;
        let Some(Mount {
            running,
            offer,
            hung,
        }) = mount
        else {
            return Ok(());
        };
        let end = tokio::select! {
            handed = offer.serve(Arc::clone(&running)) => match handed {
                Ok(()) => End::Handed,
                Err(why) => {
                    // The store keeps the descriptor for the next start, as at the end of a
                    // `tierstore fs` process that could not serve again.
                    failed.notify_one();
                    leave_to_kernel(running);
                    return Err(why.into());
                }
            },
            () = hang(hung) => {
                failed.notify_one();
                End::Hung
            }
            () = drained.notified() => End::Signal,
        };
        if matches!(end, End::Handed) {
            tracing::info!(
                "tierstore fs: a successor serves the mount; S3 and OCI are served until it takes them"
            );
        }
        let leaving = tokio::task::spawn_blocking(move || ended(end, running, &manager));
        leaving
            .await
            .unwrap_or_else(|panicked| std::panic::resume_unwind(panicked.into_panic()))
    }
}

#[cfg(test)]
#[path = "interface_tests.rs"]
mod tests;
