//! What the steps do to the machine, as ports: the nftables rules, the units, the slot links,
//! the instances' HTTP answers, what the host says of itself, the clock and the state
//! directory.
//!
//! Each port is a small trait; the steps take a [`Ports`] and never reach for the machine
//! themselves. Tests keep the machine's world in a fake. Nothing here reads an environment
//! variable. The machine's implementations come in a later change.
//!
//! A port that changes something is idempotent: stopping a stopped unit, removing an absent
//! link and applying a ruleset twice are not errors.

use std::any::Any;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Which port failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortKind {
    /// The nftables rules.
    Nft,
    /// The units.
    Units,
    /// The slot links.
    Slots,
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
            PortKind::Nft => "nft",
            PortKind::Units => "units",
            PortKind::Slots => "slots",
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

/// The nftables rules.
pub trait Nft {
    /// Applies `ruleset` in one transaction (`nft -f -`, the text on standard input).
    ///
    /// # Errors
    /// The program failed; the rules are then as they were.
    fn apply(&self, ruleset: &str) -> Result<(), PortError>;

    /// The listing of the table `table` of the `inet` family; `None` when there is no such table.
    ///
    /// # Errors
    /// The program failed.
    fn list_table(&self, table: &str) -> Result<Option<String>, PortError>;

    /// Deletes the table `table` of the `inet` family; an absent table is fine.
    ///
    /// # Errors
    /// The program failed.
    fn delete_table(&self, table: &str) -> Result<(), PortError>;
}

/// The unit manager.
pub trait Units {
    /// Starts `unit` and returns when the manager has started it. A unit that cannot start
    /// because its condition fails (no link) is not an error: it is not active.
    ///
    /// # Errors
    /// The program failed.
    fn start(&self, unit: &str) -> Result<(), PortError>;

    /// Stops `unit` and returns when its process has exited (the drain included).
    ///
    /// # Errors
    /// The program failed.
    fn stop(&self, unit: &str) -> Result<(), PortError>;
}

/// The slot links: `<state_dir>/slots/<port>` points at a staged release, or is absent.
pub trait Slots {
    /// Where the link of `port` points; `None` when there is none.
    ///
    /// # Errors
    /// The path exists and is not a readable link.
    fn target(&self, port: u16) -> Result<Option<PathBuf>, PortError>;

    /// Points the link of `port` at `release`, atomically (a new link renamed over the old).
    ///
    /// # Errors
    /// The link could not be made; it is then what it was.
    fn point(&self, port: u16, release: &Path) -> Result<(), PortError>;

    /// Removes the link of `port`; an absent link is fine.
    ///
    /// # Errors
    /// The link could not be removed.
    fn remove(&self, port: u16) -> Result<(), PortError>;
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
    /// `GET path` on `127.0.0.1:port`. The public port is asked too: the answer says which
    /// instance the redirect sends a client to.
    ///
    /// # Errors
    /// No answer (refused, reset, timed out). A status other than 200 is a [`Reply`].
    fn get(&self, port: u16, path: &str) -> Result<Reply, PortError>;
}

/// What the host says of its connection tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conntrack {
    /// `nf_conntrack_tcp_timeout_established`, in seconds.
    pub tcp_timeout_established_secs: u64,
    /// `nf_conntrack_count`.
    pub count: u64,
    /// `nf_conntrack_max`.
    pub max: u64,
}

/// What the host says of itself.
pub trait Host {
    /// The text of the declaration (`/etc/updater.d/tierstore.toml`).
    ///
    /// # Errors
    /// The file cannot be read.
    fn declaration(&self) -> Result<String, PortError>;

    /// The identifier of this boot of the host (`/proc/sys/kernel/random/boot_id`).
    ///
    /// # Errors
    /// It cannot be read.
    fn boot_id(&self) -> Result<String, PortError>;

    /// The connection tracking limits and fill.
    ///
    /// # Errors
    /// A value cannot be read.
    fn conntrack(&self) -> Result<Conntrack, PortError>;

    /// The capabilities (`tierstore --capabilities`) of the release in `release`. A release
    /// that does not know the flag has none.
    ///
    /// # Errors
    /// The release's binary cannot be run at all.
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
    /// Checks the directory (owner, mode, not a link, not writable by the service) and takes the
    /// exclusive lock without waiting.
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

/// Everything a step may touch.
pub struct Ports<'a> {
    /// The nftables rules.
    pub nft: &'a dyn Nft,
    /// The units.
    pub units: &'a dyn Units,
    /// The slot links.
    pub slots: &'a dyn Slots,
    /// The instances' answers.
    pub instances: &'a dyn Instances,
    /// The host.
    pub host: &'a dyn Host,
    /// Time.
    pub clock: &'a dyn Clock,
    /// The state directory.
    pub state: &'a dyn StateDir,
    /// The public port the redirect sends to the serving slot ([`PUBLIC_PORT`] on a host).
    ///
    /// [`PUBLIC_PORT`]: super::PUBLIC_PORT
    pub public_port: u16,
}
