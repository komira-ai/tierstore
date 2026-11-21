//! The bytes of a file of a tree: the cache file of its blob, or the span of the pack a file of a
//! placed tree is, filled through the fill socket first if the cache does not hold them.
//!
//! # One descriptor per file of the cache
//!
//! Every open of a file of a tree is a handle that holds the file it reads, until the kernel
//! releases it. The opens of a build's jobs open the same headers over and over, and the kernel
//! may release them long after the job closed them (a job's files are opened through its
//! overlay, whose lower layer is the mount). The handles of one file
//! therefore share one descriptor of the file of the cache ([`OpenFiles`]), so that the process
//! holds one descriptor per blob, or per pack, however many handles there are, and not one per
//! open: those would run out (`EMFILE`), and every open after them would fail.
//!
//! A descriptor is shared while the name in the cache still leads to the file it is open on: a
//! file of the cache that was replaced since (an evicted pack filled again is a new file) is
//! opened again, so that no span is read from a file that does not hold it.
//!
//! # A fill that fails
//!
//! A fill that fails for a reason that can pass (the node's permits are busy, the upstream
//! answered 5xx, the answer did not come in `fs.fetch_timeout`, the node is being replaced, a file
//! could not be opened) is asked again, after a pause of a second, [`FILL_TRIES`] times in all:
//! the longest an open waits for its bytes is three times `fs.fetch_timeout` and two seconds. A
//! fill that still fails, or fails for a reason that does not pass (the upstream has no such
//! object, the bytes do not hash to the digest), fails the open with `EIO`. It is recorded on the
//! attachments of the tree, the runs' and the lookup's (`attach.rs`), and said on stderr at warn:
//! the tree, the path of the file in it, the blob and where it is read from, and the last answer.
//! The lines of one tree are said at most once in [`SAY_EVERY_MS`], each with the count of the
//! failures since the line before.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use crate::server::local_cache::blob_path;
use crate::tree_manifest::{BlobSource, TreeDigest, TreeEntry};

use super::super::attach::Failure;
use super::super::locked;
use super::super::tree::Tree;
use super::read::FileBytes;
use super::{Core, Errno, Loaded};

/// How many times a fill that fails for a reason that can pass is asked, the first time included.
pub(in crate::server::treefs) const FILL_TRIES: u32 = 3;

/// The least time between two lines about the failed opens of one tree, in milliseconds.
pub(in crate::server::treefs) const SAY_EVERY_MS: u64 = 10_000;

/// How many maps the shared descriptors are split in.
const SHARDS: usize = 16;

/// The least size of a map of the shared descriptors at which those that are closed are dropped:
/// then again at twice the size it has after.
const SWEEP_AT: usize = 64;

/// The words a fill that can pass is answered with, after the `fill: ` of a span: the node's
/// permits were busy, its upstream failed, it lost the lock of the cache or the bytes it read, or
/// its fill was cut short or stopped.
const PASSING: [&str; 8] = [
    "busy",
    "short",
    "read-only",
    "changed",
    "aborted",
    "io",
    "no space",
    "the filled range is no longer in the cache",
];

/// The words of the fill client when an answer did not come in time (`fill_client.rs`): the
/// node's fill goes on, and a fill asked again joins it.
const NO_ANSWER: &str = "the fill socket gave no answer in time";

/// Whether a fill that ended with `reason` may succeed if it is asked again: see [`PASSING`];
/// an upstream that answered a status of 500 or above but 501 (which is not implemented, and
/// stays so); no answer in time; and a file of the cache that could not be opened (`io: ...`).
pub(in crate::server::treefs) fn passes(reason: &str) -> bool {
    let words = reason.strip_prefix("fill: ").unwrap_or(reason);
    if let Some(status) = words.strip_prefix("upstream ") {
        return status
            .parse::<u16>()
            .is_ok_and(|status| status >= 500 && status != 501);
    }
    words == NO_ANSWER || words.starts_with("io: ") || PASSING.contains(&words)
}

/// What a failed open of a file of the cache is recorded as: `io: ` and what the system said.
fn unopened(error: &io::Error) -> String {
    format!("io: {error}")
}

/// What opens a file of the cache: `File::open`, or what a test puts in its place.
pub(in crate::server::treefs) type Opener = Box<dyn Fn(&Path) -> io::Result<File> + Send + Sync>;

/// A file of the cache open for reading, with the device and inode it was opened on.
struct Open {
    file: Weak<File>,
    id: (u64, u64),
}

