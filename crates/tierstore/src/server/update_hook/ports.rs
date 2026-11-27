//! What the steps do to the machine, as ports: the two systemd instances of each of the two
//! templates and their start markers, the process of an fs instance and the binary it runs, the
//! launchd job, the instances' HTTP answers and the control socket of the one that serves, the
//! node's mount provider and its mount, what the host says of itself, the clock and the state
//! directory.
//!
//! Each port is a small trait; the steps take a [`SystemdPorts`] or a [`LaunchdPorts`] and never
//! reach for the machine themselves. Tests keep the machine's world in a fake. Nothing here reads
//! an environment variable. [`super::machine`] has the machine's implementations.
//!
//! A port that changes something is idempotent: starting a running instance, stopping a stopped
//! one, removing an absent marker and booting out a job that is not loaded are not errors.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Which port failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortKind {
    /// The systemd units.
    Units,
    /// The start markers of the instances.
    Markers,
    /// The launchd job.
    Launchd,
    /// The instances' HTTP answers.
    Instances,
    /// What the host says of itself.
    Host,
    /// The state directory.
    State,
}

impl fmt::Display for PortKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PortKind::Units => "units",
            PortKind::Markers => "markers",
            PortKind::Launchd => "launchd",
            PortKind::Instances => "instances",
            PortKind::Host => "host",
            PortKind::State => "state",
        })
    }
}

/// Why a port could not do what was asked of it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    /// Another step holds the lock of the state directory.
    #[error("another step holds the lock of the state directory")]
    Busy,
    /// The call failed.
    #[error("{kind}: {message}")]
    Failed {
        /// Which port.
        kind: PortKind,
        /// What it said.
        message: String,
    },
}

impl PortError {
    /// A failure of `kind`.
    #[must_use]
    pub fn failed(kind: PortKind, message: impl Into<String>) -> PortError {
        PortError::Failed {
            kind,
            message: message.into(),
        }
    }
}

/// One of the two systemd templates the hook drives. Each has the instances `a` and `b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// `tierstore@.service`, the S3 process.
    Store,
    /// `tierstore-fs@.service`, `tierstore fs`.
    Fs,
}

impl Family {
    /// The unit of `instance`: `tierstore@a.service` or `tierstore-fs@a.service`.
    #[must_use]
    pub fn unit(self, instance: Instance) -> String {
        match self {
            Family::Store => format!("tierstore@{}.service", instance.letter()),
            Family::Fs => format!("tierstore-fs@{}.service", instance.letter()),
        }
    }

    /// The file name of `instance`'s start marker: the name the template's
    /// `ConditionPathExists=` gives with `%i`, `run-a` or `run-fs-a`.
    #[must_use]
    pub fn marker_file(self, instance: Instance) -> String {
        match self {
            Family::Store => format!("run-{}", instance.letter()),
            Family::Fs => format!("run-fs-{}", instance.letter()),
        }
    }
}

/// One of the two instances of a systemd template, `tierstore@.service` or
/// `tierstore-fs@.service`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Instance {
    /// `tierstore@a.service`.
    A,
    /// `tierstore@b.service`.
    B,
}

impl Instance {
    /// Both instances.
    pub const ALL: [Instance; 2] = [Instance::A, Instance::B];

    /// The instance name, `%i` of the template: `a` or `b`.
    #[must_use]
    pub const fn letter(self) -> &'static str {
        match self {
            Instance::A => "a",
            Instance::B => "b",
        }
    }

    /// The other instance.
    #[must_use]
    pub const fn other(self) -> Instance {
        match self {
            Instance::A => Instance::B,
            Instance::B => Instance::A,
        }
    }
}

impl fmt::Display for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.letter())
    }
}

/// What systemd says of a unit (`ActiveState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitState {
    /// `active` (or `reloading`): its process runs.
    Active,
    /// `activating`: it is starting, or waiting to be started again (`Restart=`).
    Activating,
    /// `deactivating`: it is stopping.
    Deactivating,
    /// `inactive`: no process; a start whose condition failed leaves it so.
    Inactive,
    /// `failed`: no process; it ended badly.
    Failed,
}

