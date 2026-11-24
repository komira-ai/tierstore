//! The FUSE side of the process of `tierstore fs`: what a start checks and makes around the neutral
//! start (`start.rs`). Linux only: the mount is a FUSE mount.
//!
//! A start, in order: a mount of this file system that the kernel's table of mounts shows at the
//! mount point, which no process serves at a fresh start, is aborted and detached, with no look
//! at the mount point; the cache directory must be on a file system that is not stacked (the job's
//! overlay above this mount must stay within the kernel's depth of stacking); the neutral start is
//! done (`Setup`: a table of tree numbers afresh, the attachments made again, the thread started
//! that tells the cache which trees are attached once its fill socket answers, the control socket
//! bound); the mount is made and the record of its `INIT` handshake written. Each step's time is
//! said as it ends (`steps.rs`). That is [`Running::start`]. [`Running::adopt`] is the
//! start of a process that takes a connection over (after a crash or a restart, with the
//! descriptor the service manager kept, or in a hand-over from the running instance): the same
//! state is read from the runtime directory instead of made, and nothing is mounted. What
//! decides between them, tells the service manager and waits for the signal that ends the process
//! is in `launch`.
//!
//! Where `fs.put` is enabled, both open the put directory's stage (replaying its journal) before
//! the frontend serves; a process that stops reading for a hand-over or a restart writes what the
//! kernel holds of the put directory for its successor (`put/carry.rs`), which takes it over.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustix::mount::UnmountFlags;

use super::backing::{self, Saved};
use super::config::FsSettings;
use super::control::Control;
use super::core::Core;
use super::fills::Reach;
use super::init_record::{self, Record};
use super::locked;
use super::put::carry;
use super::put::space::{PutSpace, SWEEP_EVERY};
use super::put::{StagedStore, Uploader};
use super::recover;
use super::serving::Serving;
use super::start::{
    Identity, Setup, StartError, control_error, core_of, create_mount_point, create_runtime_dir, io,
};
use super::steps::Steps;
use super::table::TreeTable;
use crate::server::local_cache::SystemDisk;
use crate::{MonotonicClock, SystemClock};

/// Serves the put directory of `settings` from `core`, where `fs.put` is enabled, its puts asked by
/// `reach`: the stage opened
/// and its journal replayed, the hand-over of the process before taken over (`handed`, none for a
/// fresh mount or a start after a crash), and the sweep started. `generation` numbers the handles.
///
/// # Errors
/// [`StartError::Put`] for a stage that cannot be used, [`StartError::Io`] for the sweep's thread.
fn serve_put(
    (settings, reach): (&FsSettings, &Reach),
    core: &Core,
    generation: u32,
    handed: Option<Option<carry::Handed>>,
) -> Result<(), StartError> {
    let Some(put) = &settings.put else {
        return Ok(());
    };
    let (store, replayed) = StagedStore::open(
        put.store.clone(),
        &put.stage_dir,
        &settings.cache_path,
        Arc::new(SystemClock),
        Arc::new(SystemDisk),
    )?;
    tracing::info!(?replayed, "tierstore fs: the put directory's stage is open");
    let clock = Arc::new(MonotonicClock::new());
    let uploader = Uploader::new(
        reach.puts(settings, put.commit_deadline),
        put.commit_deadline,
        clock.clone(),
        Arc::new(std::thread::sleep),
    );
    let space = PutSpace::new(
        store,
        uploader,
        (settings.owner, generation),
        put.commit_deadline,
        clock,
    );
    if let Some(handed) = handed {
        space.take_over(handed);
    }
    let space = Arc::new(space);
    space
        .start_sweep(SWEEP_EVERY)
        .map_err(io("starting the thread that sweeps the put directory"))?;
    core.serve_put(space);
    Ok(())
}

