//! Whole trees written as plain files into a local directory: what the provider answers when its
//! interface gives `local_trees` (a Mac's, where a job's programs cannot read a network volume, so
//! no directory of `tierstore fs` can serve them).
//!
//! A tree is written once per tree list key, into `<local_trees>/<64 hex>`, and shared by every
//! source that names the key, now and after a restart of this process; it is never changed once
//! written, and nothing here removes it.
//!
//! - **The write.** The tree list is read through the cache and checked against its key, as the
//!   preparation of a tree of `tierstore fs` checks it (version 1 or version 2). The tree is laid
//!   out in `<local_trees>/.partial-<64 hex>`, mode 0700, so that no other account reads any of it:
//!   its directories (mode 0755), its links (as written) and then its files, at most
//!   `trees.fill_concurrency` at once across the node. Each file's bytes are read through the cache
//!   from the span the list names, hashed and written in one pass with no more of them held than
//!   one chunk of the read, then checked against the file's size and SHA-256 and synced. A file
//!   ends mode 0755 if the list gives it any execute bit and 0644 otherwise: no setuid, setgid or
//!   sticky bit, as a directory ends 0755 whatever its listed mode. Once every file is written the
//!   directories are synced, the partial directory is given mode 0755 and renamed to
//!   `<local_trees>/<64 hex>`: a reader finds the whole tree or nothing.
//! - **One writer per key.** A second ensure of a key being written joins the write in progress; an
//!   ensure of a key already written answers it at once.
//! - **A crash.** A process that ends during a write leaves its partial directory, mode 0700, which
//!   no source is answered with. The next write of the key, and the start of the provider, remove
//!   it.
//! - **Refusals.** A tree list that is missing or that the format refuses, an entry below a file or
//!   a link, two names the file system takes as one (a Mac's volume ignores case), a file whose
//!   object is missing, or whose bytes are not its size or do not hash to its digest: the ref
//!   cannot be used, `FAILED_PRECONDITION`. A read that fails or passes `fetch_timeout`, a write
//!   of the disk that fails, or a stop of the process: a failure the next `Mount` writes again
//!   from the start. Either way the partial directory is removed.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use futures::{StreamExt as _, TryStreamExt as _, stream};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

use super::prepare::{Job, read_list, report};
use super::sources::{Shared, Stage};
use crate::tree_image::ImageKind;
use crate::{
    Backend as _, ByteRange, Error, ListKind, TreeDigest, TreeImage, TreeLimits, TreeListError,
    is_tree_list_v2, parse_tree_list,
};

/// The mode of a directory of a written tree, and of a file with an execute bit.
const OPEN_MODE: u32 = 0o755;

/// The mode of a file of a written tree with no execute bit.
const FILE_MODE: u32 = 0o644;

/// The mode of a tree being written: its account's alone.
const PARTIAL_MODE: u32 = 0o700;

/// The first part of the name of a tree being written.
const PARTIAL: &str = ".partial-";

/// How a write ended, as the ensures that wait on it see it: none while it runs.
type Outcome = Option<Result<(), Stage>>;

/// The directory of whole trees, and the writes in progress.
pub(super) struct LocalTrees {
    root: PathBuf,
    /// The writes in progress, by the hex of their key.
    writes: Mutex<BTreeMap<String, watch::Receiver<Outcome>>>,
    /// How many writes found their tree not yet written and wrote it (or tried).
    #[cfg(test)]
    pub(super) written: std::sync::atomic::AtomicU64,
}

impl LocalTrees {
    /// The trees under `root`, with the partial directories a write of a process before this one
    /// left removed. A partial directory that cannot be removed is said in the log, and the next
    /// write of its key removes it or fails saying why.
    pub(super) fn open(root: PathBuf) -> Self {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(PARTIAL)
                    && let Err(why) = std::fs::remove_dir_all(entry.path())
                {
                    tracing::warn!(
                        "tierstore: mount provider: {} was not removed: {why}",
                        entry.path().display()
                    );
                }
            }
        }
        Self {
            root,
            writes: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            written: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The directory of whole trees.
    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// The directory of the tree of `digest`, whole once it is there.
    pub(super) fn path_of(&self, digest: &TreeDigest) -> PathBuf {
        self.root.join(digest.to_string())
    }

    /// The directory a write of the tree of `digest` lays it out in.
    fn partial_of(&self, digest: &TreeDigest) -> PathBuf {
        self.root.join(format!("{PARTIAL}{digest}"))
    }
}

