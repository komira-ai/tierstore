//! The process of `tierstore fs` on Linux: the FUSE mount around the neutral start (`start.rs`),
//! and the wait for the signal that ends it.
//!
//! A start, in order: the cache directory must be on a file system that is not stacked (the job's
//! overlay above this mount must stay within the kernel's depth of stacking); the neutral start
//! is done (`Setup`: a table of tree numbers afresh, the attachments made again, the control
//! socket bound); the mount is made; and the service manager is told it is ready. SIGTERM and
//! SIGINT unmount and end the process with status 0.

use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::sync::Arc;

use fuser::{BackgroundSession, Session};

use super::config::FsSettings;
use super::control::Control;
use super::core::Core;
use super::fuse::{self, Frontend};
use super::start::{Setup, Signals, StartError, io};

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

/// A started `tierstore fs`: the core, the control socket and the mount.
pub(super) struct Running {
    _core: Arc<Core>,
    _control: Control,
    session: BackgroundSession,
}

impl Running {
    /// Does what a start does, up to and including the mount.
    ///
    /// # Errors
    /// [`StartError`]: why nothing was mounted.
    pub(super) fn start(settings: &FsSettings) -> Result<Self, StartError> {
        check_cache(&settings.cache_path)?;
        let Setup { core, _control } = Setup::start(settings, true)?;
        let config = fuse::config(settings.threads, settings.allow_other);
        let session = Session::new(
            Frontend::new(Arc::clone(&core), settings.passthrough),
            &settings.mountpoint,
            &config,
        )
        .and_then(Session::spawn)
        .map_err(io(format!("mounting {}", settings.mountpoint.display())))?;
        Ok(Self {
            _core: core,
            _control,
            session,
        })
    }

    /// The core the mount is served from.
    #[cfg(test)]
    pub(super) fn core(&self) -> &Arc<Core> {
        &self._core
    }

    /// Unmounts and waits for the threads of the mount to end.
    ///
    /// # Errors
    /// What the system said when the mount could not be unmounted.
    pub(super) fn stop(self) -> Result<(), StartError> {
        self.session.umount_and_join().map_err(io("unmounting"))
    }
}

/// Tells the service manager that the unit is ready: `READY=1` to the datagram socket systemd's
/// `NOTIFY_SOCKET` names. Nothing is sent where the process was not started by systemd with
/// `Type=notify`.
pub(super) fn notify_ready(socket: Option<&std::ffi::OsStr>) {
    use std::os::linux::net::SocketAddrExt as _;
    use std::os::unix::ffi::OsStrExt as _;
    let Some(socket) = socket else {
        return;
    };
    let bytes = socket.as_bytes();
    let sent = UnixDatagram::unbound().and_then(|datagram| match bytes.strip_prefix(b"@") {
        Some(name) => {
            let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            datagram.send_to_addr(b"READY=1", &address)
        }
        None => datagram.send_to(b"READY=1", Path::new(socket)),
    });
    if let Err(why) = sent {
        tracing::warn!("tierstore fs: cannot tell the service manager it is ready: {why}");
    }
}

/// The `NOTIFY_SOCKET` a service manager gave this process.
#[expect(
    clippy::disallowed_methods,
    reason = "NOTIFY_SOCKET is set by systemd for a Type=notify unit, a platform-set marker (AGENTS.md S2), not configuration"
)]
fn notify_socket() -> Option<std::ffi::OsString> {
    std::env::var_os("NOTIFY_SOCKET")
}

/// Starts, tells the service manager at `notify`, and waits for SIGTERM or SIGINT; then unmounts.
///
/// # Errors
/// [`StartError`]: why nothing was mounted, or why the mount could not be unmounted.
pub(super) fn serve_notifying(
    settings: &FsSettings,
    notify: Option<&std::ffi::OsStr>,
) -> Result<(), StartError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io("starting the runtime"))?;
    runtime.block_on(async {
        // The handlers are in place before the mount is: a signal after it is never lost.
        let mut signals = Signals::install()?;
        let running = Running::start(settings)?;
        notify_ready(notify);
        signals.wait().await;
        running.stop()
    })
}

/// [`serve_notifying`], telling the socket the service manager gave this process.
///
/// # Errors
/// As [`serve_notifying`].
pub(in crate::server) fn serve(settings: &FsSettings) -> Result<(), StartError> {
    serve_notifying(settings, notify_socket().as_deref())
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
