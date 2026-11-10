//! The FUSE side of the process of `tierstore fs`: what a start checks and makes around the neutral
//! start (`start.rs`). Linux only: the mount is a FUSE mount.
//!
//! A start, in order: the cache directory must be on a file system that is not stacked (the job's
//! overlay above this mount must stay within the kernel's depth of stacking); the neutral start is
//! done (`Setup`: a table of tree numbers afresh, the attachments made again, the cache told
//! which trees are attached, the control socket bound); the mount is made and the record of its
//! `INIT` handshake written. That is [`Running::start`]. [`Running::adopt`] is the
//! start of a process that takes a connection over (after a crash or a restart, with the
//! descriptor the service manager kept, or in a hand-over from the running instance): the same
//! state is read from the runtime directory instead of made, and nothing is mounted. What
//! decides between them, tells the service manager and waits for the signal that ends the process
//! is in `launch`.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustix::mount::UnmountFlags;

use super::backing::{self, Saved};
use super::config::FsSettings;
use super::control::Control;
use super::core::Core;
use super::init_record::{self, Record};
use super::locked;
use super::recover;
use super::serving::Serving;
use super::start::{Setup, StartError, control_error, core_of, create_directories, io};
use super::table::TreeTable;

/// The mounts of this process's namespace.
const MOUNTINFO: &str = "/proc/self/mountinfo";

/// The magic number of an overlay file system (`OVERLAYFS_SUPER_MAGIC`).
const OVERLAYFS: i64 = 0x794c_7630;

/// The magic number of an eCryptfs file system (`ECRYPTFS_SUPER_MAGIC`).
const ECRYPTFS: i64 = 0xf15f;

/// The name of the stacked file system with the magic number `magic`, if it is one.
fn stacked_name(magic: i64) -> Option<&'static str> {
    match magic {
        OVERLAYFS => Some("overlayfs"),
        ECRYPTFS => Some("eCryptfs"),
        _ => None,
    }
}

/// Refuses the cache directory `path` if its file system has the magic number `magic` of a stacked
/// one.
fn refuse_stacked(path: &Path, magic: i64) -> Result<(), StartError> {
    match stacked_name(magic) {
        Some(name) => Err(StartError::Stacked {
            path: path.display().to_string(),
            name,
        }),
        None => Ok(()),
    }
}

/// Refuses a cache directory on a stacked file system.
fn check_cache(path: &Path) -> Result<(), StartError> {
    let stat = rustix::fs::statfs(path)
        .map_err(|errno| std::io::Error::from_raw_os_error(errno.raw_os_error()))
        .map_err(io(format!("fs.cache_path {}", path.display())))?;
    refuse_stacked(path, stat.f_type as i64)
}

/// What a process that takes over a connection reads before it takes it: the record of the `INIT`
/// handshake (with the generation this process will have), the backing ids the process before
/// registered, and the table of tree numbers. Nothing is changed by reading it, so a failure
/// leaves the process before in charge.
pub(super) struct Prepared {
    record: Record,
    saved: Vec<Saved>,
    table: TreeTable,
}

impl Prepared {
    /// Reads the state in `settings.runtime_dir`.
    ///
    /// # Errors
    /// [`StartError`]: the record or the table is missing, is of another layout or cannot be read.
    pub(super) fn load(settings: &FsSettings) -> Result<Self, StartError> {
        let record = init_record::read(&settings.runtime_dir)?.next();
        let table = TreeTable::open(&settings.runtime_dir)?;
        let saved = backing::saved(&settings.runtime_dir.join(backing::FILE))
            .map_err(io("reading the backing table"))?;
        Ok(Self {
            record,
            saved,
            table,
        })
    }
}

/// The parts of a started process that a hand-over stops and starts again.
struct Parts {
    control: Control,
    serving: Serving,
}

/// A started `tierstore fs`: the core, the control socket and the readers of the connection.
pub(super) struct Running {
    core: Arc<Core>,
    settings: FsSettings,
    parts: Mutex<Parts>,
}

/// Makes the directories a start needs.
fn make_directories(settings: &FsSettings) -> Result<(), StartError> {
    check_cache(&settings.cache_path)?;
    create_directories(settings)
}