impl UnitState {
    /// Whether a process of the unit may run: active, activating or deactivating.
    #[must_use]
    pub const fn running(self) -> bool {
        matches!(
            self,
            UnitState::Active | UnitState::Activating | UnitState::Deactivating
        )
    }
}

/// The systemd units of the two instances of one template.
pub trait Units {
    /// Starts the instance's unit and returns when systemd has started it. A start whose
    /// `ConditionPathExists=` fails (no marker) is not an error: the unit stays inactive.
    ///
    /// # Errors
    /// The program failed.
    fn start(&self, instance: Instance) -> Result<(), PortError>;

    /// Stops the instance's unit and returns when its process has exited (the drain included).
    ///
    /// # Errors
    /// The program failed.
    fn stop(&self, instance: Instance) -> Result<(), PortError>;

    /// What systemd says of the instance's unit.
    ///
    /// # Errors
    /// The program failed, or said a state that is none of [`UnitState`].
    fn state(&self, instance: Instance) -> Result<UnitState, PortError>;
}

/// The start markers: a file per instance in the service's state directory, whose presence the
/// template's `ConditionPathExists=` requires. An instance without its marker does not start,
/// at boot or on any start.
pub trait Markers {
    /// Whether the instance's marker is there.
    ///
    /// # Errors
    /// It cannot be looked at, or the path is not a regular file.
    fn present(&self, instance: Instance) -> Result<bool, PortError>;

    /// Lays the instance's marker down (and the directory, when it is absent); a marker already
    /// there is fine.
    ///
    /// # Errors
    /// It could not be made.
    fn set(&self, instance: Instance) -> Result<(), PortError>;

    /// Removes the instance's marker; an absent one is fine.
    ///
    /// # Errors
    /// It could not be removed.
    fn clear(&self, instance: Instance) -> Result<(), PortError>;
}

/// A file as the kernel tells it from every other: its device and inode. Two paths name the same
/// file when these are equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    /// The device.
    pub dev: u64,
    /// The inode on it.
    pub ino: u64,
}

/// The binary a process runs, as `/proc/<pid>/exe` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exe {
    /// The path the link names: the path the file had when the process executed it, with
    /// ` (deleted)` after it once that name was removed or replaced (as the hook's copy is
    /// replaced by a rename).
    pub path: PathBuf,
    /// The file the process runs, which a replaced name no longer names.
    pub file: FileId,
}

/// The processes of the instances of `tierstore-fs@.service` (Linux): which process systemd
/// has for an instance, and the binary that process runs.
pub trait Processes {
    /// The main process of the instance's unit (`MainPID`); `None` when systemd names none.
    ///
    /// # Errors
    /// The program failed, or said something that is not a pid.
    fn main_pid(&self, instance: Instance) -> Result<Option<u32>, PortError>;

    /// The binary the process `pid` runs (`/proc/<pid>/exe`).
    ///
    /// # Errors
    /// The link cannot be read or followed: the process is gone, or the hook may not look at it.
    fn exe(&self, pid: u32) -> Result<Exe, PortError>;

    /// The file at [`SERVICE_BINARY`](super::SERVICE_BINARY): the copy the hook installed, which
    /// the fs unit runs.
    ///
    /// # Errors
    /// There is no file there, or it cannot be looked at.
    fn installed(&self) -> Result<FileId, PortError>;

    /// The version of the binary the process `pid` runs: what `--version` of its
    /// `/proc/<pid>/exe` prints after the program's name, such as `0.5.0+0123456789ab`. The fs
    /// process serves no port, so this is the one way to ask it; it works for a binary whose
    /// name was replaced too.
    ///
    /// # Errors
    /// The binary cannot be run, ends any way but 0, or prints no version.
    fn version(&self, pid: u32) -> Result<String, PortError>;
}

/// The launchd system domain, for the one job of the service.
pub trait Launchd {
    /// Whether the system domain has the job `label`.
    ///
    /// # Errors
    /// `launchctl` failed.
    fn loaded(&self, label: &str) -> Result<bool, PortError>;