/// The files of the cache open for the handles of the core, by path: each shared by every
/// handle of it while one is open.
pub(in crate::server::treefs) struct OpenFiles {
    shards: [Mutex<Shard>; SHARDS],
    /// Opens a file of the cache.
    open: Opener,
}

/// One map of the shared descriptors, and how large it may grow before the closed are dropped.
#[derive(Default)]
struct Shard {
    files: HashMap<PathBuf, Open>,
    sweep_at: usize,
}

impl OpenFiles {
    /// No file open; the files are opened with `File::open`.
    pub(in crate::server::treefs) fn new() -> Self {
        Self::opening(Box::new(|path| File::open(path)))
    }

    /// No file open; the files are opened with `open`.
    pub(in crate::server::treefs) fn opening(open: Opener) -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(Shard::default())),
            open,
        }
    }

    /// The map that holds `path`.
    fn shard(&self, path: &Path) -> &Mutex<Shard> {
        use std::hash::{BuildHasher as _, BuildHasherDefault, DefaultHasher};
        let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(path);
        // The remainder is below `SHARDS`.
        &self.shards[(hash % SHARDS as u64) as usize]
    }

    /// The file at `path`, shared with every handle that has it open while the name leads to the
    /// same file; opened under the lock of its map, so that two opens of a file at once open it
    /// once.
    ///
    /// # Errors
    /// What the system said when there is no file at `path` or it cannot be opened.
    pub(in crate::server::treefs) fn get(&self, path: &Path) -> io::Result<Arc<File>> {
        let id = std::fs::metadata(path).map(|meta| (meta.dev(), meta.ino()))?;
        let mut shard = locked(self.shard(path));
        let held = shard
            .files
            .get(path)
            .filter(|open| open.id == id)
            .and_then(|open| open.file.upgrade());
        if let Some(file) = held {
            return Ok(file);
        }
        let opened = (self.open)(path)?;
        // The file opened, which another may have replaced at the name since the look.
        let id = opened
            .metadata()
            .map_or(id, |meta| (meta.dev(), meta.ino()));
        let file = Arc::new(opened);
        let open = Open {
            file: Arc::downgrade(&file),
            id,
        };
        let _ = shard.files.insert(path.to_owned(), open);
        if shard.files.len() > shard.sweep_at {
            shard.files.retain(|_, open| open.file.strong_count() > 0);
            shard.sweep_at = (shard.files.len() * 2).max(SWEEP_AT);
        }
        Ok(file)
    }

    /// The number of paths the maps hold, of files open or closed.
    #[cfg(test)]
    pub(in crate::server::treefs) fn entries(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| locked(shard).files.len())
            .sum()
    }

    /// The number of files open now, shared or not.
    #[cfg(test)]
    pub(in crate::server::treefs) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                let shard = locked(shard);
                shard
                    .files
                    .values()
                    .filter(|open| open.file.strong_count() > 0)
                    .count()
            })
            .sum()
    }
}

/// The lines said about the failed opens of one tree: when the last was said, and how many
/// failures came since.
#[derive(Debug, Default)]
pub(in crate::server::treefs) struct FailLog {
    said: Option<u64>,
    unsaid: u64,
}

impl FailLog {
    /// Counts one more failure at `now`; whether to say it, and how many failures before it were
    /// not said, if none was said in the last [`SAY_EVERY_MS`].
    pub(in crate::server::treefs) fn failed(&mut self, now: u64) -> Option<u64> {
        if self
            .said
            .is_some_and(|said| now.saturating_sub(said) < SAY_EVERY_MS)
        {
            self.unsaid += 1;
            return None;
        }
        self.said = Some(now);
        Some(std::mem::take(&mut self.unsaid))
    }
}

/// The line said about the failed open of `failure`, in the tree `digest` numbered `number`, of
/// `size` bytes read from `source` (none: the object of the blob), after `tries` fills, with
/// `unsaid` failures of the tree since the line before.
pub(in crate::server::treefs) fn said(
    (digest, number): (&TreeDigest, u64),
    failure: &Failure,
    (size, source): (u64, Option<&BlobSource>),
    (tries, unsaid): (u32, u64),
) -> String {
    let from = source.map_or_else(
        || format!("cas/l/{}", failure.digest),
        |source| format!("{} @ {}", source.key, source.offset),
    );
    let more = match unsaid {
        0 => String::new(),
        1 => "; 1 more failed open of this tree since the last line".to_owned(),
        n => format!("; {n} more failed opens of this tree since the last line"),
    };
    format!(
        "tierstore fs: cannot open {} in the tree {digest} (number {number}): the blob {} of \
         {size} bytes from {from}: {} (after {tries} fills){more}",
        failure.path, failure.digest, failure.reason,
    )
}

