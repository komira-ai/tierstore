//! What the mount of the `fs` interface does between its start and its end: whether to take the
//! mount over or to make it, what to tell the service manager, and what ends it
//! (`docs/design/virtual-build-fs.md` sections 7.2 and 7.3): the choice of the start ([`start`]),
//! and what ends a process ([`ended`]), which the `fs` interface of the one process runs
//! (`interface.rs`). Its core calls the node's cache in the process.
//!
//! At a start, in order:
//!
//! 1. A running instance answering on `fs.handoff_socket` is asked for the mount
//!    (`handoff-fs-v1`, `handover.rs`). The descriptor goes into the service manager's store and
//!    the state is read before the running instance is told to stop; the mount is served from the
//!    recorded `INIT` outcome.
//! 2. Otherwise a descriptor the service manager passed under the name `fuse` is the connection of
//!    a process that died or was stopped: it is served from the recorded outcome, and where the
//!    kernel can send requests again (`FUSE_HAS_RESEND`) it is asked to send those the process
//!    before read and did not answer; where it cannot, `fs.recover_wait` bounds the wait for them
//!    (`recover.rs`). A descriptor whose state cannot be read is taken out of the store and the
//!    mount is made afresh, which aborts its connection, so that no job hangs on it.
//! 3. Otherwise the mount is made, and the descriptor is put in the manager's store. A mount of
//!    this file system that the kernel's table of mounts shows at the mount point has no process
//!    serving it by then: it is aborted and detached first, and the mount point is not looked at
//!    (`run.rs`).
//!
//! None of these waits for the announcement of the mount's pins and placements to the node's
//! cache: a thread of the core tells it the pins once the cache takes the write role
//! (`core/live.rs`). What a start still asks of the cache before it is ready is the trees of the
//! attachments file, which it makes again (`start.rs`): none after a boot, when the runtime
//! directory is new. Each step's time is
//! said as it ends (`steps.rs`), on the clock of the platform.
//!
//! The manager is then told the unit is ready, and the process serves until SIGTERM or SIGINT
//! (it leaves the mount for the store to keep, where there is a manager; it unmounts where there is
//! none), until a successor has taken the mount (status 0), or until the connection had to be
//! aborted (status 1: the unit starts again).

use std::os::fd::AsFd as _;
use std::sync::Arc;

use tsfuse::abi::InitFlags;

use super::config::FsSettings;
use super::fills::Calls;
#[cfg(test)]
use super::handover::Offer;
use super::handover::{self, Paths};
use super::recover;
use super::run::{Kernel, Prepared, Running};
use super::sd::{self, FUSE_NAME, Manager, Stored};
#[cfg(test)]
use super::start::Signals;
use super::start::{StartError, io};
use super::steps::Steps;
use crate::{Clock, MonotonicClock};

/// What the platform gives the process: the channel to the service manager and the descriptors it
/// passed.
#[derive(Clone)]
pub(super) struct Platform {
    /// The service manager, if there is one.
    pub manager: Manager,
    /// The descriptors the manager passed.
    pub stored: Vec<Stored>,
    /// Where the kernel shows its mounts and connections.
    pub kernel: Kernel,
    /// The clock the steps of the start are timed on.
    pub clock: Arc<dyn Clock>,
}

/// What the start made.
pub(super) struct Started {
    pub running: Arc<Running>,
    /// The connection to the instance this one took the mount from.
    pub notice: Option<handover::Notice>,
    /// Receives when the watch of a restart has aborted the connection.
    pub hung: Option<tokio::sync::oneshot::Receiver<()>>,
}

/// Serves the connection of the descriptor the service manager kept, and starts the watch of a
/// restart where the kernel cannot send requests again. An error means the connection cannot be
/// served: the start goes on to make a mount.
fn adopt_stored(
    (settings, reach): (&FsSettings, &Calls),
    (stored, kernel): (&Stored, &Kernel),
) -> Result<(Arc<Running>, Option<tokio::sync::oneshot::Receiver<()>>), StartError> {
    let device = sd::take(stored.fd).map_err(|source| StartError::Io {
        what: "taking the descriptor of the store".to_owned(),
        source,
    })?;
    let prepared = Prepared::load(settings)?;
    let running = Running::adopt((settings, reach), prepared, device, (None, true))?;
    // A kernel that can send the requests again and refused to has left them as unanswered as
    // one that cannot.
    let hung =
        if running.outcome().flags.contains(InitFlags::HAS_RESEND) && !running.resend_failed() {
            None
        } else {
            watch(settings, kernel)
        };
    Ok((Arc::new(running), hung))
}

