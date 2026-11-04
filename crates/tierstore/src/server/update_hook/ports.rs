//! What the steps do to the machine, as ports: the two systemd instances and their start
//! markers, the launchd job, the instances' HTTP answers, what the host says of itself, the
//! clock and the state directory.
//!
//! Each port is a small trait; the steps take a [`SystemdPorts`] or a [`LaunchdPorts`] and never
//! reach for the machine themselves. Tests keep the machine's world in a fake. Nothing here reads
//! an environment variable. [`super::machine`] has the machine's implementations.
//!
//! A port that changes something is idempotent: starting a running instance, stopping a stopped
//! one, removing an absent marker and booting out a job that is not loaded are not errors.

use std::any::Any;
use std::fmt;
use std::path::Path;
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

/// One of the two instances of the systemd template `tierstore@.service`.
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

    /// The unit's name: `tierstore@a.service` or `tierstore@b.service`.
    #[must_use]
    pub fn unit(self) -> String {
        format!("tierstore@{}.service", self.letter())
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

/// The systemd units of the two instances.
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

    /// The capabilities (`bin/tierstore --capabilities`) of the release in `release`. A release
    /// that does not know the flag has none.
    ///
    /// # Errors
    /// The release's binary is missing, is not one to run, or cannot be run at all.
    fn capabilities(&self, release: &Path) -> Result<Vec<String>, PortError>;
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

/// Everything a step of the systemd arm may touch.
pub struct SystemdPorts<'a> {
    /// The two instances' units.
    pub units: &'a dyn Units,
    /// Their start markers.
    pub markers: &'a dyn Markers,
    /// The instances' answers.
    pub instances: &'a dyn Instances,
    /// The host.
    pub host: &'a dyn Host,
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
    /// Time.
    pub clock: &'a dyn Clock,
    /// The state directory.
    pub state: &'a dyn StateDir,
}
