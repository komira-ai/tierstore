//! The keys that start with `_entries/` in a directory tier: where their files are, and the lock
//! that makes a conditional write of one atomic between processes.
//!
//! The file of a key `_entries/<name>` is `<name>`, encoded as any key is, in the subdirectory
//! `_entries/` of the root, which is made when the first of them is written. A listing of the
//! root keeps only regular files, so a release that does not know the subdirectory does not see
//! it. A listing of the prefix `_entries/` reads that subdirectory and no other directory.
//!
//! The lock is an exclusive `flock(2)` on the file `.lock` in the subdirectory. It is held by
//! each create-only put, each replacement by the content tag and each delete of such a key, from
//! its check to its change, so that another process over the same directory cannot come between
//! the two: a replacement never makes a key that a delete just removed, and a create-only put
//! never lands between the check and the move of a replacement. `flock` is a promise between the
//! processes of one host and file system only, as the lock file of the server says.

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::{Error, PLAIN_MODE, failed, io_error};

/// The name of the subdirectory of the root that holds the files of these keys.
pub(super) const DIR: &str = "_entries";

/// The prefix of a key whose file is in the subdirectory.
pub(super) const PREFIX: &str = "_entries/";

/// The file of the lock, in the subdirectory. Its name starts with `.`, so it is never listed.
const LOCK_FILE: &str = ".lock";

/// The part of `key` behind the prefix, if it has the prefix.
pub(super) fn rest(key: &str) -> Option<&str> {
    key.strip_prefix(PREFIX)
}

/// The lock of the subdirectory, held until this is dropped.
pub(super) struct EntryLock {
    _file: File,
}

/// Waits for the lock of the subdirectory `dir` and takes it, making `dir` first if it is not
/// there. Gives the lock, and whether this call made `dir`, so that the caller syncs the root.
/// The wait is on a thread of the blocking pool, so that it holds up no task.
pub(super) async fn take(dir: PathBuf) -> Result<(EntryLock, bool), Error> {
    let waiting = tokio::task::spawn_blocking(move || lock(&dir));
    waiting.await.map_err(|why| Error::Other(why.to_string()))?
}

/// Makes `dir` if it is not there, opens its lock file and waits for the lock on it.
fn lock(dir: &Path) -> Result<(EntryLock, bool), Error> {
    let created = match std::fs::create_dir(dir) {
        Ok(()) => true,
        Err(why) if why.kind() == ErrorKind::AlreadyExists => false,
        Err(why) => return Err(failed(dir, &why)),
    };
    let path = dir.join(LOCK_FILE);
    let mut options = OpenOptions::new();
    options
        .create(true)
        .truncate(false)
        .write(true)
        .mode(PLAIN_MODE);
    let file = options.open(&path).map_err(io_error(&path))?;
    file.lock().map_err(io_error(&path))?;
    Ok((EntryLock { _file: file }, created))
}