/// Starts the watch for requests no one will answer (`recover.rs`), if the kernel shows the
/// connection of the mount.
fn watch(settings: &FsSettings, kernel: &Kernel) -> Option<tokio::sync::oneshot::Receiver<()>> {
    let number = kernel.connection_of(&settings.mountpoint)?;
    let connection = recover::Connection::new(&kernel.connections, number);
    let (hung, hang) = tokio::sync::oneshot::channel();
    let _ = recover::guard(connection, settings.recover_wait, move || {
        let _ = hung.send(());
    });
    Some(hang)
}

/// Takes the descriptor of the connection out of the service manager's store; a store that cannot
/// be reached is said.
fn unstore(manager: &Manager) {
    if let Err(why) = manager.unstore(FUSE_NAME) {
        tracing::warn!("tierstore fs: cannot take the descriptor out of the store: {why}");
    }
}

/// What a taker of the mount does with the device of an offer before it says `ack`, on either
/// conversation (`take-fs 1`, and the one hand-over, `interface.rs`): it checks that the runtime
/// state of `settings` loads and puts the device in `manager`'s store, so that from `ack` on
/// someone always holds the connection. What it says when it cannot is the reason of the `nak`.
///
/// # Errors
/// The state does not load, or the descriptor cannot be stored.
pub(super) fn prepare(
    settings: &FsSettings,
    manager: &Manager,
    device: &std::os::fd::OwnedFd,
) -> Result<(), String> {
    Prepared::load(settings).map_err(|why| why.to_string())?;
    manager
        .store(FUSE_NAME, device.as_fd())
        .and_then(|()| manager.barrier())
        .map_err(|why| format!("the descriptor cannot be stored: {why}"))
}

/// Makes the start's choice, in this order: take the mount from a running instance, else adopt the
/// descriptor the service manager kept, else mount afresh. The core asks the node's cache and
/// tree store by `reach`.
pub(super) async fn start(
    (settings, reach): (&FsSettings, &Calls),
    platform: &Platform,
    steps: &mut Steps,
) -> Result<Started, StartError> {
    let manager = &platform.manager;
    let paths = Paths {
        socket: &settings.handoff_socket,
        mountpoint: &settings.mountpoint,
        control: &settings.control_socket,
        wait: settings.handoff_wait,
        device: platform.kernel.device,
    };
    // The state is read to see that it can be, before the running instance is told to stop, and
    // read again once it has: it changes until then (a client attaches a tree).
    let taken = handover::take(
        &paths,
        Box::new(|device| prepare(settings, manager, device)),
    )
    .await?;
    let _ = steps.done("asking a running instance for the mount");
    if let Some(taken) = taken {
        let handover::Taken {
            device,
            control,
            notice,
        } = taken;
        let prepared = Prepared::load(settings)?;
        // Nothing reads the device until the running instance's `bye` (`run`).
        let running = Running::take_over((settings, reach), prepared, device, control)?;
        let _ = steps.done("taking the mount over");
        return Ok(Started {
            running: Arc::new(running),
            notice: Some(notice),
            hung: None,
        });
    }
    if let Some(stored) = platform
        .stored
        .iter()
        .find(|stored| stored.name == FUSE_NAME)
    {
        match adopt_stored((settings, reach), (stored, &platform.kernel)) {
            Ok((running, hung)) => {
                let _ = steps.done("serving the descriptor the service manager kept");
                return Ok(Started {
                    running,
                    notice: None,
                    hung,
                });
            }
            Err(why) => {
                // The fresh start below aborts and detaches the mount of the descriptor.
                tracing::error!("tierstore fs: kept connection not served ({why}): remounting");
                unstore(manager);
            }
        }
    }
    let running = Running::start((settings, reach), &platform.kernel)?;
    let stored = running
        .device()
        .and_then(|device| manager.store(FUSE_NAME, device.as_fd()))
        .and_then(|()| manager.barrier());
    if let Err(why) = stored {
        tracing::warn!("tierstore fs: the descriptor is not in the manager's store: {why}");
    }
    let _ = steps.done("making the mount");
    Ok(Started {
        running: Arc::new(running),
        notice: None,
        hung: None,
    })
}

/// Why the process stops serving.
pub(super) enum End {
    /// SIGTERM or SIGINT.
    Signal,
    /// A successor serves the mount.
    Handed,
    /// The connection was aborted.
    Hung,
}

/// Waits for the watch of a restart to report a hang; never, if there is none.
pub(super) async fn hang(hung: Option<tokio::sync::oneshot::Receiver<()>>) {
    if let Some(hung) = hung
        && hung.await.is_ok()
    {
        return;
    }
    std::future::pending().await
}

