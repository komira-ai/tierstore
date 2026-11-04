//! The seam through which the directory backend makes its changes durable, and through which a
//! test sees them.

use std::path::Path;

use async_trait::async_trait;
use tokio::fs::File;

/// Makes the changes to the entries of a directory durable. It is a seam so that a test can see
/// when the backend syncs a directory, relative to the change and to the acknowledgement.
#[async_trait]
pub(super) trait SyncDir: Send + Sync {
    /// Returns once the entries of `dir` are on the disk.
    async fn sync_dir(&self, dir: &Path) -> std::io::Result<()>;

    /// Called on the blocking thread of an entry write just before it waits for the lock between
    /// processes, so that a test knows the write has reached the lock.
    fn lock_reached(&self) {}
}

/// Syncs a directory with `fsync` on it.
pub(super) struct FsyncDir;

#[async_trait]
impl SyncDir for FsyncDir {
    async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        File::open(dir).await?.sync_all().await
    }
}
