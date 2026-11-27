//! The file of the provider's socket (`listener`): bound replacing the file an instance that is
//! gone left there, with mode 0600, and removed by the instance that made it when it goes, if the
//! name still leads to it, so that the removal by an instance that is going cannot take the file
//! of the one that took over.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::UnixListener;

/// How long the listener waits after the socket could not be bound before it tries again.
pub(super) const REBIND_PAUSE: Duration = Duration::from_secs(1);

/// The socket file this process made, and removed when it is dropped if the name still leads to
/// it.
pub(super) struct SocketFile {
    path: PathBuf,
    identity: (u64, u64),
}

impl SocketFile {
    /// What the name `path` leads to now: its device and inode.
    fn identity_of(path: &Path) -> Option<(u64, u64)> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        Some((meta.dev(), meta.ino()))
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if Self::identity_of(&self.path) == Some(self.identity) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Binds `path`, replacing the file an instance that is gone left there, with mode 0600: the
/// listener, and the file, which is removed when it is dropped if the name still leads to it.
pub(super) fn bind_socket(path: &Path) -> std::io::Result<(UnixListener, SocketFile)> {
    match std::fs::remove_file(path) {
        Err(why) if why.kind() != std::io::ErrorKind::NotFound => return Err(why),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    let file = SocketFile {
        path: path.to_owned(),
        identity: SocketFile::identity_of(path).unwrap_or_default(),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, file))
}

/// Waits `pause` if `wanted`; never otherwise.
pub(super) async fn pause_if(wanted: bool, pause: Duration) {
    if wanted {
        tokio::time::sleep(pause).await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
#[path = "socket_tests.rs"]
mod tests;