/// What the kernel holds of the put directory of `core`, written for the successor; a failure is
/// said, and the successor then finds what the store holds. For a hand-over (`handing`) the
/// commits still running are waited for first: each answers a request this process read, which
/// the kernel does not send again to the successor, and writes the journal and the stage the
/// successor reads (`put/space_rename.rs`). A restart does not wait: the unit's stop timeout is
/// short, and the next process is asked for the requests again where the kernel can send them.
fn hand_put(core: &Core, runtime_dir: &Path, handing: bool) {
    if let Some(put) = core.put() {
        put.hold(true);
        if handing {
            put.drain_commits();
        }
        carry::write_or_say(runtime_dir, &put.handed());
    }
}

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
/// handshake (with the generation this process will have), the backing ids an earlier release
/// registered on the connection (`backing.rs`), and the table of tree numbers. Nothing is changed by reading it, so a failure
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

/// What every start checks and makes: the cache directory is there and not on a stacked file
/// system, and the runtime directory is made. It touches nothing at the mount point.
fn make_state_directories(settings: &FsSettings) -> Result<(), StartError> {
    check_cache(&settings.cache_path)?;
    create_runtime_dir(settings)
}

/// Makes the directories a fresh start needs: those of [`make_state_directories`] and the mount
/// point. Only a start that makes the mount calls this: the mount point of a connection that is
/// taken over is a live mount whose root has no cache time, so a `stat` of it is a request to the
/// process that is not serving yet.
fn make_directories(settings: &FsSettings) -> Result<(), StartError> {
    make_state_directories(settings)?;
    create_mount_point(settings)
}

/// What a fresh start does first: `discard`, which detaches a dead mount at the mount point, and
/// then the directories. A dead mount answers `mkdir` with `EEXIST` and `stat` with `ENOTCONN`, so
/// `create_dir_all` fails on it: it must be gone before the directories are made.
fn begin(settings: &FsSettings, discard: impl FnOnce()) -> Result<(), StartError> {
    discard();
    make_directories(settings)
}

impl Running {
    /// Does what a start does, up to and including the mount, with a core that asks the node's
    /// cache and tree store by `reach`: a mount of this file system that `kernel` shows at the
    /// mount point is aborted and detached (no process serves it: nothing answered the hand-over
    /// and the store held nothing), the state of an earlier mount is replaced, the control socket
    /// is bound and the mount is made.
    ///
    /// # Errors
    /// [`StartError`]: why nothing was mounted.
    pub(super) fn start(
        (settings, reach): (&FsSettings, &Reach),
        kernel: &Kernel,
    ) -> Result<Self, StartError> {
        let mut steps = Steps::new(Arc::new(MonotonicClock::new()));
        begin(settings, || discard_unserved(settings, kernel))?;
        let _ = steps.done("making the directories");
        let Setup {
            core,
            mount_id,
            _control: control,
            ..
        } = Setup::start((settings, reach), true, Identity::Fresh, None)?;
        let _ = steps.done("setting up the core and the control socket");
        // A hand-over left by an earlier mount names nothing of this one.
        carry::remove(&settings.runtime_dir)
            .map_err(io("removing the put directory's hand-over"))?;
        serve_put((settings, reach), &core, 1, None)?;
        let _ = steps.done("opening the put directory");
        let serving = Serving::mount(&core, settings)
            .map_err(io(format!("mounting {}", settings.mountpoint.display())))?;
        let record = Record::first(serving.outcome(), mount_id);
        init_record::write(&settings.runtime_dir, &record)?;
        let _ = steps.done("mounting");
        Ok(Self::of(core, settings, Parts { control, serving }))
    }

    /// Serves the connection of `device`, which an earlier process set up and `prepared` describes,
    /// with a core that asks the node's cache and tree store by `reach`:
    /// no mount is made and no handshake is run, and nothing is asked of the mount point (it is the
    /// live mount, and the readers that would answer are not started yet). `control` is the control socket the earlier
    /// process gave over, or none to bind it afresh. `resend` asks the kernel for the requests the
    /// earlier process read and did not answer, where the connection can.
    ///
    /// # Errors
    /// [`StartError`]: why the connection is not served.
    pub(super) fn adopt(
        (settings, reach): (&FsSettings, &Reach),
        prepared: Prepared,
        device: OwnedFd,
        (control, resend): (Option<std::os::unix::net::UnixListener>, bool),
    ) -> Result<Self, StartError> {
        Self::adopt_as((settings, reach), prepared, device, (control, resend), true)
    }

