//! The put directory as a namespace of inode numbers, `<mountpoint>/put/`: what a frontend answers
//! for each request below it, from the staged-file store, with no protocol in it. The FUSE frontend
//! (`fuse_put.rs`) decodes the kernel's requests into these calls; a test makes them directly.
//!
//! **Directories.** No directory is made or kept: a path is one if it leads to an allowed prefix
//! (`put/`, `put/cas` for the prefix `cas/b/`: mode 0711, owned by root, never listed) or is at or
//! under one (mode 0700, owned by `fs.owner`, listed with its files alone, never its directories).
//! A name with nothing behind it is a directory or nothing, so that the directory a job is given
//! (`put/cas/b/<build>/<nonce>`) exists before anything is in it, and a name a job will create or
//! commit to is not there:
//!
//! 1. a staged or committed name of the store is that file;
//! 2. else a `.part` name is not there;
//! 3. else a path that holds a name of the store below it, or that `mkdir` made and the kernel has
//!    not forgotten, is a directory;
//! 4. else in a directory that holds a file, the name is not there (it is the name of a commit);
//! 5. else, at or under a prefix, the name is a directory.
//!
//! Every answer has a timeout of zero, so the kernel asks again at each use and the answer follows
//! the store. A directory's inode number is a hash of its path (`ino.rs`), and the paths of those
//! the kernel holds are kept until it forgets them; a file's is its number in the stage, so a
//! committed name keeps the number its `.part` name had, as the kernel's rename expects.
//!
//! **Files.** A create makes a `.part` file in a directory at or under a prefix and opens it to
//! write; the handle's number says the process that made it (`handle.rs`), so a write or a release
//! that names the handle of an earlier process is taken over (`adopt`). A file is never opened
//! again: a staged one is `EPERM`, a committed one `EACCES`, and a read `EACCES`. `setattr` is a
//! change of nothing (the size it has, the mode it has), else `EPERM`, so the set-id bits are never
//! set. The commit is the rename of a `.part` name to a final name in the same directory: it waits,
//! until `fs.put.commit_deadline_secs`, while the file is busy for a while only (a handle whose
//! descriptors were all closed and flushed and whose release has not come yet, since FUSE sends it
//! after the close returns, or a commit of the file or of the key running), and is `EBUSY` at once
//! for a handle still open; then it uploads the file ([`super::upload`]) and
//! records the answer. Errors are the errnos of `super::error`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak, mpsc};
use std::time::Duration;

use rustix::io::Errno;
use sha2::{Digest as _, Sha256};

use super::carry::{Handed, HeldDir};
use super::commit::HANDLE_OPEN;
use super::error::PutError;
use super::names::{PutDir, check_component, is_part};
use super::store::{Handle, Kind, Rename, StagedStore};
use super::upload::Uploader;
use crate::Clock;
use crate::server::local_cache::Space;
use crate::server::treefs::core::FIXED_MTIME;
use crate::server::treefs::ino::{self, PutIno};
use crate::server::treefs::{handle, locked};

/// The mode of `put/` and of the directories that lead to a prefix: anyone may pass, nobody lists.
const LEADING_MODE: u16 = 0o711;

/// The mode of a directory at or under a prefix.
const DIR_MODE: u16 = 0o700;

/// The mode of a staged file.
const STAGED_MODE: u16 = 0o600;

/// The mode of a committed name: shown, never read.
const COMMITTED_MODE: u16 = 0o400;

/// How often the sweep of the store runs.
pub(in crate::server) const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// The attributes of an inode of the put directory, as a frontend tells them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) struct PutAttr {
    pub ino: u64,
    /// A directory, or a file.
    pub dir: bool,
    pub size: u64,
    /// The permission bits.
    pub perm: u16,
    pub uid: u32,
    pub gid: u32,
    /// The modification time, in milliseconds since the epoch.
    pub ms: u64,
}

/// A directory the kernel was told of.
#[derive(Debug)]
struct Dir {
    path: String,
    /// The lookups the kernel holds; none when they are not known (a start after a crash), and then
    /// the directory is kept.
    lookups: Option<u64>,
    /// Whether `mkdir` made it.
    made: bool,
}