impl Core {
    /// This core, opening the files of the cache with `open` (a test's).
    #[cfg(test)]
    pub(in crate::server::treefs) fn opening(mut self, open: Opener) -> Self {
        self.open_files = OpenFiles::opening(open);
        self
    }

    /// The files of the cache open for the handles now, each one descriptor.
    #[cfg(test)]
    pub(in crate::server::treefs) fn open_descriptors(&self) -> usize {
        self.open_files.len()
    }

    /// The cache file of a blob, filled if the cache does not hold it; why not, in the words of
    /// the fill socket's answer, or `io: ` and what the system said.
    pub(super) fn cache_file(
        &self,
        (digest, size, source): (&TreeDigest, u64, Option<&BlobSource>),
    ) -> Result<Arc<File>, String> {
        let sha = digest.to_string();
        let path = blob_path(&self.settings.cache_path, &sha);
        if let Ok(file) = self.open_files.get(&path) {
            return Ok(file);
        }
        let filled = match source {
            Some(source) => self.fills.fill_blob(&sha, size, &source.key, source.offset),
            None => self.fills.fill_blob(&sha, size, &format!("cas/l/{sha}"), 0),
        };
        filled.and_then(|()| self.open_files.get(&path).map_err(|error| unopened(&error)))
    }

    /// The span of the pack that a file of a placed tree is, which the node fills first if it does
    /// not hold it; why not, as [`Core::cache_file`].
    fn span_file(
        &self,
        (digest, size, source): (&TreeDigest, u64, Option<&BlobSource>),
    ) -> Result<FileBytes, String> {
        let source = source.ok_or("the file has no source")?;
        let sha = digest.to_string();
        let (base, path) = self.fills.span(&sha, size, &source.key, source.offset)?;
        let file = self
            .open_files
            .get(&path)
            .map_err(|error| unopened(&error))?;
        Ok(FileBytes { file, base, size })
    }

    /// The bytes of a file: the cache file of its blob, filled if it must be, which is pinned for
    /// the tree the first time it is opened under it (`Core::pin_once`); for a placed tree, the
    /// span of the pack, which the tree store pins. A fill that fails for a reason that can pass
    /// is asked again (module docs).
    ///
    /// # Errors
    /// [`Errno::Io`] for bytes that cannot be had: recorded on the attachments of the tree and
    /// said on stderr.
    pub(super) fn blob(
        &self,
        loaded: &Loaded,
        number: u64,
        entry: &TreeEntry,
        blob: (&TreeDigest, u64, Option<&BlobSource>),
    ) -> Result<FileBytes, Errno> {
        let mut tries = 0;
        let reason = loop {
            tries += 1;
            let got = if loaded.tree.location().is_some() {
                self.span_file(blob)
            } else {
                self.cache_file(blob).map(|file| {
                    self.pin_once((loaded, number), blob.0);
                    FileBytes::whole(file, blob.1)
                })
            };
            match got {
                Ok(bytes) => return Ok(bytes),
                Err(reason) if tries < FILL_TRIES && passes(&reason) => self.fills.rest(),
                Err(reason) => break reason,
            }
        };
        let failure = self.failed((loaded, number), entry.index, blob.0, reason);
        let unsaid = locked(&loaded.failures).failed(self.now());
        if let Some(unsaid) = unsaid {
            let line = said(
                (&loaded.digest, number),
                &failure,
                (blob.1, blob.2),
                (tries, unsaid),
            );
            tracing::warn!("{line}");
        }
        Err(Errno::Io)
    }

    /// Records that the blob of the entry `index` of the tree `number` could not be had, on every
    /// attachment of the tree; the failure.
    pub(super) fn failed(
        &self,
        (loaded, number): (&Loaded, u64),
        index: u32,
        digest: &TreeDigest,
        reason: String,
    ) -> Failure {
        let failure = Failure {
            path: path_of(&loaded.tree, index),
            digest: digest.to_string(),
            reason,
        };
        self.state().attachments.fail(number, &failure);
        failure
    }
}

/// The path of the entry `index` in its tree, from the root: `/a/b/c`.
pub(in crate::server::treefs) fn path_of(tree: &Tree, index: u32) -> String {
    let mut names = Vec::new();
    let mut at = tree.entry(index);
    while let Some(entry) = at {
        if entry.parent.is_some() {
            names.push(entry.name.to_string());
        }
        at = entry.parent.and_then(|parent| tree.entry(parent));
    }
    names.reverse();
    format!("/{}", names.join("/"))
}

#[cfg(test)]
#[path = "blob_tests.rs"]
mod tests;