/// What a fresh start does first: `discard`, which detaches a dead mount at the mount point, and
/// then the directories. A dead mount answers `mkdir` with `EEXIST` and `stat` with `ENOTCONN`, so
/// `create_dir_all` fails on it: it must be gone before the directories are made.
fn begin(settings: &FsSettings, discard: impl FnOnce()) -> Result<(), StartError> {
    discard();
    make_directories(settings)
}

impl Running {
    /// Does what a start does, up to and including the mount: the state of an earlier mount is
    /// replaced, the control socket is bound and the mount is made.
    ///
    /// # Errors
    /// [`StartError`]: why nothing was mounted.
    pub(super) fn start(settings: &FsSettings) -> Result<Self, StartError> {
        begin(settings, || {
            discard_dead_mount(
                settings,
                std::fs::metadata(&settings.mountpoint),
                &Kernel::real(),
            );
        })?;
        let Setup {
            core,
            mount_id,
            _control: control,
        } = Setup::start(settings, true)?;
        let serving = Serving::mount(&core, settings)
            .map_err(io(format!("mounting {}", settings.mountpoint.display())))?;
        let record = Record::first(serving.outcome(), mount_id);
        init_record::write(&settings.runtime_dir, &record)?;
        Ok(Self::of(core, settings, Parts { control, serving }))
    }

    /// Serves the connection of `device`, which an earlier process set up and `prepared` describes:
    /// no mount is made and no handshake is run. `control` is the control socket the earlier
    /// process gave over, or none to bind it afresh. `resend` asks the kernel for the requests the
    /// earlier process read and did not answer, where the connection can.
    ///
    /// # Errors
    /// [`StartError`]: why the connection is not served.
    pub(super) fn adopt(
        settings: &FsSettings,
        prepared: Prepared,
        device: OwnedFd,
        (control, resend): (Option<std::os::unix::net::UnixListener>, bool),
    ) -> Result<Self, StartError> {
        make_directories(settings)?;
        let Prepared {
            record,
            saved,
            table,
        } = prepared;
        let core = core_of(
            settings,
            table,
            (record.mount_id.clone(), record.generation),
            true,
        )?;
        let control = match control {
            Some(listener) => {
                Control::serve_on(Arc::clone(&core), listener, &settings.control_socket)
            }
            None => Control::serve(
                Arc::clone(&core),
                &settings.control_socket,
                settings.client_group,
            ),
        }
        .map_err(control_error(settings))?;
        init_record::write(&settings.runtime_dir, &record)?;
        let serving = Serving::adopt(
            &core,
            settings,
            device,
            record.outcome,
            (Some(&saved), resend),
        )
        .map_err(io("serving the connection"))?;
        Ok(Self::of(core, settings, Parts { control, serving }))
    }

    fn of(core: Arc<Core>, settings: &FsSettings, parts: Parts) -> Self {
        Self {
            core,
            settings: settings.clone(),
            parts: Mutex::new(parts),
        }
    }

    /// The core the mount is served from.
    #[cfg(test)]
    pub(super) fn core(&self) -> &Arc<Core> {
        &self.core
    }