/// What the lock `state` guards.
#[derive(Debug, Default)]
struct State {
    /// The directories the kernel was told of, by inode number.
    dirs: BTreeMap<u64, Dir>,
    /// The store's handle of each open the kernel holds, by the number the kernel was given.
    handles: BTreeMap<u64, Handle>,
    /// The opens the kernel has flushed (each `close` of a descriptor flushes its open) and not
    /// written through since: opens that are being closed, whose release may still come.
    flushed: BTreeSet<u64>,
    /// The count of the opens this process gave out.
    opens: u64,
}

/// An entry of a directory, for a listing.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::server) struct PutItem {
    pub name: String,
    pub attr: PutAttr,
}

/// The put directory.
pub(in crate::server) struct PutSpace {
    store: StagedStore,
    uploader: Uploader,
    /// The user and group of the directories at or under a prefix and of the files (`fs.owner`).
    owner: (u32, u32),
    /// The generation of this process, in the numbers of the handles it gives out.
    generation: u32,
    /// How long a commit may take.
    deadline: Duration,
    /// The time the deadlines are told by.
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    /// A count of the changes a waiting commit looks for: a release, a commit ended, an unlink.
    changes: Mutex<u64>,
    changed: Condvar,
    /// Whether a successor is taking over: the sweep does nothing.
    held: AtomicBool,
    /// Held by a sweep for the whole of it, and by a hold while it is set, so that no sweep that
    /// began before a hold goes on after it.
    sweeping: Mutex<()>,
    /// The end of the channel the sweep's thread waits on; dropped with the space, which ends it.
    sweeper: Mutex<Option<mpsc::Sender<()>>>,
    /// The waits for a change begun, for the tests that act once a commit waits.
    #[cfg(test)]
    pub(super) waits: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for PutSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PutSpace")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// `dir/name`, or `name` below `put/` itself.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// The directory above `path`, a directory below `put/` (`put/` itself is the empty path).
fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// The inode number of the directory `path` (`put/` for the empty path).
pub(in crate::server) fn dir_ino(path: &str) -> u64 {
    if path.is_empty() {
        return ino::PUT;
    }
    let digest = Sha256::digest(path.as_bytes());
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    ino::put_dir(u64::from_le_bytes(first))
}

/// The errno of a refusal of the store.
fn errno(error: &PutError) -> Errno {
    error.errno()
}

impl PutSpace {
    /// A put directory over `store`, uploading through `uploader`, for the process of
    /// `generation`.
    pub(in crate::server) fn new(
        store: StagedStore,
        uploader: Uploader,
        (owner, generation): ((u32, u32), u32),
        deadline: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            uploader,
            owner,
            generation,
            deadline,
            clock,
            state: Mutex::new(State::default()),
            changes: Mutex::new(0),
            changed: Condvar::new(),
            held: AtomicBool::new(false),
            sweeping: Mutex::new(()),
            sweeper: Mutex::new(None),
            #[cfg(test)]
            waits: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The store.
    pub(in crate::server) fn store(&self) -> &StagedStore {
        &self.store
    }

    /// Whether `path` is a directory at or under one of the prefixes.
    fn under(&self, path: &str) -> Option<PutDir> {
        PutDir::new(path, &self.store.settings.prefixes).ok()
    }

    /// The directory `path` of the store, where files may be made: at or under a prefix.
    ///
    /// # Errors
    /// `EPERM` for `put/` and every other directory that is not at or under a prefix.
    fn writable(&self, path: &str) -> Result<PutDir, Errno> {
        self.under(path).ok_or(Errno::PERM)
    }

    /// Whether the directory `path` below `put/` leads to a prefix and is not at or under one:
    /// `cas` for `cas/b/`.
    fn leading(&self, path: &str) -> bool {
        let below = format!("{path}/");
        self.under(path).is_none()
            && self
                .store
                .settings
                .prefixes
                .iter()
                .any(|prefix| prefix.starts_with(&below))
    }

    /// The attributes of the directory `path`.
    fn dir_attr(&self, path: &str) -> PutAttr {
        let (perm, (uid, gid)) = if self.under(path).is_some() {
            (DIR_MODE, self.owner)
        } else {
            (LEADING_MODE, (0, 0))
        };
        PutAttr {
            ino: dir_ino(path),
            dir: true,
            size: 0,
            perm,
            uid,
            gid,
            ms: FIXED_MTIME * 1000,
        }
    }

    /// The attributes of the file numbered `id`, of `size` bytes, made or committed at `ms`.
    fn file_attr(&self, id: u64, attr: super::store::Attr) -> PutAttr {
        let perm = match attr.kind {
            Kind::Staged => STAGED_MODE,
            Kind::Committed(_) => COMMITTED_MODE,
        };
        PutAttr {
            ino: ino::put_file(id),
            dir: false,
            size: attr.size,
            perm,
            uid: self.owner.0,
            gid: self.owner.1,
            ms: attr.ms,
        }
    }

    /// The path of the directory `ino`.
    ///
    /// # Errors
    /// `ESTALE` for a number this process does not know (a directory a process before this one
    /// was asked for, after a start that had no hand-over), `ENOTDIR` for a file.
    fn dir_path(&self, ino: u64) -> Result<String, Errno> {
        match ino::put_of(ino) {
            Some(PutIno::Root) => Ok(String::new()),
            Some(PutIno::Dir) => locked(&self.state)
                .dirs
                .get(&ino)
                .map(|dir| dir.path.clone())
                .ok_or(Errno::STALE),
            Some(PutIno::File(_)) => Err(Errno::NOTDIR),
            None => Err(Errno::STALE),
        }
    }

    /// Tells of the directory `path` below `put/`, counting one lookup of it the kernel holds (or
    /// `made` by `mkdir`), and gives its attributes.
    ///
    /// # Errors
    /// `EIO` when another directory the kernel holds has the same inode number.
    fn tell_dir(&self, path: &str, made: bool) -> Result<PutAttr, Errno> {
        let attr = self.dir_attr(path);
        let mut state = locked(&self.state);
        let dir = state.dirs.entry(attr.ino).or_insert_with(|| Dir {
            path: path.to_owned(),
            lookups: Some(0),
            made,
        });
        if dir.path != path {
            tracing::warn!(
                "tierstore fs: the put directories {path} and {} have one inode number",
                dir.path
            );
            return Err(Errno::IO);
        }
        dir.made |= made;
        if let Some(lookups) = dir.lookups.as_mut() {
            *lookups += 1;
        }
        Ok(attr)
    }

    /// Whether `mkdir` made `path` and the kernel has not forgotten it.
    fn made(&self, path: &str) -> bool {
        locked(&self.state)
            .dirs
            .get(&dir_ino(path))
            .is_some_and(|dir| dir.made && dir.path == path)
    }

    /// The name `name` of the directory `parent`: its attributes, or none when it is not there
    /// (see the module for the rules). A directory's lookup is counted until it is forgotten.
    ///
    /// # Errors
    /// `ESTALE` or `ENOTDIR` for a parent that is not a known directory, `EINVAL` or
    /// `ENAMETOOLONG` for a name that is not one, `EIO` for two directories of one number.
    pub(in crate::server) fn lookup(
        &self,
        parent: u64,
        name: &str,
    ) -> Result<Option<PutAttr>, Errno> {
        let parent = self.dir_path(parent)?;
        check_component(name).map_err(|error| errno(&error))?;
        let child = join(&parent, name);
        let Some(dir) = self.under(&parent) else {
            if self.leading(&child) || self.under(&child).is_some() {
                return self.tell_dir(&child, false).map(Some);
            }
            return Ok(None);
        };
        if let Some((id, attr)) = self.store.find(&dir, name) {
            return Ok(Some(self.file_attr(id, attr)));
        }
        if is_part(name) {
            return Ok(None);
        }
        let known = self.store.holds_under(&child) || self.made(&child);
        if known || (!self.store.holds(&parent) && self.under(&child).is_some()) {
            return self.tell_dir(&child, false).map(Some);
        }
        Ok(None)
    }

    /// The kernel forgets `nlookup` lookups of `ino`; a directory it no longer holds is let go.
    pub(in crate::server) fn forget(&self, ino: u64, nlookup: u64) {
        let mut state = locked(&self.state);
        let Some(dir) = state.dirs.get_mut(&ino) else {
            return;
        };
        if let Some(lookups) = dir.lookups.as_mut() {
            *lookups = lookups.saturating_sub(nlookup);
            if *lookups == 0 {
                let _ = state.dirs.remove(&ino);
            }
        }
    }

    /// The attributes of `ino`.
    ///
    /// # Errors
    /// `ESTALE` for a directory this process does not know, `ENOENT` for a file that is gone.
    pub(in crate::server) fn getattr(&self, ino: u64) -> Result<PutAttr, Errno> {
        if let Some(PutIno::File(id)) = ino::put_of(ino) {
            let (_, _, attr) = self.store.by_id(id).ok_or(Errno::NOENT)?;
            return Ok(self.file_attr(id, attr));
        }
        let path = self.dir_path(ino)?;
        Ok(self.dir_attr(&path))
    }

    /// A change of the attributes of `ino`: taken only when it changes nothing (the size the file
    /// has, the permission bits it has), as the kernel asks at a create with `O_TRUNC` or to clear
    /// set-id bits it does not have.
    ///
    /// # Errors
    /// `EPERM` for any change, and those of [`PutSpace::getattr`].
    pub(in crate::server) fn setattr(
        &self,
        ino: u64,
        (mode, size): (Option<u32>, Option<u64>),
        others: bool,
    ) -> Result<PutAttr, Errno> {
        let attr = self.getattr(ino)?;
        let same_mode = mode.is_none_or(|mode| mode & 0o7777 == u32::from(attr.perm));
        let same_size = size.is_none_or(|size| !attr.dir && size == attr.size);
        if others || !same_mode || !same_size {
            return Err(Errno::PERM);
        }
        Ok(attr)
    }

    /// Opens the directory `ino` for listing.
    ///
    /// # Errors
    /// `EACCES` for `put/` and the directories that lead to a prefix, and those of
    /// [`PutSpace::getattr`].
    pub(in crate::server) fn opendir(&self, ino: u64) -> Result<(), Errno> {
        let path = self.dir_path(ino)?;
        if self.under(&path).is_none() {
            return Err(Errno::ACCESS);
        }
        Ok(())
    }

    /// Gives the entries of the directory `ino` from position `offset` (0 is the start) to `sink`,
    /// with the position after each, until it answers false: `.`, `..`, and the files, staged and
    /// committed, in the order of their names; never a directory.
    ///
    /// # Errors
    /// As [`PutSpace::opendir`].
    pub(in crate::server) fn readdir(
        &self,
        ino: u64,
        offset: u64,
        sink: &mut dyn FnMut(u64, PutItem) -> bool,
    ) -> Result<(), Errno> {
        self.opendir(ino)?;
        let path = self.dir_path(ino)?;
        let dir = self.under(&path).ok_or(Errno::ACCESS)?;
        let parent = parent_of(&path);
        let mut items = vec![
            PutItem {
                name: ".".to_owned(),
                attr: self.dir_attr(&path),
            },
            PutItem {
                name: "..".to_owned(),
                attr: self.dir_attr(parent),
            },
        ];
        for (name, id, attr) in self.store.list_numbered(&dir) {
            items.push(PutItem {
                name,
                attr: self.file_attr(id, attr),
            });
        }
        let first = usize::try_from(offset).unwrap_or(usize::MAX);
        for (position, item) in items.into_iter().enumerate().skip(first) {
            if !sink(position as u64 + 1, item) {
                break;
            }
        }
        Ok(())
    }

    /// Makes the directory `name` in `parent`: a directory that the kernel is told of until it
    /// forgets it, at or under a prefix.
    ///
    /// # Errors
    /// `EPERM` for a directory that is not at or under a prefix, or a `.part` name; `EEXIST` for a
    /// name that is a file or a directory with names below it; those of [`PutSpace::lookup`].
    pub(in crate::server) fn mkdir(&self, parent: u64, name: &str) -> Result<PutAttr, Errno> {
        let parent = self.dir_path(parent)?;
        check_component(name).map_err(|error| errno(&error))?;
        if is_part(name) {
            return Err(Errno::PERM);
        }
        let child = join(&parent, name);
        PutDir::new(&child, &self.store.settings.prefixes).map_err(|error| errno(&error))?;
        let file = self
            .under(&parent)
            .and_then(|dir| self.store.find(&dir, name));
        if file.is_some() || self.store.holds_under(&child) || self.made(&child) {
            return Err(Errno::EXIST);
        }
        self.tell_dir(&child, true)
    }

    /// Creates the `.part` file `name` in `parent` and opens it to write: its attributes and the
    /// number of the open.
    ///
    /// # Errors
    /// `EPERM` for a directory that is not at or under a prefix, or a name that is not `.part`;
    /// the errnos of the store's create.
    pub(in crate::server) fn create(
        &self,
        parent: u64,
        name: &str,
    ) -> Result<(PutAttr, u64), Errno> {
        let parent = self.dir_path(parent)?;
        let dir = self.writable(&parent)?;
        let handle = self.store.create(&dir, name).map_err(|e| errno(&e))?;
        let (id, attr) = self.store.find(&dir, name).ok_or(Errno::NOENT)?;
        let mut state = locked(&self.state);
        state.opens += 1;
        let fh = handle::make(self.generation, state.opens, false);
        let _ = state.handles.insert(fh, handle);
        Ok((self.file_attr(id, attr), fh))
    }

    /// The open of `ino` that is not a create.
    ///
    /// # Errors
    /// `EPERM` for a staged file (only its create opens it), `EACCES` for a committed name,
    /// `EISDIR` for a directory, `ENOENT` for a file that is gone.
    pub(in crate::server) fn open(&self, ino: u64) -> Result<(), Errno> {
        let Some(PutIno::File(id)) = ino::put_of(ino) else {
            return Err(Errno::ISDIR);
        };
        let (_, _, attr) = self.store.by_id(id).ok_or(Errno::NOENT)?;
        match attr.kind {
            Kind::Staged => Err(Errno::PERM),
            Kind::Committed(_) => Err(Errno::ACCESS),
        }
    }

    /// The store's handle of the open `fh` of `ino`: one this process gave out, or one an earlier
    /// process gave out, taken over at its first request.
    fn handle_of(&self, ino: u64, fh: u64) -> Result<Handle, Errno> {
        let mut state = locked(&self.state);
        if let Some(handle) = state.handles.get(&fh) {
            return Ok(*handle);
        }
        let Some(PutIno::File(id)) = ino::put_of(ino) else {
            return Err(Errno::BADF);
        };
        if handle::generation_of(fh) == self.generation {
            return Err(Errno::BADF);
        }
        let handle = self.store.adopt(id).map_err(|error| errno(&error))?;
        let _ = state.handles.insert(fh, handle);
        Ok(handle)
    }

    /// Writes `bytes` at `offset` through the open `fh` of `ino`, and gives the bytes written.
    ///
    /// # Errors
    /// `EBADF` for an open that is not one, and the errnos of the store's write.
    pub(in crate::server) fn write(
        &self,
        (ino, fh): (u64, u64),
        offset: u64,
        bytes: &[u8],
    ) -> Result<usize, Errno> {
        let handle = self.handle_of(ino, fh)?;
        let _ = locked(&self.state).flushed.remove(&fh);
        self.store
            .write(handle, offset, bytes)
            .map_err(|error| errno(&error))
    }

    /// The kernel has closed the open `fh`: a commit waiting for the file goes on. An open this
    /// process does not know is one an earlier process gave out and nobody wrote through since.
    pub(in crate::server) fn release(&self, fh: u64) {
        let handle = {
            let mut state = locked(&self.state);
            let _ = state.flushed.remove(&fh);
            state.handles.remove(&fh)
        };
        if let Some(handle) = handle {
            self.store.release(handle);
        }
        self.note_change();
    }

    /// The kernel flushed the open `fh`: a descriptor of it was closed. Its release follows if it
    /// was the last.
    pub(in crate::server) fn flush(&self, fh: u64) {
        let mut state = locked(&self.state);
        if state.handles.contains_key(&fh) {
            let _ = state.flushed.insert(fh);
        }
    }

    /// Whether every open of the staged file numbered `id` has been flushed since its last write:
    /// the file is being closed, and its releases are on their way.
    fn closing(&self, id: u64) -> bool {
        let of_handle: BTreeMap<u64, u64> = self
            .store
            .open_handles()
            .into_iter()
            .map(|(handle, file)| (handle.0, file))
            .collect();
        let state = locked(&self.state);
        state
            .handles
            .iter()
            .filter(|(_, handle)| of_handle.get(&handle.0) == Some(&id))
            .all(|(fh, _)| state.flushed.contains(fh))
    }

    /// Removes the `.part` file `name` of `parent` (the abort).
    ///
    /// # Errors
    /// `EPERM` for a name that is not `.part` or a directory that is not at or under a prefix, and
    /// the errnos of the store's unlink.
    pub(in crate::server) fn unlink(&self, parent: u64, name: &str) -> Result<(), Errno> {
        let parent = self.dir_path(parent)?;
        let dir = self.writable(&parent)?;
        let removed = self.store.unlink(&dir, name).map_err(|error| errno(&error));
        self.note_change();
        removed
    }

    /// Renames `name` of `parent` to `new` of `new_parent`: a `.part` name to another, a change of
    /// the name; to a final name, the commit (see the module). `noreplace` refuses a `new` that is
    /// there (`RENAME_NOREPLACE`).
    ///
    /// # Errors
    /// `EPERM` for two directories or one not at or under a prefix, `EEXIST` for a `new` that is
    /// there with `noreplace`, `EBUSY` when the file is still busy at the deadline, `EIO` when the
    /// upload did not finish by it, and the errnos of the store's rename and of the upload's answer.
    pub(in crate::server) fn rename(
        &self,
        (parent, name): (u64, &str),
        (new_parent, new): (u64, &str),
        noreplace: bool,
    ) -> Result<(), Errno> {
        if parent != new_parent {
            return Err(Errno::PERM);
        }
        let parent = self.dir_path(parent)?;
        let dir = self.writable(&parent)?;
        if noreplace && self.store.find(&dir, new).is_some() {
            return Err(Errno::EXIST);
        }
        let deadline = self.clock.now().saturating_add(millis(self.deadline));
        let commit = loop {
            let seen = *locked(&self.changes);
            match self.store.rename(&dir, name, new) {
                Ok(Rename::Renamed) => return Ok(()),
                Ok(Rename::Commit(commit)) => break commit,
                Err(PutError::Busy(why)) => {
                    // A file still open is not waited for: only one being closed, whose release
                    // FUSE sends after the close returns, or another commit of the file or key.
                    let open = why == HANDLE_OPEN
                        && !self
                            .store
                            .find(&dir, name)
                            .is_some_and(|(id, _)| self.closing(id));
                    if open || !self.wait_for_change(seen, deadline) {
                        tracing::warn!("tierstore fs: the commit of {parent}/{new}: {why}");
                        return Err(Errno::BUSY);
                    }
                }
                Err(error) => return Err(errno(&error)),
            }
        };
        let outcome = self.uploader.put(
            (commit.key(), commit.sha256(), commit.size()),
            &commit.path(),
            deadline,
        );
        let finished = commit.finish(outcome).map_err(|error| errno(&error));
        self.note_change();
        finished
    }

    /// Tells the waiting commits that something they may wait for changed.
    fn note_change(&self) {
        *locked(&self.changes) += 1;
        self.changed.notify_all();
    }

    /// Waits until a change after `seen`, or `deadline` on the clock; whether a change came first.
    fn wait_for_change(&self, seen: u64, deadline: u64) -> bool {
        let mut changes = locked(&self.changes);
        #[cfg(test)]
        let _ = self.waits.fetch_add(1, Ordering::AcqRel);
        while *changes == seen {
            let now = self.clock.now();
            if now >= deadline {
                return false;
            }
            let wait = Duration::from_millis(deadline - now);
            changes = self
                .changed
                .wait_timeout(changes, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        true
    }

    /// The size and the free space of the stage's volume.
    ///
    /// # Errors
    /// What the system said.
    pub(in crate::server) fn space(&self) -> std::io::Result<Space> {
        self.store.disk.space(&self.store.stage)
    }

    /// Stops the sweep, for a successor that takes over (`true`), or starts it again (`false`).
    pub(in crate::server) fn hold(&self, on: bool) {
        let _sweeping = locked(&self.sweeping);
        self.held.store(on, Ordering::Release);
    }

    /// Runs the sweep of the store, unless held.
    pub(in crate::server) fn sweep(&self) {
        let _sweeping = locked(&self.sweeping);
        if self.held.load(Ordering::Acquire) {
            return;
        }
        let report = self.store.sweep();
        if report != super::sweep::SweepReport::default() {
            tracing::info!(?report, "tierstore fs: the put directory swept");
        }
    }

    /// Starts the thread that sweeps the store every `every`; it ends with the space.
    ///
    /// # Errors
    /// What the system said when the thread cannot be made.
    pub(in crate::server) fn start_sweep(self: &Arc<Self>, every: Duration) -> std::io::Result<()> {
        let (stop, wait) = mpsc::channel::<()>();
        let space: Weak<Self> = Arc::downgrade(self);
        let _ = std::thread::Builder::new()
            .name("fs-put-sweep".to_owned())
            .spawn(move || {
                // A space gone ends the channel too: the next wait ends the thread.
                while let Err(mpsc::RecvTimeoutError::Timeout) = wait.recv_timeout(every) {
                    let _ = space.upgrade().map(|space| space.sweep());
                }
            })?;
        *locked(&self.sweeper) = Some(stop);
        Ok(())
    }

    /// What a successor is handed: the directories the kernel holds, with the lookups of each
    /// where known, and the opens it holds, with the number of the staged file of each.
    pub(in crate::server) fn handed(&self) -> Handed {
        let state = locked(&self.state);
        let dirs = state
            .dirs
            .values()
            .map(|dir| HeldDir {
                lookups: dir.lookups,
                made: dir.made,
                path: dir.path.clone(),
            })
            .collect();
        let of_handle: BTreeMap<u64, u64> = self
            .store
            .open_handles()
            .into_iter()
            .map(|(handle, id)| (handle.0, id))
            .collect();
        let opens = state
            .handles
            .iter()
            .filter_map(|(fh, handle)| Some((*fh, *of_handle.get(&handle.0)?)))
            .collect();
        Handed { dirs, opens }
    }

    /// Takes over what the process before handed over (see [`PutSpace::handed`]). An open whose
    /// file is no longer staged is left out. Without a hand-over (`None`: a start after a crash),
    /// the directories of the store's names and those above them are known, with their lookups
    /// unknown, so that a directory the kernel holds that has a name is found again.
    pub(in crate::server) fn take_over(&self, handed: Option<Handed>) {
        let mut state = locked(&self.state);
        let Some(Handed { dirs, opens }) = handed else {
            for dir in self.store.dirs() {
                let mut path = dir.as_str();
                while !path.is_empty() {
                    let _ = state.dirs.entry(dir_ino(path)).or_insert_with(|| Dir {
                        path: path.to_owned(),
                        lookups: None,
                        made: false,
                    });
                    path = parent_of(path);
                }
            }
            return;
        };
        for HeldDir {
            lookups,
            made,
            path,
        } in dirs
        {
            if path.is_empty() || PutDir::parse(&path).is_err() {
                continue;
            }
            let _ = state.dirs.insert(
                dir_ino(&path),
                Dir {
                    path,
                    lookups,
                    made,
                },
            );
        }
        for (fh, id) in opens {
            if let Ok(handle) = self.store.adopt(id) {
                let _ = state.handles.insert(fh, handle);
            }
        }
    }
}

/// Milliseconds of `duration`, at most `u64::MAX`.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "space_commit_tests.rs"]
mod commit_tests;
#[cfg(test)]
#[path = "space_tests.rs"]
mod tests;