/// Prepares the source of `job` by writing its tree, or joining the write of it, and reports the
/// stage that leaves it at: placed once the tree is whole.
pub(super) async fn run(shared: Arc<Shared>, local: Arc<LocalTrees>, job: Job) {
    let written = ensure(&shared, &local, &job.key, job.digest).await;
    match written {
        Ok(path) => {
            tracing::info!(
                "tierstore: mount provider: {} of {}: whole at {}",
                job.id.path(),
                job.key,
                path.display()
            );
            report(&job.stage, Stage::Placed);
        }
        Err(stage) => {
            tracing::warn!(
                "tierstore: mount provider: {} of {} not written: {stage:?}",
                job.id.path(),
                job.key
            );
            report(&job.stage, stage);
        }
    }
}

/// The directory of the whole tree of the tree list `key`, whose digest is `digest`: at once if it
/// is written, after the write in progress if there is one, or after a write begun here. The write
/// runs on a task of its own, so that it goes on when the ensures waiting on it go.
///
/// # Errors
/// The stage a refusal or a failure of the write leaves a source at.
pub(super) async fn ensure(
    shared: &Arc<Shared>,
    local: &Arc<LocalTrees>,
    key: &str,
    digest: TreeDigest,
) -> Result<PathBuf, Stage> {
    let hex = digest.to_string();
    let mut outcome = {
        let mut writes = local.writes.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(running) = writes.get(&hex) {
            running.clone()
        } else {
            let (sender, outcome) = watch::channel(None);
            writes.insert(hex.clone(), outcome.clone());
            let (shared, local, key) = (Arc::clone(shared), Arc::clone(local), key.to_owned());
            tokio::spawn(async move {
                // Forgets the write however it ends, a panic included, so that the next ensure
                // of the key begins another.
                let _forget = Forget {
                    local: Arc::clone(&local),
                    hex,
                };
                let ended = write(&shared, &local, &key, digest).await;
                sender.send_replace(Some(ended));
            });
            outcome
        }
    };
    // The sender goes only after it sent the end, or with a write that panicked.
    let ended = outcome
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|ended| ended.clone())
        .unwrap_or_else(|| {
            Err(Stage::Failed(
                "the write of the tree ended early".to_owned(),
            ))
        });
    ended.map(|()| local.path_of(&digest))
}

/// Removes a write from the writes in progress when it is dropped.
struct Forget {
    local: Arc<LocalTrees>,
    hex: String,
}

impl Drop for Forget {
    fn drop(&mut self) {
        let mut writes = self
            .local
            .writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        writes.remove(&self.hex);
    }
}

/// What a tree holds at one path below its root.
#[derive(Debug)]
pub(super) enum Entry {
    Dir,
    File(FileEntry),
    Link(String),
}

/// A file of a tree: where its bytes are and what they must be.
#[derive(Clone, Debug)]
pub(super) struct FileEntry {
    path: String,
    /// Whether the list gives it an execute bit.
    exec: bool,
    size: u64,
    sha256: Option<TreeDigest>,
    /// The key of the object its bytes are in and their offset there; none for no bytes.
    source: Option<(Arc<str>, u64)>,
}

/// Writes the tree of `key` whole, unless it is written: the partial directory laid out, the files
/// written, the whole renamed. A refusal or a failure removes the partial directory.
pub(super) async fn write(
    shared: &Shared,
    local: &LocalTrees,
    key: &str,
    digest: TreeDigest,
) -> Result<(), Stage> {
    let (whole, partial) = (local.path_of(&digest), local.partial_of(&digest));
    if tokio::fs::metadata(&whole)
        .await
        .is_ok_and(|meta| meta.is_dir())
    {
        return Ok(());
    }
    #[cfg(test)]
    local
        .written
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let bytes = read_list(shared, key).await?;
    let limits = shared.store.config().limits();
    let laid = {
        let partial = partial.clone();
        let root = local.root().to_path_buf();
        tokio::task::spawn_blocking(move || {
            let entries = entries_of(bytes, &digest, &limits)?;
            lay_out(&root, &partial, entries)
        })
        .await
        .map_err(|why| Stage::Failed(format!("the tree was not laid out: {why}")))
    };
    let written = match laid {
        Ok(Ok(files)) => write_files(shared, &partial, files).await,
        Ok(Err(stage)) | Err(stage) => Err(stage),
    };
    let ended = match written {
        Ok(()) => {
            let (partial, whole) = (partial.clone(), whole.clone());
            tokio::task::spawn_blocking(move || finish(&partial, &whole))
                .await
                .unwrap_or_else(|why| Err(io::Error::other(why.to_string())))
                .map_err(|why| Stage::Failed(format!("the tree was not made whole: {why}")))
        }
        Err(stage) => Err(stage),
    };
    if ended.is_err() {
        let _ = tokio::fs::remove_dir_all(&partial).await;
    }
    ended
}