    /// A copy of the `/dev/fuse` descriptor.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn device(&self) -> std::io::Result<OwnedFd> {
        locked(&self.parts).serving.device()
    }

    /// Whether the kernel was asked to send the earlier process's requests again and refused.
    pub(super) fn resend_failed(&self) -> bool {
        locked(&self.parts).serving.resend_failed()
    }

    /// The outcome of the handshake the connection was set up with.
    pub(super) fn outcome(&self) -> fuser::InitOutcome {
        locked(&self.parts).serving.outcome()
    }

    /// Copies of the descriptor of the connection and of the control socket's listening socket,
    /// for the offer of a hand-over.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn share(&self) -> std::io::Result<(OwnedFd, OwnedFd)> {
        let parts = locked(&self.parts);
        Ok((parts.serving.device()?, parts.control.listener_fd()?))
    }

    /// Stops answering for a hand-over: the control socket stops, the readers answer the request
    /// each holds and stop, and the backing table is written with its counts exact. Blocks until
    /// then.
    pub(super) fn quiesce(&self) {
        let mut parts = locked(&self.parts);
        parts.control.pause();
        parts.serving.stop_reading();
        self.core.write_backing_exact();
    }

    /// Answers again after [`Running::quiesce`]: a hand-over that did not complete.
    ///
    /// # Errors
    /// What the system said when the readers or the control socket cannot start again.
    pub(super) fn resume(&self) -> std::io::Result<()> {
        let mut parts = locked(&self.parts);
        parts.serving.restart(&self.core, &self.settings)?;
        // The counts written exact at the pause are a lower bound again once opens go on.
        self.core.write_backing_inexact();
        parts.control.resume()
    }

    /// The mount is the successor's: the control socket's file is its too, and is not removed, and
    /// the backing ids are its too: they stay open in the kernel when this process ends.
    pub(super) fn give_up(&self) {
        locked(&self.parts).control.leave_path();
        self.core.abandon_backing();
    }

    /// Ends the process's service of the mount and leaves the mount for the service manager's
    /// descriptor store to keep: the readers answer what they hold and stop, and nothing is
    /// unmounted.
    pub(super) fn leave(self) {
        locked(&self.parts).serving.stop_reading();
        // The successor of a restart adopts the ids, and trusts the counts of a line that says
        // they are exact; the ids must stay open in the kernel when this process's table goes.
        self.core.write_backing_exact();
        self.core.abandon_backing();
    }

    /// Unmounts and waits for the threads of the mount to end. A mount this process did not make
    /// is left.
    ///
    /// # Errors
    /// What the system said when the mount could not be unmounted.
    pub(super) fn stop(self) -> Result<(), StartError> {
        let Self { parts, .. } = self;
        let Parts { serving, control } = parts
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(control);
        serving.unmount().map_err(io("unmounting"))
    }
}

/// Where the kernel shows the mounts and the connections of FUSE mounts.
#[derive(Clone, Debug)]
pub(super) struct Kernel {
    /// The table of mounts, `/proc/self/mountinfo`.
    pub mountinfo: PathBuf,
    /// The directory of connections, `/sys/fs/fuse/connections`.
    pub connections: PathBuf,
}

impl Kernel {
    /// The files of the machine.
    pub(super) fn real() -> Self {
        Self {
            mountinfo: PathBuf::from(MOUNTINFO),
            connections: PathBuf::from(recover::SYS_CONNECTIONS),
        }
    }

    /// The number of the connection of the FUSE mount at `mountpoint`, if there is one.
    pub(super) fn connection_of(&self, mountpoint: &Path) -> Option<u64> {
        let info = std::fs::read_to_string(&self.mountinfo).unwrap_or_default();
        recover::connection_of(&info, mountpoint)
    }
}

/// Aborts and detaches the FUSE mount at `fs.mountpoint` if `probe`, the answer of a look at the
/// mount point, says its connection is dead (`ENOTCONN`): what a process that was killed with no
/// one holding the descriptor leaves. A mount that answers is not touched.
fn discard_dead_mount(
    settings: &FsSettings,
    probe: std::io::Result<std::fs::Metadata>,
    kernel: &Kernel,
) {
    if probe.is_err_and(|why| not_connected(&why)) {
        discard_mount(settings, false, kernel);
    }
}

/// Whether `error` is the kernel's `ENOTCONN`: the answer of a mount whose connection has ended.
fn not_connected(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::NOTCONN.raw_os_error())
}

/// Detaches the FUSE mount at `fs.mountpoint`, after aborting its connection if `abort`: every
/// request waiting on it then fails, and the mount point is free for a new mount.
pub(super) fn discard_mount(settings: &FsSettings, abort: bool, kernel: &Kernel) {
    let detach = |path: &Path| {
        rustix::mount::unmount(path, UnmountFlags::DETACH).map_err(std::io::Error::from)
    };
    discard_with(&settings.mountpoint, (kernel, abort), &detach);
}

/// [`discard_mount`] with the call that detaches a mount given.
fn discard_with(
    mountpoint: &Path,
    (kernel, abort): (&Kernel, bool),
    detach: &dyn Fn(&Path) -> std::io::Result<()>,
) {
    let Some(number) = kernel.connection_of(mountpoint) else {
        return;
    };
    if abort && let Err(why) = recover::Connection::new(&kernel.connections, number).abort() {
        tracing::warn!("tierstore fs: cannot abort the connection {number}: {why}");
    }
    let shown = mountpoint.display();
    if let Err(why) = detach(mountpoint) {
        tracing::warn!("tierstore fs: cannot detach {shown}: {why}");
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