    /// Loads the job of the plist `plist` into the system domain, which starts it (`RunAtLoad`).
    ///
    /// # Errors
    /// `launchctl` failed.
    fn bootstrap(&self, plist: &Path) -> Result<(), PortError>;

    /// Stops the job `label` and removes it from the domain, and returns when the domain no
    /// longer has it (its process has exited, the drain included). A job that is not loaded is
    /// not an error.
    ///
    /// # Errors
    /// `launchctl` failed, or the domain still has the job when its time is up.
    fn bootout(&self, label: &str) -> Result<(), PortError>;

    /// [`Launchd::bootout`] with bounds of its own, for a job that takes longer to exit than the
    /// service does: `launchctl bootout` may run `call` (it returns when the process has
    /// exited), and the domain is then looked at up to `gone_attempts` times.
    ///
    /// # Errors
    /// As for [`Launchd::bootout`].
    fn bootout_within(
        &self,
        label: &str,
        call: Duration,
        gone_attempts: u32,
    ) -> Result<(), PortError>;

    /// Sends the process of the job `label` SIGUSR2 (`launchctl kill SIGUSR2 system/<label>`): a
    /// build node of a release that lists `handoff-exec-v1` then hands over to the next image of
    /// its program, keeping its pid and its job. Returns once launchd has taken the signal; what
    /// the process does with it is seen on the public port.
    ///
    /// # Errors
    /// `launchctl` failed (a job that is not loaded included).
    fn hand_to_next_image(&self, label: &str) -> Result<(), PortError>;

    /// Disables the job `label` (`launchctl disable system/<label>`): launchd does not load it,
    /// at a boot or by a `bootstrap`, until it is enabled. A job already disabled is not an
    /// error.
    ///
    /// # Errors
    /// `launchctl` failed.
    fn disable(&self, label: &str) -> Result<(), PortError>;

    /// Enables the job `label` again (`launchctl enable system/<label>`); a job that is not
    /// disabled is not an error.
    ///
    /// # Errors
    /// `launchctl` failed.
    fn enable(&self, label: &str) -> Result<(), PortError>;
}

/// An answer to a GET.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The status.
    pub status: u16,
    /// The body, as text.
    pub body: String,
}

/// The instances' HTTP answers, on `127.0.0.1`.
pub trait Instances {
    /// `GET path` on `127.0.0.1:port`.
    ///
    /// # Errors
    /// No answer (refused, reset, timed out). A status other than 200 is a [`Reply`].
    fn get(&self, port: u16, path: &str) -> Result<Reply, PortError>;
}

/// What the host says of itself.
pub trait Host {
    /// The text of the node's configuration file `path`; `None` when there is no such file.
    ///
    /// # Errors
    /// The path exists and cannot be read.
    fn config(&self, path: &Path) -> Result<Option<String>, PortError>;

    /// Whether the host's list of mounts has one at `path` (the mount point as the file service's
    /// configuration names it, or the path it resolves to).
    ///
    /// # Errors
    /// The list cannot be had.
    fn mounted(&self, path: &Path) -> Result<bool, PortError>;

    /// Whether `path` is a directory, as the file system that holds it answers a look-up of it
    /// (`stat`, which follows links): what a job agent's `canonicalize` asks of a directory the
    /// mount provider answers. `false` when nothing is there.
    ///
    /// # Errors
    /// The look-up fails any other way (a mount whose server is gone answers `EIO` or
    /// `ETIMEDOUT` once the kernel marks it dead).
    fn is_dir(&self, path: &Path) -> Result<bool, PortError>;

    /// The capabilities (`bin/tierstore --capabilities`) of the release in `release`. A release
    /// that does not know the flag has none.
    ///
    /// # Errors
    /// The release's binary is missing, is not one to run, or cannot be run at all.
    fn capabilities(&self, release: &Path) -> Result<Vec<String>, PortError>;