/// The entries of the tree list in `bytes`, which must hash to `digest`, by path from the root,
/// each directory before what it holds.
///
/// # Errors
/// [`Stage::Refused`] with what the format refuses.
pub(super) fn entries_of(
    bytes: Vec<u8>,
    digest: &TreeDigest,
    limits: &TreeLimits,
) -> Result<Vec<(String, Entry)>, Stage> {
    let refused = |why: &dyn std::fmt::Display| Stage::Refused(why.to_string());
    if !is_tree_list_v2(&bytes) {
        let list = parse_tree_list(&bytes, digest).map_err(|why| refused(&why))?;
        let entries = list.entries.into_iter().map(|entry| {
            let kind = match entry.kind {
                ListKind::Dir { .. } => Entry::Dir,
                ListKind::Symlink { target } => Entry::Link(target),
                ListKind::File {
                    mode,
                    size,
                    source,
                    sha256,
                } => Entry::File(FileEntry {
                    path: entry.path.clone(),
                    exec: mode.bits() & 0o111 != 0,
                    size,
                    sha256,
                    source: source.map(|source| (source.key, source.offset)),
                }),
            };
            (entry.path, kind)
        });
        return Ok(entries.collect());
    }
    let actual = TreeDigest::of(&bytes);
    if actual != *digest {
        let why = TreeListError::DigestMismatch {
            expected: *digest,
            actual,
        };
        return Err(refused(&why));
    }
    let image = TreeImage::open(bytes.into(), limits, |_| {}).map_err(|why| refused(&why))?;
    // In the image's order every directory comes before what it holds, so a parent's path is
    // known when its child is met. The root, index 0, is the partial directory itself.
    let mut paths: Vec<String> = vec![String::new()];
    let mut entries = Vec::new();
    for entry in (1..).map_while(|at| image.entry(at)) {
        let parent = entry.parent.and_then(|parent| paths.get(parent as usize));
        let path = match parent.map(String::as_str) {
            Some("") | None => entry.name.to_owned(),
            Some(parent) => format!("{parent}/{}", entry.name),
        };
        let kind = match entry.kind {
            ImageKind::Dir { .. } => Entry::Dir,
            ImageKind::Symlink { target } => Entry::Link(target.to_owned()),
            ImageKind::File {
                mode,
                size,
                sha256,
                span,
            } => Entry::File(FileEntry {
                path: path.clone(),
                exec: mode.bits() & 0o111 != 0,
                size,
                sha256: Some(sha256),
                source: span.map(|(key, offset)| (Arc::clone(key), offset)),
            }),
        };
        paths.push(path.clone());
        entries.push((path, kind));
    }
    Ok(entries)
}

/// Makes `partial` afresh under `root` (removing what a write before left there), mode 0700, and
/// in it every directory and link of `entries`; the files still to write.
///
/// # Errors
/// [`Stage::Refused`] for an entry below a file or a link, or a name the file system already has;
/// [`Stage::Failed`] for a directory that cannot be made.
fn lay_out(
    root: &Path,
    partial: &Path,
    entries: Vec<(String, Entry)>,
) -> Result<Vec<FileEntry>, Stage> {
    let failed = |path: &Path, what: &str, why: io::Error| {
        Stage::Failed(format!("{} {what}: {why}", path.display()))
    };
    std::fs::create_dir_all(root).map_err(|why| failed(root, "cannot be made", why))?;
    match std::fs::remove_dir_all(partial) {
        Err(why) if why.kind() != io::ErrorKind::NotFound => {
            return Err(failed(
                partial,
                "is left by a write before and cannot be removed",
                why,
            ));
        }
        _ => {}
    }
    make_dir(partial, PARTIAL_MODE).map_err(|why| failed(partial, "cannot be made", why))?;
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut others: BTreeSet<String> = BTreeSet::new();
    let mut files = Vec::new();
    for (path, entry) in entries {
        // Every directory above the entry is one this write made: a path never goes through a
        // file or a link, so nothing is written outside the partial directory.
        let mut above = 0;
        while let Some(at) = path[above..].find('/') {
            above += at;
            let parent = &path[..above];
            if others.contains(parent) {
                return Err(Stage::Refused(format!(
                    "{path:?} is below {parent:?}, which is not a directory"
                )));
            }
            if dirs.insert(parent.to_owned()) {
                make_dir(&partial.join(parent), OPEN_MODE).map_err(|why| made(&path, &why))?;
            }
            above += 1;
        }
        match entry {
            // A directory is listed before what it holds, so it is not made yet.
            Entry::Dir => {
                make_dir(&partial.join(&path), OPEN_MODE).map_err(|why| made(&path, &why))?;
                dirs.insert(path);
            }
            Entry::Link(target) => {
                std::os::unix::fs::symlink(&target, partial.join(&path))
                    .map_err(|why| made(&path, &why))?;
                others.insert(path);
            }
            Entry::File(file) => {
                others.insert(path);
                files.push(file);
            }
        }
    }
    Ok(files)
}

