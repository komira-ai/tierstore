//! The lock file of a tierstore process: an exclusive `flock(2)` on a file, taken without
//! waiting, that decides which of two processes over one directory may write to it.
//!
//! The build node's lock is `<cache>/.lock` (see [`local_cache`](super::local_cache)); a storage
//! node's is the file `--lock-file` names. The process that holds it is the writer; one that
//! finds it held goes on without and tries again later. The kernel frees the lock when the
//! process exits or crashes, so nothing has to release it.
//!
//! A lock belongs to the open file description, not to the process: a descriptor of it sent to
//! another process (`SCM_RIGHTS`, in the hand-over of `server/handoff/`) holds the same lock, and
//! the kernel frees it only when the last descriptor of the description is closed. An unlock
//! (`flock(LOCK_UN)`) through any of them frees it for all, so a lock whose description another
//! process has a descriptor of too is never unlocked by a drop: [`LockFile::disarm`] for the one
//! that handed it over, [`Received`] for the one it was handed to.
//!
//! A lock is on the file that was opened, not on the name. A file that is removed and made again
//! under the same name is another file, and a second process can lock it while the first still
//! holds the old one, so [`LockFile::is_current`] says whether the name still leads to the file
//! that is held. The file is opened with `O_NOFOLLOW` and `O_CLOEXEC`, and refused on a network
//! file system, where `flock` is not a promise between hosts.

use std::fs::{File, TryLockError};
use std::io;
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags, openat};

/// A lock file that is held. Dropping it unlocks, and closes the file, unless it was
/// [disarmed](LockFile::disarm): then it only closes.
#[derive(Debug)]
pub(super) struct LockFile {
    file: File,
    path: PathBuf,
    /// Whether the drop unlocks.
    armed: bool,
}

/// Opens the lock file at `path` (making it, mode 0600, if it is not there) and tries the
/// exclusive lock without waiting: `Some` if this call holds it now, `None` if another open
/// file holds it.
///
/// # Errors
/// What the system said about the file: it cannot be opened, it is a link, or it is on a
/// network file system.
pub(super) fn try_take(path: &Path) -> io::Result<Option<LockFile>> {
    let flags = OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = openat(CWD, path, flags, Mode::RUSR | Mode::WUSR)?;
    let file = File::from(fd);
    check_file_system(&file, path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(why)) => return Err(why),
    }
    let taken = LockFile {
        file,
        path: path.to_owned(),
        armed: true,
    };
    // The name may have been given to another file between the open and the lock: a lock on a
    // file nothing else will look at. It is dropped, and the caller tries again.
    let current = taken.is_current();
    if current {
        tracing::debug!("tierstore: lock file taken: {}", path.display());
    }
    Ok(current.then_some(taken))
}

/// Why `path` cannot be a lock file, if it cannot: the words that follow its name in a refusal.
/// A relative path would lead to another file when the working directory is another, and two
/// processes of one node must name one file.
pub(super) fn check_lock_path(path: &Path) -> Result<(), &'static str> {
    if !path.is_absolute() {
        return Err("must be an absolute path");
    }
    if path.file_name().is_none() {
        return Err("must name a file, not a directory");
    }
    Ok(())
}

impl LockFile {
    /// Whether the name this lock was taken under still leads to the file that is held (the
    /// `fstat` of the descriptor against the `lstat` of the path). `false` if the file was
    /// replaced, turned into a link or removed.
    pub(super) fn is_current(&self) -> bool {
        let identity = |meta: std::fs::Metadata| (meta.dev(), meta.ino());
        let held = self.file.metadata().ok().map(identity);
        let named = std::fs::symlink_metadata(&self.path).ok().map(identity);
        held.is_some_and(|held| Some(held) == named)
    }

    /// The path the lock was taken under.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// A descriptor of the same open file description, which holds the same lock: what an offer
    /// of the lock sends.
    ///
    /// # Errors
    /// What the system said about the copy.
    pub(super) fn share(&self) -> io::Result<OwnedFd> {
        self.file.as_fd().try_clone_to_owned()
    }

