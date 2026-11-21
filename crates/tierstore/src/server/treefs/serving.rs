//! A FUSE session being served: the readers of the `/dev/fuse` descriptor, and what a process does
//! to them across a crash, a restart and a hand-over (`docs/design/virtual-build-fs.md` sections
//! 7.2 and 7.3). The library is `tsfuse` (`docs/design/fetch-cache-serve.md` 5.2).
//!
//! Three ways to have one: [`Serving::mount`] makes the mount and the handshake; [`Serving::adopt`]
//! serves a connection the kernel has already set up, from the descriptor and the recorded
//! outcome of its `INIT` (the descriptor came from the service manager's store or from the
//! running instance); [`Serving::restart`] serves it again after [`Serving::stop_reading`].
//!
//! The connection has one descriptor, the one the service manager keeps and a hand-over passes:
//! `fs.threads` readers each read a `dup` of it, so every reader has the one processing queue of
//! that descriptor, and the kernel's requests a process read and did not answer stay on the
//! descriptor that outlives it (no clone a supervisor does not hold, `FUSE_DEV_IOC_CLONE`, is
//! made). [`Serving::stop_reading`] ends the readers with every request they had read passed to
//! the core and answered. The descriptor stays open, in this process and wherever else it is held,
//! so the kernel keeps the connection; a request that arrives meanwhile waits in the kernel for the
//! next reader.

use std::io;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use tsfuse::abi::{InitConfig, InitFlags, Negotiated};
use tsfuse::mount::{Mount, MountOptions, Mounter};
use tsfuse::{DevFd, Fs, Handover, Session};

use super::backing::{self, BackingId, Guard, Saved};
use super::config::FsSettings;
use super::core::Core;
use super::init_record::Outcome;
use super::node_fs::NodeFs;

/// The name the mount shows in `/proc/mounts`, as its source and as its type's subtype
/// (`fuse.tierstore-fs`).
pub(super) const NAME: &str = "tierstore-fs";

/// The options of the mount: read-write (the put directory is below its root), `nosuid`,
/// `nodev`, `default_permissions` (the kernel checks the modes, which the core reports), and
/// `allow_other` where the settings let other users in (jobs run as other users).
pub(super) fn mount_options(allow_other: bool) -> MountOptions {
    MountOptions {
        read_only: false,
        nosuid: true,
        nodev: true,
        default_permissions: true,
        allow_other,
        fsname: NAME.to_owned(),
        subtype: Some(NAME.to_owned()),
    }
}

/// What the mount asks of the kernel at `INIT`: the put mount's terms of `tsfuse` (a `max_write`
/// of 1 MiB for the put directory's writes, `max_background` 1024). The node's mount answers every
/// OPEN and OPENDIR with a handle, so the `NO_OPEN` flags those terms ask for change nothing: the
/// kernel skips an open only after an OPEN answered `ENOSYS`.
pub(super) fn init_config() -> InitConfig {
    InitConfig::put()
}

/// An error of `tsfuse` as the system error the process reports.
fn system(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(error)
}

/// The readers of one connection.
pub(super) struct Serving {
    /// The readers; none once they have stopped.
    readers: Option<tsfuse::Serving>,
    /// The frontend the readers serve, whose parked requests a stop waits for.
    fs: Arc<NodeFs>,
    /// This process's own copy of the descriptor, to store, to hand over and to serve again.
    device: OwnedFd,
    /// The terms the connection was set up with.
    negotiated: Negotiated,
    /// The mount, where this process made it and unmounts it.
    mount: Option<Mount>,
    /// Whether the kernel was asked to send requests again and refused: requests the process
    /// before read stay unanswered.
    resend_failed: bool,
}

/// The number of readers of `settings`.
fn readers(settings: &FsSettings) -> NonZeroUsize {
    NonZeroUsize::new(settings.threads).unwrap_or(NonZeroUsize::MIN)
}

/// The session of `device`, set up with `negotiated`, read by `readers` readers on `dup`s of it,
/// without a handshake and without asking the kernel to send anything again, and leaving every
/// INTERRUPT unanswered.
fn attach(device: &OwnedFd, negotiated: Negotiated, readers: NonZeroUsize) -> io::Result<Session> {
    let mut devices = Vec::with_capacity(readers.get());
    for _ in 0..readers.get() {
        devices.push(device.try_clone()?);
    }
    let handover = Handover::new(negotiated, devices).map_err(system)?;
    let session = Session::attach(handover).map_err(system)?;
    // Every request is answered by the frontend, as on the library before this one: an INTERRUPT
    // never replaces an answer (a commit of the put directory that goes on, a lookup the core
    // counted) with `EINTR`.
    session.ignore_interrupts();
    Ok(session)
}

impl Serving {
    /// Mounts at `fs.mountpoint` and runs the handshake with the kernel; the readers start.
    ///
    /// # Errors
    /// What the system said when the mount, the handshake or the threads fail.
    pub(super) fn mount(core: &Arc<Core>, settings: &FsSettings) -> io::Result<Self> {
        let (device, mount) = Mounter::default()
            .mount(&settings.mountpoint, &mount_options(settings.allow_other))
            .map_err(system)?;
        let served = Self::serve_mount(core, settings, device, &mount);
        if served.is_err() {
            // A mount no process serves is not left behind (its descriptor is closed, so it would
            // answer `ENOTCONN`); a lazy unmount detaches it even where it is in use.
            let _ = mount.unmount(true);
        }
        served
    }