    /// Has the release in `release` check the node's configuration file `config`
    /// (`bin/tierstore --config <config> --check-config`), as the service account would start it.
    /// `None` is a configuration the release accepts; `Some` is the line it refuses it with.
    ///
    /// # Errors
    /// The release's binary is missing, is not one to run, cannot be run at all, or ended any way
    /// but accepting or refusing; or the service account is not known to the host.
    fn check_config(&self, release: &Path, config: &Path) -> Result<Option<String>, PortError>;

    /// Puts the binary of the release in `release` where the units run it,
    /// [`SERVICE_BINARY`](super::SERVICE_BINARY), when the release has a binary of its own there to
    /// put (an archive release's `bin/tierstore`): a copy owned by root and readable by every
    /// user, which replaces what was there in one rename, so that a process running the old copy
    /// keeps it. Any other release (of the Debian or RPM package, or of the macOS installer
    /// package) has its binary put in place by its package manager: nothing is done.
    ///
    /// # Errors
    /// The archive binary is not one to run (a link, a directory, or not root's alone), or the
    /// copy cannot be made.
    fn install_binary(&self, release: &Path) -> Result<(), PortError>;

    /// Removes the copy [`Host::install_binary`] made; one that is not there is fine.
    ///
    /// # Errors
    /// It is there and cannot be removed.
    fn remove_binary(&self) -> Result<(), PortError>;
}

/// Time, and waiting for it to pass.
pub trait Clock {
    /// Milliseconds on a monotonic clock.
    fn now_ms(&self) -> u64;

    /// Waits `span`. The fake counts it and moves its time on, and waits for nothing.
    fn pause(&self, span: Duration);
}

/// The exclusive lock of the state directory, held for a whole step and released when dropped.
pub struct Lock {
    _guard: Box<dyn Any>,
}

impl Lock {
    /// A lock that is released when `guard` is dropped.
    pub fn new<T: 'static>(guard: T) -> Lock {
        Lock {
            _guard: Box::new(guard),
        }
    }
}

/// The state directory: the record, and the lock that keeps two steps apart.
pub trait StateDir {
    /// Checks the directory (root's alone, mode 0700, not a link), making it when it is absent,
    /// and takes the exclusive lock without waiting.
    ///
    /// # Errors
    /// [`PortError::Busy`] when another step holds the lock; another error when the directory is
    /// not as it must be.
    fn lock(&self) -> Result<Lock, PortError>;

    /// The bytes of `state.json`; `None` when there is none.
    ///
    /// # Errors
    /// The file cannot be read.
    fn load(&self) -> Result<Option<Vec<u8>>, PortError>;

    /// Replaces `state.json` with `bytes`: a temporary file, synced, renamed over, the directory
    /// synced. A reader sees the old bytes or the new.
    ///
    /// # Errors
    /// The file could not be written; the record is then what it was.
    fn save(&self, bytes: &[u8]) -> Result<(), PortError>;
}

/// The network guard of a Linux node, `tierstore-guard.service`, which loads the node's
/// `guard.nft`. Only the config hook touches it: a config package may replace that file.
pub trait Guard {
    /// Makes the guard load the node's guard file as it now is (`systemctl reload-or-restart`,
    /// which starts the unit when it is not running). A node without the guard file has no guard:
    /// nothing is done.
    ///
    /// # Errors
    /// The program failed.
    fn reload(&self) -> Result<(), PortError>;
}

/// The control socket of the `fs` interface of the instance that serves (`treefs/control.rs`):
/// only the config hook asks it, to apply a change of its configuration in place.
pub trait Control {
    /// Sends the one request `line` on the Unix socket `socket` (the path the node's file names)
    /// and returns the one line it is answered with, without its newline.
    ///
    /// # Errors
    /// No answer: the socket cannot be reached, the request cannot be written, or no whole line
    /// is read back.
    fn ask(&self, socket: &Path, line: &str) -> Result<String, PortError>;
}

/// What one path of a tree is, as a read of the mount finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeNode {
    /// A regular file, with its bytes.
    File(Vec<u8>),
    /// A directory.
    Dir,
    /// A symbolic link, with its target.
    Link(String),
}