    /// [`Running::adopt`] of the connection of `device` taken in a hand-over, with the control
    /// socket `control` the running instance gave over: the same state is read and made, but
    /// nothing reads the device and the control socket accepts nothing before
    /// [`Running::start_reading`], which the successor calls once the running instance has said
    /// `bye` (until then it may resume reading).
    ///
    /// # Errors
    /// As [`Running::adopt`].
    pub(super) fn take_over(
        (settings, reach): (&FsSettings, &Reach),
        prepared: Prepared,
        device: OwnedFd,
        control: std::os::unix::net::UnixListener,
    ) -> Result<Self, StartError> {
        Self::adopt_as(
            (settings, reach),
            prepared,
            device,
            (Some(control), false),
            false,
        )
    }

    /// [`Running::adopt`], with the readers and the control socket started when `reading`.
    fn adopt_as(
        (settings, reach): (&FsSettings, &Reach),
        prepared: Prepared,
        device: OwnedFd,
        (control, resend): (Option<std::os::unix::net::UnixListener>, bool),
        reading: bool,
    ) -> Result<Self, StartError> {
        let mut steps = Steps::new(Arc::new(MonotonicClock::new()));
        make_state_directories(settings)?;
        let _ = steps.done("making the directories");
        let Prepared {
            record,
            saved,
            table,
        } = prepared;
        let core = core_of(
            (settings, reach),
            table,
            record.mount_id.clone(),
            record.generation,
            true,
        )?;
        let _ = steps.done("setting up the core");
        let handed = carry::read(&settings.runtime_dir)
            .map_err(io("reading the put directory's hand-over"))?;
        serve_put((settings, reach), &core, record.generation, Some(handed))?;
        // Taken over: a later start must not take it over again.
        carry::remove_or_say(&settings.runtime_dir);
        let _ = steps.done("opening the put directory");
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
        let mut control = control;
        let serving = if reading {
            Serving::adopt(
                &core,
                settings,
                device,
                record.outcome,
                (Some(&saved), resend),
            )
        } else {
            control.pause();
            Serving::paused(&core, settings, device, record.outcome, &saved)
        }
        .map_err(io("serving the connection"))?;
        let _ = steps.done("serving the connection");
        Ok(Self::of(core, settings, Parts { control, serving }))
    }

    fn of(core: Arc<Core>, settings: &FsSettings, parts: Parts) -> Self {
        Self {
            core,
            settings: settings.clone(),
            parts: Mutex::new(parts),
        }
    }

    /// Starts the readers and the control socket of a connection [taken over](Running::take_over):
    /// from here this process reads the device.
    ///
    /// # Errors
    /// What the system said when the readers or the control socket cannot start.
    pub(super) fn start_reading(&self) -> std::io::Result<()> {
        let mut parts = locked(&self.parts);
        parts.serving.restart(&self.core, &self.settings)?;
        parts.control.resume()
    }