/// Starts, tells the service manager, and serves until the process ends: the loop of the process
/// that served the mount alone, which the tests run to drive the start, the hand-over and the end
/// that the `fs` interface of the one process (`interface.rs`) is made of.
///
/// # Errors
/// [`StartError`]: why the mount is not served, or why it could not be left.
#[cfg(test)]
async fn run(
    (settings, reach): (&FsSettings, &Calls),
    platform: Platform,
) -> Result<(), StartError> {
    // The handlers are in place before the mount is: a signal after it is never lost.
    let mut signals = Signals::install()?;
    let mut steps = Steps::new(Arc::clone(&platform.clock));
    let Started {
        running,
        notice,
        hung,
    } = start((settings, reach), &platform, &mut steps).await?;
    let manager = &platform.manager;
    let bind = || {
        Offer::bind(
            &settings.handoff_socket,
            &settings.mountpoint,
            &settings.control_socket,
            settings.handoff_wait,
        )
        .map(|offer| offer.with_manager(manager.clone()))
    };
    // The socket is there before the manager hears the unit is ready, so that the instance that
    // is started next (the other letter) finds it. A successor binds after it has told the
    // instance it took the mount from that it serves: that one still holds the path until then.
    let early = if notice.is_none() {
        Some(bind()?)
    } else {
        None
    };
    tracing::info!("tierstore fs: ready after {} ms", steps.total());
    if let Err(why) = manager.ready() {
        tracing::warn!("tierstore fs: cannot tell the service manager it is ready: {why}");
    }
    if let Some(notice) = notice {
        notice.ready().await?;
        // The running instance said `bye`, or ended (a release before the step): it reads the
        // device no more.
        running
            .start_reading()
            .map_err(io("reading the connection taken over"))?;
    }
    let offer = match early {
        Some(offer) => offer,
        None => bind()?,
    };
    let end = tokio::select! {
        () = signals.wait() => End::Signal,
        handed = offer.serve(Arc::clone(&running)) => {
            handed?;
            End::Handed
        }
        () = hang(hung) => End::Hung,
    };
    ended(end, running, manager)
}

/// What the process does once it stops serving for `end`.
///
/// After a hand-over nothing of this process's state is needed: the successor serves the mount,
/// answers every request from here on, and reads what it needs from the runtime directory, all of
/// it written before `done` (`Running::quiesce`). The state is left to the end of the process
/// ([`leave_to_kernel`]) rather than taken apart, so the process ends at once after `ready`.
pub(super) fn ended(end: End, running: Arc<Running>, manager: &Manager) -> Result<(), StartError> {
    let hung = match end {
        End::Handed => {
            leave_to_kernel(running);
            return Ok(());
        }
        End::Signal => false,
        End::Hung => true,
    };
    let busy = || std::io::Error::other("a hand-over is still being answered");
    let alone = Arc::try_unwrap(running).map_err(|_| io("ending")(busy()))?;
    if hung {
        unstore(manager);
        drop(alone);
        return Err(StartError::Aborted);
    }
    if manager.present() {
        alone.leave();
        return Ok(());
    }
    alone.stop()
}

/// Leaves the state of a process that handed its mount over to the end of the process, which
/// releases its memory and closes its descriptors all at once. Taking it apart here would free
/// each of its loaded trees, tables and open files one by one: on a node whose process holds tens
/// of gigabytes, some of it swapped out, that took minutes, which the process spent doing nothing
/// for anyone. Nothing else holds it up: the threads still running (the announcer, the sweeps,
/// the sessions of control clients) are detached, and they end with the process.
pub(super) fn leave_to_kernel(running: Arc<Running>) {
    std::mem::forget(running);
}

/// Serves with `platform`, the core calling the cache and the tree store of `reach`, on a runtime
/// of its own, which the work of the cache of `reach` does not run on.
///
/// # Errors
/// As [`run`].
#[cfg(test)]
pub(super) fn serve_with(
    (settings, reach): (&FsSettings, &Calls),
    platform: Platform,
) -> Result<(), StartError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io("starting the runtime"))?;
    runtime.block_on(run((settings, reach), platform))
}

impl Platform {
    /// The service manager this process was started by, if any, the descriptors it passed, and
    /// the kernel's files of this machine.
    pub(super) fn of_process() -> Self {
        let env = sd::environment();
        Self {
            manager: Manager::new(env.notify_socket.as_deref()),
            stored: env.stored_here(),
            kernel: Kernel::real(),
            clock: Arc::new(MonotonicClock::new()),
        }
    }
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