/// Makes the directory `path` with exactly `mode`, whatever the process's umask.
fn make_dir(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::create_dir(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// The stage an entry at `path` that could not be made leaves a source at: a name the file system
/// already has (two names of the list it takes as one, as a volume that ignores case does) is a
/// refusal, anything else a failure.
pub(super) fn made(path: &str, why: &io::Error) -> Stage {
    if why.kind() == io::ErrorKind::AlreadyExists {
        return Stage::Refused(format!(
            "{path:?} is a name this file system already has: the tree has two names it takes as one"
        ));
    }
    Stage::Failed(format!("{path:?} cannot be made: {why}"))
}

/// Writes `files` under `partial`, `trees.fill_concurrency` at once across the node; the first
/// refusal or failure ends the others.
async fn write_files(shared: &Shared, partial: &Path, files: Vec<FileEntry>) -> Result<(), Stage> {
    let at_once = shared.store.config().fill_concurrency;
    stream::iter(files)
        .map(|file| async move { write_file(shared, partial, &file).await })
        .buffer_unordered(at_once)
        .try_collect::<()>()
        .await
}

/// Writes one file, once a read of the node's is free, within `fetch_timeout`.
async fn write_file(shared: &Shared, partial: &Path, file: &FileEntry) -> Result<(), Stage> {
    let Ok(_turn) = shared.fills.acquire().await else {
        return Err(Stage::Failed("the provider is stopping".to_owned()));
    };
    let bound = shared.settings.fill_timeout;
    tokio::time::timeout(bound, copy(shared, partial, file))
        .await
        .unwrap_or_else(|_| {
            Err(Stage::Failed(format!(
                "{:?} was not read within {bound:?}",
                file.path
            )))
        })
}

/// Reads the bytes of `file` through the cache, hashing and writing each chunk as it comes, checks
/// them, gives the file its mode and syncs it.
async fn copy(shared: &Shared, partial: &Path, file: &FileEntry) -> Result<(), Stage> {
    let path = &file.path;
    let failed = |why: io::Error| Stage::Failed(format!("{path:?} was not written: {why}"));
    let mut out = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(partial.join(path))
        .await
        .map_err(|why| made(path, &why))?;
    let mut hasher = Sha256::new();
    let mut got: u64 = 0;
    if let Some((key, offset)) = &file.source {
        let range = ByteRange::new(*offset, file.size);
        let unread = |why: Error| match why {
            Error::NotFound(_) => Stage::Refused(format!("{path:?}: no object {key}")),
            why => Stage::Failed(format!("{path:?} was not read: {why}")),
        };
        let mut body = shared
            .cache
            .get_stream(key, Some(range))
            .await
            .map_err(unread)?;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(unread)?;
            got += chunk.len() as u64;
            hasher.update(&chunk);
            out.write_all(&chunk).await.map_err(failed)?;
        }
    }
    // A write of tokio's file is handed to a thread; its failure is reported by the flush.
    out.flush().await.map_err(failed)?;
    if got != file.size {
        return Err(Stage::Refused(format!(
            "{path:?} is {} bytes in the list, and {got} were read",
            file.size
        )));
    }
    let actual = TreeDigest::from_bytes(hasher.finalize().into());
    if file.sha256.is_some_and(|sha256| sha256 != actual) {
        return Err(Stage::Refused(format!(
            "{path:?}: the bytes hash to {actual}, not to the list's digest"
        )));
    }
    let mode = if file.exec { OPEN_MODE } else { FILE_MODE };
    out.set_permissions(std::fs::Permissions::from_mode(mode))
        .await
        .map_err(failed)?;
    out.sync_all().await.map_err(failed)
}

/// Syncs every directory of the tree at `partial` (the files are synced as they are written), gives
/// it mode 0755 and renames it to `whole`. A tree another process made whole first is kept, and
/// `partial` removed.
pub(super) fn finish(partial: &Path, whole: &Path) -> io::Result<()> {
    sync_dirs(partial)?;
    std::fs::set_permissions(partial, std::fs::Permissions::from_mode(OPEN_MODE))?;
    if let Err(why) = std::fs::rename(partial, whole) {
        if !whole.is_dir() {
            return Err(why);
        }
        std::fs::remove_dir_all(partial)?;
    }
    std::fs::File::open(whole.parent().unwrap_or(whole))?.sync_all()
}

/// Syncs the directory `dir` and every directory below it, so that their entries outlast a crash
/// of the host once the tree is renamed.
fn sync_dirs(dir: &Path) -> io::Result<()> {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
        std::fs::File::open(&dir)?.sync_all()?;
    }
    Ok(())
}

/// The line the log has at the start of a provider that writes whole trees into `root`.
pub(super) fn mode_line(root: &Path) -> String {
    format!(
        "tierstore: mount provider: local_trees = {}: Prepare writes each tree whole there, and Mount answers it once it is whole",
        root.display()
    )
}
