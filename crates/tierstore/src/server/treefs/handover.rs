//! What the one hand-over needs of the mount (`docs/design/one-service.md` section 3.1;
//! `server/handoff/` holds the conversation, `take 1`, `ready`, `mount`): the mount of a running
//! [`Running`] as that conversation offers it ([`offered`]), the check of the kind of the device a
//! taker is offered ([`check_device`]), and what a resume that cannot serve the mount again tells
//! the start ([`Stuck`], [`HandoverError::Stuck`]).
//!
//! There is no conversation of the mount's own: the `take-fs 1` of the two-process layout, which
//! was asked on `handoff.sock` in the runtime directory, is neither offered nor asked any more
//! (`handoff-fs-v1` is no longer listed). An instance that holds the mount hands it over only
//! with its sockets and its lock, in the one hand-over.

use std::io::ErrorKind;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use crate::server::handoff::{Claim, Mount};

use super::run::Running;
use super::sd::{FUSE_NAME, Manager};

/// Why the mount could not be served on after a hand-over.
#[derive(Debug, thiserror::Error)]
pub(in crate::server) enum HandoverError {
    /// The running instance could not serve again after a hand-over that did not complete.
    #[error("hand-over: the instance could not serve again: {0}")]
    Stuck(std::io::Error),
}

/// The FUSE character device (`/dev/fuse`): its major and minor numbers, which the kernel fixes
/// (`MISC_MAJOR`, `FUSE_MINOR`).
const FUSE_DEVICE: (u32, u32) = (10, 229);

/// Whether a file of `mode` and device number `rdev` is the FUSE device.
pub(super) fn is_fuse(mode: u32, rdev: u64) -> bool {
    let character =
        rustix::fs::FileType::from_raw_mode(mode) == rustix::fs::FileType::CharacterDevice;
    character && (rustix::fs::major(rdev), rustix::fs::minor(rdev)) == FUSE_DEVICE
}

/// Checks that `fd` is the FUSE device, as the offer of a mount says it is.
///
/// # Errors
/// `InvalidData` when it is not; what the system said when it cannot be asked.
pub(in crate::server) fn check_device(fd: BorrowedFd<'_>) -> std::io::Result<()> {
    let stat = rustix::fs::fstat(fd)?;
    is_fuse(stat.st_mode, stat.st_rdev)
        .then_some(())
        .ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidData,
                "the offered device is not the FUSE device",
            )
        })
}

/// What a resume that cannot serve the mount again is told: the error.
pub(super) type Stuck = Arc<dyn Fn(&std::io::Error) + Send + Sync>;

/// The mount of `running`, served with `settings`, as the one hand-over offers it
/// (`server/handoff/`: `take 1`, `ready`, `mount`): shared and quiesced at `ack`, resumed when the
/// taker does not complete, within `settings.handoff_wait`, and at `ready` given up for good, with
/// the descriptor's entry of `manager`'s store removed. `claim` tells the instance's `serve` that
/// the mount was given up. A resume that cannot serve the mount again calls `stuck` with the
/// error: the start stops the process there, so that its unit is started again and adopts the
/// descriptor from its store.
pub(in crate::server) fn offered(
    running: &Arc<Running>,
    settings: &super::config::FsSettings,
    manager: Manager,
    (stuck, claim): (Stuck, Arc<Claim>),
) -> Mount {
    // Weak: the offer that holds these does not keep the mount's state alive, so a process that
    // ends its service of the mount (a hang, a stop) takes it whole whatever the offer is doing.
    let weak = Arc::downgrade(running);
    let (sharing, quiescing, resuming, leaving) = (weak.clone(), weak.clone(), weak.clone(), weak);
    Mount {
        mountpoint: settings.mountpoint.clone(),
        control: settings.control_socket.clone(),
        share: Arc::new(move || alive(&sharing)?.share()),
        quiesce: Arc::new(move || drop(alive(&quiescing).map(|running| running.quiesce()))),
        resume: Arc::new(move || {
            let resumed = alive(&resuming).map(|running| running.resume());
            stuck_on(resumed.unwrap_or(Ok(())), &*stuck);
        }),
        give_up: Arc::new(move || {
            drop(alive(&leaving).map(|running| running.give_up()));
            if let Err(why) = manager.unstore(FUSE_NAME) {
                tracing::warn!("tierstore: the descriptor's store entry was not removed: {why}");
            }
        }),
        wait: settings.handoff_wait,
        claim,
    }
}

/// The mount's state, while the process still serves it.
///
/// # Errors
/// `NotFound`: the process has ended its service of the mount.
fn alive(running: &std::sync::Weak<Running>) -> std::io::Result<Arc<Running>> {
    running.upgrade().ok_or_else(|| {
        std::io::Error::new(ErrorKind::NotFound, "the mount is no longer served here")
    })
}

/// Calls `stuck` with the error of a resume that failed: the mount cannot be served again.
pub(super) fn stuck_on(resumed: std::io::Result<()>, stuck: &dyn Fn(&std::io::Error)) {
    if let Err(why) = resumed {
        tracing::error!("tierstore: the mount cannot be served again: {why}");
        stuck(&why);
    }
}
