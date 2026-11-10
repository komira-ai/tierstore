//! What a start of `tierstore fs` does before a frontend serves: the part that has no mount in it,
//! so that the FUSE frontend (`run.rs`, Linux) and an NFS frontend call the same code. It compiles
//! on Linux, and on every target in the tests.
//!
//! A start, in order: the runtime directory and the mount point are made; the runtime directory
//! gets a table of tree numbers (afresh, or, for an NFS mount that outlives the server, the one the
//! mount of this boot left, see [`Identity`]); the backing table of an earlier mount is removed; the
//! core is made over the fill socket; the attachments the file holds are made again (the manifests
//! are read through the fill socket); the cache is told which trees are attached, so that it
//! drops the pins of any other; and the control socket is bound.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use super::attach::Attachments;
use super::backing;
use super::config::FsSettings;
use super::control::Control;
use super::core::{self, Core};
use super::fill_client::FillClient;
use super::table::{TableError, TreeTable};

/// Why a start was refused.
#[derive(Debug, thiserror::Error)]
pub(in crate::server) enum StartError {
    /// The cache directory is on a stacked file system.
    #[cfg(target_os = "linux")]
    #[error(
        "fs.cache_path {path}: the file system is {name}, a stacked one; the job's overlay above \
         the mount would pass the kernel's depth of stacking; put the cache on XFS or ext4"
    )]
    Stacked {
        /// The cache directory.
        path: String,
        /// The kind of the file system.
        name: &'static str,
    },
    /// Something the start needs could not be done.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: String,
        /// What the system said.
        source: std::io::Error,
    },
    /// The table of tree numbers could not be made.
    #[error("{0}")]
    Table(#[from] TableError),
}

impl StartError {
    /// The exit status of a start that this refused: 3 for a cache on a stacked file system, as
    /// for a cache directory whose file system fails the check of the cache, else 1.
    pub(in crate::server) fn status(&self) -> u8 {
        #[cfg(target_os = "linux")]
        if matches!(self, Self::Stacked { .. }) {
            return 3;
        }
        1
    }
}

/// The error of `what` failing with `source`.
pub(super) fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> StartError {
    let what = what.into();
    move |source| StartError::Io { what, source }
}

/// An id for this start: sixteen random bytes as hex, which the pins are made under.
fn new_mount_id() -> Result<String, StartError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(io("reading /dev/urandom"))?;
    Ok(hex::encode(bytes))
}

/// Removes the file at `path`, which a mount that is gone left; none is no fault.
fn remove_stale(path: &Path) -> Result<(), StartError> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(StartError::Io {
            what: format!("removing {}", path.display()),
            source: error,
        }),
        _ => Ok(()),
    }
}

/// Whose mount a start is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Identity {
    /// A new mount, with a new connection: the table of tree numbers is made afresh and the mount
    /// id is random (FUSE).
    Fresh,
    /// The mount of this boot, which a restart of the server does not end: the table of tree
    /// numbers is reopened when it is of this epoch (a nonzero hash of the boot), and the mount id
    /// is the epoch, so that the pins of the earlier start stay this mount's (NFS).
    Boot(u32),
}

/// What a start makes before a frontend serves: the core, and the control socket it is driven by.
pub(super) struct Setup {
    /// The core a frontend serves from.
    pub core: Arc<Core>,
    /// The control socket; it is closed when this is dropped.
    pub _control: Control,
    /// Whether the table of tree numbers of an earlier start was reopened.
    pub reopened: bool,
}

impl Setup {
    /// Does what a start does up to a frontend's mount. `count_lookups` is whether the core counts
    /// the lookups of its inodes (a frontend that is told when they are forgotten, FUSE), or drops
    /// a tree when it is detached (one that is not told).
    ///
    /// # Errors
    /// [`StartError`]: why nothing was made.
    pub(super) fn start(
        settings: &FsSettings,
        count_lookups: bool,
        identity: Identity,
    ) -> Result<Self, StartError> {
        std::fs::create_dir_all(&settings.runtime_dir).map_err(io(format!(
            "fs.runtime_dir {}",
            settings.runtime_dir.display()
        )))?;
        std::fs::create_dir_all(&settings.mountpoint).map_err(io(format!(
            "fs.mountpoint {}",
            settings.mountpoint.display()
        )))?;
        let (table, reopened, mount_id) = match identity {
            Identity::Fresh => (
                TreeTable::create(&settings.runtime_dir)?,
                false,
                new_mount_id()?,
            ),
            Identity::Boot(epoch) => {
                let (table, reopened) = TreeTable::open_or_create(&settings.runtime_dir, epoch)?;
                (table, reopened, format!("nfs-{epoch:08x}"))
            }
        };
        // The backing ids of an earlier mount belong to its connection, which is gone.
        remove_stale(&settings.runtime_dir.join(backing::FILE))?;
        let fills = FillClient::new(settings.fill_socket.clone(), settings.fetch_timeout);
        let core = Arc::new(Core::new(
            core::Settings {
                owner: settings.owner,
                cache_path: settings.cache_path.clone(),
                mountpoint: settings.mountpoint.clone(),
                mount_id,
                max_backing_ids: settings.max_backing_ids,
                count_lookups,
            },
            fills,
            table,
            &settings.runtime_dir,
        ));
        let saved = Attachments::saved(&settings.runtime_dir)
            .map_err(io("reading the attachments file"))?;
        core.restore(saved);
        core.pins_live();
        let control = Control::serve(
            Arc::clone(&core),
            &settings.control_socket,
            settings.client_group,
        )
        .map_err(io(format!(
            "fs.control_socket {}",
            settings.control_socket.display()
        )))?;
        Ok(Self {
            core,
            _control: control,
            reopened,
        })
    }
}

/// The signals that end the process, caught.
pub(super) struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

impl Signals {
    /// Catches SIGTERM and SIGINT from now on: one that comes before [`Signals::wait`] is kept.
    /// This must run inside a Tokio runtime.
    pub(super) fn install() -> Result<Self, StartError> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate()).map_err(io("handling SIGTERM"))?,
            int: signal(SignalKind::interrupt()).map_err(io("handling SIGINT"))?,
        })
    }

    /// Waits for SIGTERM or SIGINT.
    pub(super) async fn wait(&mut self) {
        let _ =
            futures::future::select(Box::pin(self.term.recv()), Box::pin(self.int.recv())).await;
    }
}

/// Tests that send signals to the process go one at a time: a signal is for every handler.
#[cfg(test)]
pub(super) static SIGNALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[path = "start_tests.rs"]
mod tests;
