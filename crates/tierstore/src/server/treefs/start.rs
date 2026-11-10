//! What a start of `tierstore fs` does before a frontend serves: the part that has no mount in it,
//! so that the FUSE frontend (`run.rs`, Linux) and an NFS frontend call the same code. It compiles
//! on Linux, and on every target in the tests.
//!
//! A start, in order: the runtime directory and the mount point are made; the runtime directory
//! gets a table of tree numbers afresh; the backing table of an earlier mount is removed; the
//! core is made over the fill socket; the attachments the file holds are made again (the manifests
//! are read through the fill socket); the cache is told which trees are attached, so that it
//! drops the pins of any other; and the control socket is bound.
//!
//! The pieces a process that takes a connection over also needs (the directories, the core over a
//! table that was read, the control socket's error) are here too: `run.rs` calls them.

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
    /// The record of the `INIT` handshake could not be read or written.
    #[cfg(target_os = "linux")]
    #[error("{0}")]
    Record(#[from] super::init_record::RecordError),
    /// The hand-over of the mount from the running instance failed.
    #[cfg(target_os = "linux")]
    #[error("{0}")]
    Handover(#[from] super::handover::HandoverError),
    /// Requests of the connection stayed unanswered after a restart on a kernel that cannot send
    /// them again, and the connection was aborted so that they fail; the unit starts again.
    #[cfg(target_os = "linux")]
    #[error("the connection was aborted: requests the process before read stayed unanswered")]
    Aborted,
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

/// An id for this mount: sixteen random bytes as hex, which the pins are made under.
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

/// Makes the runtime directory.
pub(super) fn create_runtime_dir(settings: &FsSettings) -> Result<(), StartError> {
    std::fs::create_dir_all(&settings.runtime_dir).map_err(io(format!(
        "fs.runtime_dir {}",
        settings.runtime_dir.display()
    )))
}

/// Makes the mount point. A mount point that is a live mount of a connection taken over must not be
/// made (it is a `stat` of the mount): only a start that mounts calls this.
pub(super) fn create_mount_point(settings: &FsSettings) -> Result<(), StartError> {
    std::fs::create_dir_all(&settings.mountpoint).map_err(io(format!(
        "fs.mountpoint {}",
        settings.mountpoint.display()
    )))
}

/// A core over `table` for the mount `mount_id`, whose file handles are numbered for `generation`,
/// with the attachments the file holds made again and the cache told which trees are attached.
/// `count_lookups` is whether the core counts the lookups of its inodes (a frontend that is told
/// when they are forgotten, FUSE), or drops a tree when it is detached (one that is not told).
pub(super) fn core_of(
    settings: &FsSettings,
    table: TreeTable,
    (mount_id, generation): (String, u32),
    count_lookups: bool,
) -> Result<Arc<Core>, StartError> {
    let fills = FillClient::new(settings.fill_socket.clone(), settings.fetch_timeout);
    let core = Arc::new(Core::new(
        core::Settings {
            owner: settings.owner,
            cache_path: settings.cache_path.clone(),
            mountpoint: settings.mountpoint.clone(),
            mount_id,
            max_backing_ids: settings.max_backing_ids,
            generation,
            count_lookups,
        },
        fills,
        table,
        &settings.runtime_dir,
    ));
    let saved =
        Attachments::saved(&settings.runtime_dir).map_err(io("reading the attachments file"))?;
    core.restore(saved);
    core.pins_live();
    Ok(core)
}

/// The text `what` names: the control socket's path in an error.
pub(super) fn control_error(settings: &FsSettings) -> impl FnOnce(std::io::Error) -> StartError {
    io(format!(
        "fs.control_socket {}",
        settings.control_socket.display()
    ))
}

/// What a start makes before a frontend serves: the core, and the control socket it is driven by.
pub(super) struct Setup {
    /// The core a frontend serves from.
    pub core: Arc<Core>,
    /// The id of this mount, which the pins are made under.
    pub mount_id: String,
    /// The control socket; it is closed when this is dropped.
    pub _control: Control,
}

impl Setup {
    /// Does what a start does up to a frontend's mount: the directories are made (a start that has
    /// made them already is not harmed), the table of tree numbers is made afresh, and the core and
    /// the control socket are made. `count_lookups` is as for [`core_of`].
    ///
    /// # Errors
    /// [`StartError`]: why nothing was made.
    pub(super) fn start(settings: &FsSettings, count_lookups: bool) -> Result<Self, StartError> {
        create_runtime_dir(settings)?;
        create_mount_point(settings)?;
        let table = TreeTable::create(&settings.runtime_dir)?;
        // The backing ids of an earlier mount belong to its connection, which is gone.
        remove_stale(&settings.runtime_dir.join(backing::FILE))?;
        let mount_id = new_mount_id()?;
        let core = core_of(settings, table, (mount_id.clone(), 1), count_lookups)?;
        let control = Control::serve(
            Arc::clone(&core),
            &settings.control_socket,
            settings.client_group,
        )
        .map_err(control_error(settings))?;
        Ok(Self {
            core,
            mount_id,
            _control: control,
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

#[cfg(test)]
#[path = "start_tests.rs"]
mod tests;
