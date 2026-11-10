//! A FUSE session being served: the readers of the `/dev/fuse` descriptor, and what a process does
//! to them across a crash, a restart and a hand-over (`docs/design/virtual-build-fs.md` sections
//! 7.2 and 7.3).
//!
//! Three ways to have one: [`Serving::mount`] makes the mount and the handshake; [`Serving::adopt`]
//! serves a connection the kernel has already set up, from the descriptor and the recorded
//! outcome of its `INIT` (the descriptor came from the service manager's store or from the
//! running instance); [`Serving::restart`] serves it again after [`Serving::stop_reading`].
//!
//! The readers poll the descriptor, which is non-blocking, and a stop pipe: [`Serving::stop_reading`]
//! ends them with every request they had read passed to the core and answered. The descriptor stays
//! open, in this process and wherever else it is held, so the kernel keeps the connection; a
//! request that arrives meanwhile waits in the kernel for the next reader.

use std::io;
use std::os::fd::{AsFd as _, OwnedFd};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use fuser::{BackgroundSession, InitFlags, InitOutcome, Notifier, Session, SessionStopper};

use super::backing::{Guard, Saved};
use super::config::FsSettings;
use super::core::Core;
use super::fuse::{self, Frontend};

/// The readers of one connection.
pub(super) struct Serving {
    stopper: SessionStopper,
    /// The session's thread; none once the readers have stopped.
    session: Option<BackgroundSession>,
    /// Sent on when the readers have stopped.
    ended: Receiver<()>,
    /// This process's own copy of the descriptor, to store, to hand over and to serve again.
    device: OwnedFd,
    notifier: Notifier,
    outcome: InitOutcome,
    /// Whether this process made the mount, and so unmounts it.
    mounted: bool,
}

/// A frontend over `core`, and the channel it tells when the session ends.
fn frontend(core: &Arc<Core>, settings: &FsSettings) -> (Frontend, Receiver<()>) {
    let (ended, hear) = channel();
    let frontend = Frontend::new(Arc::clone(core), settings.passthrough).telling(ended);
    (frontend, hear)
}

impl Serving {
    /// Mounts at `fs.mountpoint` and runs the handshake with the kernel; the readers start.
    ///
    /// # Errors
    /// What the system said when the mount, the handshake or the threads fail.
    pub(super) fn mount(core: &Arc<Core>, settings: &FsSettings) -> io::Result<Self> {
        let (frontend, ended) = frontend(core, settings);
        let config = fuse::config(settings.threads, settings.allow_other);
        let session = Session::new(frontend, &settings.mountpoint, &config)?;
        let outcome = session
            .init_outcome()
            .ok_or_else(|| io::Error::other("the handshake left no outcome"))?;
        Self::start(session, ended, (outcome, true))
    }

    /// Serves the connection of `device`, which the kernel set up with `outcome`. Where the
    /// connection has passthrough, the core takes over `saved`, the backing ids of the process
    /// before this one; `None` leaves the core's table as it is (it already has the ids).
    ///
    /// # Errors
    /// What the system said when the threads or the stop pipe cannot be made.
    pub(super) fn adopt(
        core: &Arc<Core>,
        settings: &FsSettings,
        device: OwnedFd,
        outcome: InitOutcome,
        saved: Option<&[Saved]>,
    ) -> io::Result<Self> {
        let (frontend, ended) = frontend(core, settings);
        let config = fuse::config(settings.threads, settings.allow_other);
        let session = Session::from_fd_initialized(frontend, device, config.acl, config, outcome);
        if let (Some(saved), true) = (saved, outcome.flags.contains(InitFlags::FUSE_PASSTHROUGH)) {
            let notifier = session.notifier();
            let wrap = move |id| -> Guard { Arc::new(notifier.backing_id_from_raw(id)) };
            core.adopt_passthrough(saved, &wrap);
        }
        Self::start(session, ended, (outcome, false))
    }

    /// Starts the readers of `session`.
    fn start(
        mut session: Session<Frontend>,
        ended: Receiver<()>,
        (outcome, mounted): (InitOutcome, bool),
    ) -> io::Result<Self> {
        let stopper = session.stopper()?;
        let device = session.as_fd().try_clone_to_owned()?;
        let notifier = session.notifier();
        let session = session.spawn()?;
        Ok(Self {
            stopper,
            session: Some(session),
            ended,
            device,
            notifier,
            outcome,
            mounted,
        })
    }

    /// The outcome of the handshake this connection was set up with.
    pub(super) fn outcome(&self) -> InitOutcome {
        self.outcome
    }

    /// A copy of the descriptor of the connection.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn device(&self) -> io::Result<OwnedFd> {
        self.device.try_clone()
    }

    /// Whether the connection was set up with `FUSE_NOTIFY_RESEND` available.
    pub(super) fn can_resend(&self) -> bool {
        self.outcome.flags.contains(InitFlags::FUSE_HAS_RESEND)
    }

    /// Asks the kernel to send again the requests the process before this one read and did not
    /// answer. Where the connection was not set up for it, nothing is asked.
    ///
    /// # Errors
    /// What the kernel said.
    pub(super) fn resend(&self) -> io::Result<()> {
        if self.can_resend() {
            self.notifier.resend()
        } else {
            Ok(())
        }
    }

    /// Stops the readers and returns when each has answered the request it held and stopped. The
    /// connection stays open. Calling it again, with no restart between, does nothing. A stop
    /// that cannot be signalled means the readers are gone already (their end of the stop pipe is
    /// closed), and a reader that ended without saying so is one that is gone too.
    pub(super) fn stop_reading(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let _ = self.stopper.stop().map(|()| self.ended.recv());
        // The thread has nothing left to do but return. It is not joined: joining would drop the
        // session's handle to the mount, which unmounts it.
        std::mem::forget(session);
    }

    /// Serves the connection again after [`Serving::stop_reading`], from the same descriptor and
    /// outcome: the hand-over to a successor that did not complete.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn restart(&mut self, core: &Arc<Core>, settings: &FsSettings) -> io::Result<()> {
        self.stop_reading();
        // The new readers have no handle on the mount, if there was one: it was left behind with
        // the old (`stop_reading`), so a later `unmount` leaves the mount.
        *self = Self::adopt(core, settings, self.device.try_clone()?, self.outcome, None)?;
        Ok(())
    }

    /// Ends the readers, and unmounts what this process mounted. The readers of a mount are not
    /// stopped first: the kernel asks the server to answer the `DESTROY` of an unmount.
    ///
    /// # Errors
    /// What the system said when the mount could not be unmounted.
    pub(super) fn unmount(mut self) -> io::Result<()> {
        if !self.mounted {
            self.stop_reading();
            return Ok(());
        }
        self.session
            .take()
            .map_or(Ok(()), BackgroundSession::umount_and_join)
    }
}

#[cfg(test)]
#[path = "serving_tests.rs"]
mod tests;