/// The status of an answer of the mount provider that is not success, as gRPC numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderCode {
    /// `INVALID_ARGUMENT`: the request is wrong in itself.
    InvalidArgument,
    /// `FAILED_PRECONDITION`: what the request names cannot be used.
    FailedPrecondition,
    /// `UNAVAILABLE`: not now; ask again.
    Unavailable,
    /// Any other status, by its number.
    Other(i32),
}

/// Why a call of the mount provider did not succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderFault {
    /// No answer: the socket is not there or nobody listens on it, the call timed out, or what
    /// came back is not an answer of the protocol.
    Silent(String),
    /// An answer, with a status other than OK: the provider is there and said no.
    Answered {
        /// The status.
        code: ProviderCode,
        /// The provider's reason.
        reason: String,
    },
}

/// The node's mount provider (jobscheduler's `MountProvider` protocol on a Unix socket), asked as
/// jobscheduler's agent asks it, for the sources of the update check
/// (`provider/check.rs`: its agent and run, and the source's index), and the mount it answers.
pub trait MountProvider {
    /// `Prepare` of the check's source `index` with the tree list `reference`, read-only.
    ///
    /// # Errors
    /// No answer, or an answer that is not success.
    fn prepare(&self, socket: &Path, index: u32, reference: &str) -> Result<(), ProviderFault>;

    /// `Mount` of the check's source `index`: the directory of its tree.
    ///
    /// # Errors
    /// No answer, or an answer that is not success.
    fn mount(&self, socket: &Path, index: u32) -> Result<PathBuf, ProviderFault>;

    /// `Release` of the check's source `index`.
    ///
    /// # Errors
    /// No answer, or an answer that is not success.
    fn release(&self, socket: &Path, index: u32) -> Result<(), ProviderFault>;

    /// Every path below the directory `dir` with what it is: files with their bytes, directories,
    /// and links with their targets, never followed.
    ///
    /// # Errors
    /// [`ReadFault::Unread`] when the directory or a path below it could not be read, or the read
    /// did not end within its bound; [`ReadFault::NotTheTree`] when what is there is no tree of
    /// the check (a path of no kind of a tree, a name that is not UTF-8, more paths or a larger
    /// file than a tree of the check has). Neither has the bytes of any file.
    fn read_tree(&self, dir: &Path) -> Result<BTreeMap<String, TreeNode>, ReadFault>;
}

/// Why a read of a tree through the mount gave no tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFault {
    /// The system could not read a path, or the read did not end within its bound: the mount
    /// may be slow to fill it, as it is under load. What the system said.
    Unread(String),
    /// What the read found is no tree of the check. What is wrong.
    NotTheTree(String),
}

/// Everything a step of the systemd arm may touch.
pub struct SystemdPorts<'a> {
    /// The two instances' units of `tierstore@.service`.
    pub units: &'a dyn Units,
    /// Their start markers.
    pub markers: &'a dyn Markers,
    /// The two instances' units of `tierstore-fs@.service`.
    pub fs_units: &'a dyn Units,
    /// Their start markers.
    pub fs_markers: &'a dyn Markers,
    /// Their processes.
    pub fs_processes: &'a dyn Processes,
    /// The instances' answers.
    pub instances: &'a dyn Instances,
    /// The host.
    pub host: &'a dyn Host,
    /// The node's mount provider and its mount.
    pub provider: &'a dyn MountProvider,
    /// Time.
    pub clock: &'a dyn Clock,
    /// The state directory.
    pub state: &'a dyn StateDir,
}

/// Everything a step of the launchd arm may touch: no second instance, no marker.
pub struct LaunchdPorts<'a> {
    /// The launchd domain.
    pub launchd: &'a dyn Launchd,
    /// The instance's answers.
    pub instances: &'a dyn Instances,
    /// The host.
    pub host: &'a dyn Host,
    /// The node's mount provider and its mount.
    pub provider: &'a dyn MountProvider,
    /// Time.
    pub clock: &'a dyn Clock,
    /// The state directory.
    pub state: &'a dyn StateDir,
}