    /// From here the drop only closes the file: another process holds a descriptor of the lock
    /// too, and an unlock would free it for that one as well.
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }

    /// Unlocks, for every descriptor of the description: the one unlock of a lock that was handed
    /// over, made by the process that handed it once its successor is gone.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn unlock(&self) -> io::Result<()> {
        self.file.unlock()?;
        tracing::debug!("tierstore: lock file unlocked: {}", self.path.display());
        Ok(())
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if !self.armed {
            tracing::debug!("tierstore: lock file closed: {}", self.path.display());
            return;
        }
        // Unlocked before the close, so a copy of the descriptor that a forked child still has
        // does not keep the lock. An error leaves the close to release it.
        let _ = self.file.unlock();
        tracing::debug!("tierstore: lock file released: {}", self.path.display());
    }
}

/// A descriptor of a lock that another process holds, received in a hand-over. Dropping it only
/// closes it: the lock stays with the other process. It becomes a [`LockFile`] when its holder
/// becomes the writer ([`Received::into_writer`]).
#[derive(Debug)]
pub(crate) struct Received(LockFile);

impl Received {
    /// The descriptor `fd`, received as the lock file at `path`.
    ///
    /// # Errors
    /// `InvalidData` when the descriptor is not a regular file, or not the file at `path` (its
    /// `fstat` is not the `lstat` of the path); what the system said when neither can be asked.
    pub(super) fn of(fd: OwnedFd, path: &Path) -> io::Result<Self> {
        // The kernel hands a descriptor over open on exec; a lock taken here is opened without.
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
        let file = File::from(fd);
        let received = Self(LockFile {
            file,
            path: path.to_owned(),
            armed: false,
        });
        if !received.0.file.metadata()?.is_file() {
            return Err(invalid(format!(
                "the lock descriptor for {} is not a regular file",
                path.display()
            )));
        }
        if !received.0.is_current() {
            return Err(invalid(format!(
                "the lock descriptor is not the file at {}",
                path.display()
            )));
        }
        Ok(received)
    }

    /// Whether the path still leads to the file this descriptor is of.
    pub(super) fn is_current(&self) -> bool {
        self.0.is_current()
    }

    /// The lock, held by its new writer: from here its drop unlocks, as a lock taken does.
    pub(super) fn into_writer(self) -> LockFile {
        let Self(mut file) = self;
        file.armed = true;
        file
    }
}

/// An error of the kind `InvalidData` that says `why`.
fn invalid(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

/// The file system types whose `flock` is not exclusive between processes on other hosts, or
/// not kept at all (the `f_type` magic numbers of `statfs(2)`): NFS, SMB, CIFS, SMB2, Ceph,
/// FUSE (sshfs, glusterfs, virtiofs), Lustre, GPFS, OCFS2, 9p and AFS. A file system with no
/// stable magic number is not in the list.
///
/// The lock file is `<cache>/.lock`, opened without following a link, so the file system of the
/// cache directory is the one of the lock file, and checking the lock file checks both; a
/// storage node's lock file is checked, and nothing else it has is.
// Used by the Linux `check_file_system` and by the tests; unused elsewhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) const NETWORK: [i128; 11] = [
    0x6969,
    0x517B,
    0xFF53_4D42,
    0xFE53_4D42,
    0x00C3_6400,
    0x6573_5546,
    0x0BD0_0BD0,
    0x4750_4653,
    0x7461_636F,
    0x0102_1997,
    0x5346_414F,
];

/// Refuses a lock file on a file system whose `statfs` type is `magic`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn check_type(magic: i128, path: &Path) -> io::Result<()> {
    if NETWORK.contains(&magic) {
        let why = format!(
            "{} is on a network file system (FUSE and cluster file systems included); a lock \
             file must be on a local one",
            path.display()
        );
        return Err(io::Error::other(why));
    }
    Ok(())
}

/// Asks the file system of `file` what it is, and refuses a network one.
#[cfg(target_os = "linux")]
fn check_file_system(file: &File, path: &Path) -> io::Result<()> {
    let magic = rustix::fs::fstatfs(file)?.f_type;
    check_type(i128::from(magic), path)
}

/// Other systems have no `f_type` to ask; the lock is taken as it is.
#[cfg(not(target_os = "linux"))]
fn check_file_system(_file: &File, _path: &Path) -> io::Result<()> {
    Ok(())
}