    /// Runs the handshake on `device`, the descriptor of `mount`, and starts the readers.
    fn serve_mount(
        core: &Arc<Core>,
        settings: &FsSettings,
        device: DevFd,
        mount: &Mount,
    ) -> io::Result<Self> {
        let kept = device.try_clone_fd()?;
        let session = Session::init(device, &init_config()).map_err(system)?;
        let negotiated = *session.negotiated();
        // The handshake's descriptor is the session's; the readers read copies of it.
        drop(session);
        // What the kernel offered, what the reply granted, and what was asked and not offered.
        tracing::info!(terms = ?negotiated, "tierstore fs: the handshake's terms");
        let session = attach(&kept, negotiated, readers(settings))?;
        Self::start(
            session,
            (core, readers(settings)),
            (kept, negotiated),
            (Some(mount.clone()), false),
        )
    }

    /// Serves the connection of `device`, which the kernel set up with `outcome`. Where an earlier
    /// release answered the connection's `INIT` with passthrough, the core answers the opens of the
    /// blobs of `saved` with the backing ids that release registered (`backing.rs`); `None` leaves
    /// the core as it is (it already has them, if there are any). With
    /// `resend`, where the connection can, the kernel is asked to send again the requests the
    /// process before read and did not answer: before the first reader starts, since the kernel
    /// puts back every request that is being processed, those the new readers have taken too.
    ///
    /// # Errors
    /// What the system said when the descriptor cannot be copied or the threads started.
    pub(super) fn adopt(
        core: &Arc<Core>,
        settings: &FsSettings,
        device: OwnedFd,
        outcome: Outcome,
        (saved, resend): (Option<&[Saved]>, bool),
    ) -> io::Result<Self> {
        let negotiated = outcome.negotiated();
        let session = attach(&device, negotiated, readers(settings))?;
        if let (Some(saved), true) = (saved, outcome.flags.contains(InitFlags::PASSTHROUGH)) {
            let wrap = |id| -> Guard { Arc::new(BackingId(id)) };
            core.adopt_backing(&settings.runtime_dir.join(backing::FILE), saved, &wrap);
        }
        let resend_failed = resend
            && outcome.flags.contains(InitFlags::HAS_RESEND)
            && match session.resend() {
                Ok(()) => {
                    tracing::info!(
                        "tierstore fs: asked the kernel to send again the requests the earlier process read and did not answer"
                    );
                    false
                }
                Err(why) => {
                    tracing::warn!(
                        "tierstore fs: the kernel was not asked to send requests again: {why}"
                    );
                    true
                }
            };
        Self::start(
            session,
            (core, readers(settings)),
            (device, negotiated),
            (None, resend_failed),
        )
    }

    /// Starts the readers of `session`, which serve `core`.
    fn start(
        session: Session,
        (core, fetchers): (&Arc<Core>, NonZeroUsize),
        (device, negotiated): (OwnedFd, Negotiated),
        (mount, resend_failed): (Option<Mount>, bool),
    ) -> io::Result<Self> {
        let fs = Arc::new(NodeFs::new(Arc::clone(core), fetchers)?);
        let readers = session
            .serve(Arc::clone(&fs) as Arc<dyn Fs>)
            .map_err(system)?;
        Ok(Self {
            readers: Some(readers),
            fs,
            device,
            negotiated,
            mount,
            resend_failed,
        })
    }

    /// The outcome of the handshake this connection was set up with, as the record keeps it.
    pub(super) fn outcome(&self) -> Outcome {
        Outcome::of(&self.negotiated)
    }

    /// A copy of the descriptor of the connection.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn device(&self) -> io::Result<OwnedFd> {
        self.device.try_clone()
    }

    /// Whether the kernel was asked to send requests again and did not: what the process before
    /// read and did not answer stays so.
    pub(super) fn resend_failed(&self) -> bool {
        self.resend_failed
    }

    /// Stops the readers and returns when each has answered the request it held and stopped, and
    /// every request they parked on the fetches is answered (by `fs.fetch_timeout` after it was
    /// parked, for an open). The
    /// connection stays open. Calling it again, with no restart between, does nothing. The stop is
    /// a hand-over of `tsfuse`: a request the file system had not answered when its reader stopped
    /// (a commit of the put directory still running) is answered when it ends, and is never
    /// answered `EIO` for being left behind.
    pub(super) fn stop_reading(&mut self) {
        let Some(readers) = self.readers.take() else {
            return;
        };
        // A reader that failed has stopped too; the error is said.
        let ended = readers.hand_over();
        let _ = ended.inspect_err(|why| tracing::warn!("tierstore fs: a reader failed: {why}"));
        // The requests the readers parked are answered too (`fetches.rs`), each by its time.
        self.fs.drain();
    }

    /// Serves the connection again after [`Serving::stop_reading`], from the same descriptor and
    /// outcome: the hand-over to a successor that did not complete. Nothing is asked of the kernel
    /// again: every request the readers read was answered.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn restart(&mut self, core: &Arc<Core>, settings: &FsSettings) -> io::Result<()> {
        self.stop_reading();
        let device = self.device.try_clone()?;
        let session = attach(&device, self.negotiated, readers(settings))?;
        // The new readers have no handle on the mount, if there was one: a later `unmount` leaves
        // the mount, as the successor that was to take it over would have.
        *self = Self::start(
            session,
            (core, readers(settings)),
            (device, self.negotiated),
            (None, false),
        )?;
        Ok(())
    }

    /// Ends the readers, and unmounts what this process mounted. The readers of a mount are not
    /// stopped first: they answer what the kernel sends until the unmount ends the connection's
    /// reads, and are then joined.
    ///
    /// # Errors
    /// What the system said when the mount could not be unmounted.
    pub(super) fn unmount(mut self) -> io::Result<()> {
        let Some(mount) = self.mount.take() else {
            self.stop_reading();
            return Ok(());
        };
        let Some(readers) = self.readers.take() else {
            return Ok(());
        };
        mount.unmount(false).map_err(system)?;
        readers.join().map_err(system)?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "serving_tests.rs"]
mod tests;
