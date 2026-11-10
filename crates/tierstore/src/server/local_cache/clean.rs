//! What the cache does about a stop that was not clean.
//!
//! The cache is a copy: whatever it holds can be read again from the upstream. So a fill does not
//! ask the disk to make its bytes durable (a flush of the drive cache on every blob costs about
//! 10 ms on macOS, which made a cold attach of 12,394 manifests take 134 s). The price is that
//! after a power cut a file the cache had linked may be short or full of zeros, with a sidecar
//! that says it is whole: a rename is atomic but not durable without a sync, and the data of a
//! file and its name reach the disk in no set order. Such a file must never be served.
//!
//! The rule is a marker file, `.clean`, in the cache directory:
//!
//! - A process that holds the directory removes the marker when it starts (and syncs the
//!   directory, so the removal is on the disk before any fill is). A marker it did not find says
//!   the last process did not end cleanly: a crash, a kill, a power cut, or a release that did
//!   not write one.
//! - A process that ends in order (the halt of [`LocalCache::hold`](super::LocalCache::hold),
//!   after the fronts drained and the pins were written) flushes the whole machine once and
//!   then writes the marker. It writes none while a fill or a keep still has a stage file.
//! - After an unclean stop every object the scan finds is unverified: the first read of it
//!   hashes the file and compares the SHA-256 with the digest of its sidecar (the name, for a key
//!   named by its content), the check of the scrub. A file that does not match is dropped and
//!   the read is a miss, which fills it again; one that does is served from then on with no more
//!   cost. After a clean stop there is no check and no cost. A scan that finds a sidecar that
//!   does not parse, or a size that is not the sidecar's, already deletes the object.
//!
//! A cache that does not hold the lock does not touch the marker. It treats a directory with no
//! marker as unclean, as it cannot know whether the holder has checked its objects, and checks
//! the hits it serves the same way (it drops nothing: a mismatch is a miss).

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::metrics::bump;
use super::scrub::{Scrubber, Verdict};
use super::{Inner, locked};

/// The marker file of a directory whose last holder ended in order.
pub(super) const CLEAN: &str = ".clean";

/// Whether the marker is in `root`.
pub(super) fn sealed(root: &Path) -> bool {
    root.join(CLEAN).is_file()
}

/// Removes the marker of `root` and gives whether the last stop was unclean (there was none). The
/// directory is synced after a removal, so that a power cut after the first fill cannot bring the
/// marker back.
///
/// # Errors
/// What the system said about removing the marker or syncing the directory.
pub(super) fn take(root: &Path) -> io::Result<bool> {
    match std::fs::remove_file(root.join(CLEAN)) {
        Ok(()) => File::open(root)
            .and_then(|directory| directory.sync_all())
            .map(|()| false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// Writes the marker of `root` and makes it durable.
fn write_marker(root: &Path) -> io::Result<()> {
    File::create(root.join(CLEAN)).and_then(|marker| marker.sync_all())
}

impl Inner {
    /// Marks the directory as left in order: the end of the cache's work. Nothing is written
    /// by a cache that is read-only, or while a fill or a keep has a stage file, or if the flush
    /// of the disk failed: the next start then checks what it reads.
    pub(super) fn seal(&self) {
        if !self.read_write() || self.counters.staging_files.load(Ordering::SeqCst) != 0 {
            return;
        }
        let sealed = self
            .disk
            .flush_all()
            .and_then(|()| write_marker(&self.config.path));
        if let Err(why) = sealed {
            tracing::warn!("tierstore: local cache: the clean-stop marker was not written: {why}");
        }
    }

    /// Whether the object `hex` may be served: it is, unless the scan found it after an unclean
    /// stop and it has not been checked since. The check hashes the file (see the module
    /// documentation). An object that fails it is dropped, if this cache may drop it, and the
    /// read is a miss; one that could not be judged (a fill of the name is running, the disk
    /// failed) is a miss and stays unverified.
    pub(super) async fn vouched(self: &Arc<Self>, hex: &str) -> bool {
        if !locked(&self.index).is_unverified(hex) {
            return true;
        }
        let (inner, name) = (Arc::clone(self), hex.to_owned());
        let judging = tokio::task::spawn_blocking(move || {
            inner.scrub_object(&name, &Scrubber::new(1), &|| {})
        });
        match judging.await.unwrap_or(Verdict::Left) {
            Verdict::Intact(_) => {
                locked(&self.index).verified(hex);
                true
            }
            Verdict::Dropped(_) => {
                bump(&self.counters.scrub_mismatches);
                false
            }
            Verdict::Left | Verdict::Stopped => false,
        }
    }
}