    /// Whether readers of the device are running.
    #[cfg(test)]
    pub(super) fn reading(&self) -> bool {
        locked(&self.parts).serving.reading()
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
    pub(super) fn outcome(&self) -> super::init_record::Outcome {
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
    /// each holds and stop, and the commits of the put directory still running answer theirs (a
    /// commit waiting for a release ends at once; one that uploads, by its deadline). Blocks until
    /// then.
    pub(super) fn quiesce(&self) {
        let mut parts = locked(&self.parts);
        parts.control.pause();
        parts.serving.stop_reading();
        // From here the successor numbers trees and owns the pins of this mount: no idle tree is
        // detached (an unpin of a tree the successor pinned again) until a resume.
        self.core.hold(true);
        hand_put(&self.core, &self.settings.runtime_dir, true);
    }

    /// Answers again after [`Running::quiesce`]: a hand-over that did not complete.
    ///
    /// # Errors
    /// What the system said when the readers or the control socket cannot start again.
    pub(super) fn resume(&self) -> std::io::Result<()> {
        let mut parts = locked(&self.parts);
        if let Some(put) = self.core.put() {
            // A successor that did not complete its start may have rewritten the journal: this
            // process appends to a file of its own again before it serves, or does not serve.
            put.store().reopen_journal()?;
            put.hold(false);
            carry::remove_or_say(&self.settings.runtime_dir);
        }
        self.core.hold(false);
        parts.serving.restart(&self.core, &self.settings)?;
        parts.control.resume()
    }

    /// The mount is the successor's: the control socket's file is its too, and is not removed.
    pub(super) fn give_up(&self) {
        locked(&self.parts).control.leave_path();
    }

    /// Ends the process's service of the mount and leaves the mount for the service manager's
    /// descriptor store to keep: the readers answer what they hold and stop, and nothing is
    /// unmounted.
    pub(super) fn leave(self) {
        locked(&self.parts).serving.stop_reading();
        hand_put(&self.core, &self.settings.runtime_dir, false);
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
    /// The check that a descriptor offered as the mount's is the FUSE device
    /// (`handover::check_device`; a test's stand-in for the device is a socket).
    pub device: fn(std::os::fd::BorrowedFd<'_>) -> std::io::Result<()>,
}

impl Kernel {
    /// The files of the machine.
    pub(super) fn real() -> Self {
        Self {
            mountinfo: PathBuf::from(MOUNTINFO),
            connections: PathBuf::from(recover::SYS_CONNECTIONS),
            device: super::handover::check_device,
        }
    }

    /// The number of the connection of the FUSE mount at `mountpoint`, if there is one.
    pub(super) fn connection_of(&self, mountpoint: &Path) -> Option<u64> {
        let info = std::fs::read_to_string(&self.mountinfo).unwrap_or_default();
        recover::connection_of(&info, mountpoint)
    }

    /// The number of the connection of the mount of this file system (type `fuse.tierstore-fs`)
    /// at `mountpoint`, if there is one.
    pub(super) fn tierstore_connection_of(&self, mountpoint: &Path) -> Option<u64> {
        let info = std::fs::read_to_string(&self.mountinfo).unwrap_or_default();
        let fuse_type = format!("fuse.{}", super::serving::NAME);
        let ours: String = info
            .lines()
            .filter(|line| {
                let fstype = line
                    .split_once(" - ")
                    .map(|(_, after)| after.split(' ').next());
                fstype == Some(Some(fuse_type.as_str()))
            })
            .map(|line| format!("{line}\n"))
            .collect();
        recover::connection_of(&ours, mountpoint)
    }
}

/// Aborts and detaches a mount of this file system (type `fuse.tierstore-fs`) that `kernel` shows
/// at `fs.mountpoint`, as a fresh start finds one: no process serves it, since nothing answered
/// the hand-over and the service manager's store held nothing. Every request waiting on it then
/// fails, and the mount point is free for a new mount. The mount is found in the kernel's table of
/// mounts and the mount point is not looked at: a `stat` of a mount whose descriptor a process
/// holds and does not read waits for ever (`docs/design/one-service.md`, decision 2). A mount of
/// another file system there is left.
fn discard_unserved(settings: &FsSettings, kernel: &Kernel) {
    let detach = |path: &Path| {
        rustix::mount::unmount(path, UnmountFlags::DETACH).map_err(std::io::Error::from)
    };
    discard_with(&settings.mountpoint, kernel, &detach);
}

/// [`discard_unserved`] with the call that detaches a mount given. Neither a connection that
/// cannot be aborted nor a mount that cannot be detached stops the start: each is said.
fn discard_with(mountpoint: &Path, kernel: &Kernel, detach: &dyn Fn(&Path) -> std::io::Result<()>) {
    let Some(number) = kernel.tierstore_connection_of(mountpoint) else {
        return;
    };
    if let Err(why) = recover::Connection::new(&kernel.connections, number).abort() {
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
