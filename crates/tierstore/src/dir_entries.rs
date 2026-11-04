//! The keys that start with `_entries/` in a directory tier: where their files are, and the lock
//! that makes a conditional write of one atomic between processes.
//!
//! The file of a key `_entries/<name>` is `<name>`, encoded as any key is, in the subdirectory
//! `.entries/` of the root, which is made when the first of them is written. The name of the
//! subdirectory starts with `.`, and `encode` writes a `.` at the start of a key as `%2E`, so no
//! key, in this release or the one before, has a file of that name: the ordinary key `_entries`
//! (which is outside the prefix) keeps its own file beside the subdirectory. A listing of the root
//! keeps only regular files whose name does not start with `.`, so a release that does not know
//! the subdirectory does not see it. A listing of a prefix that can match a key under `_entries/`
//! reads that subdirectory as well as the root.
//!
//! The previous release did not know the prefix: it kept the key `_entries/<name>` as the file
//! `_entries~<name>` of the root, as it kept any key. A key that has such a file keeps it for its
//! whole life: a head, a get or a read finds that file, a replacement by the tag writes it in
//! place, a delete removes it, a create-only put finds the key taken, and a listing has the key
//! once, from the root. The previous release therefore reads the body this release wrote, and
//! finds the key not found after this release deletes it. Only a key with no such file is a file
//! of the subdirectory, and no write makes a file in the root for a key under the prefix. Nothing
//! is moved, so a read cannot miss a key between two places. A delete removes the file of the
//! subdirectory as well, if there is one.
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

use super::{Error, NAME_LIMIT, PLAIN_MODE, encode, failed, io_error};

/// The name of the subdirectory of the root that holds the files of these keys. It starts with
/// `.`, which no encoded key does, so it is never the file of a key.
pub(super) const DIR: &str = ".entries";

/// The prefix of a key whose file is in the subdirectory.
pub(super) const PREFIX: &str = "_entries/";

/// The file of the lock, in the subdirectory. Its name starts with `.`, so it is never listed.
const LOCK_FILE: &str = ".lock";

/// The part of `key` behind the prefix, if it has the prefix.
pub(super) fn rest(key: &str) -> Option<&str> {
    key.strip_prefix(PREFIX)
}

/// The file the previous release made for `key`, if `key` has the prefix and that file's name
/// could be a file name then.
pub(super) fn legacy(root: &Path, key: &str) -> Option<PathBuf> {
    rest(key)?;
    let name = encode(key);
    (name.len() <= NAME_LIMIT).then(|| root.join(name))
}

/// The lock of the subdirectory, held until this is dropped.
pub(super) struct EntryLock {
    _file: File,
}

/// Waits for the lock of the subdirectory `dir` and takes it, making `dir` first if it is not
/// there. The caller syncs the root, which a made directory is an entry of. `reached` is called
/// once, on the blocking thread, just before the wait for the lock begins. The wait is on a
/// thread of the blocking pool, so that it holds up no task.
pub(super) async fn take(
    dir: PathBuf,
    reached: impl FnOnce() + Send + 'static,
) -> Result<EntryLock, Error> {
    let waiting = tokio::task::spawn_blocking(move || lock(&dir, reached));
    waiting.await.map_err(|why| Error::Other(why.to_string()))?
}

/// Makes `dir` if it is not there, opens its lock file and waits for the lock on it.
fn lock(dir: &Path, reached: impl FnOnce()) -> Result<EntryLock, Error> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(why) if why.kind() == ErrorKind::AlreadyExists => {}
        Err(why) => return Err(failed(dir, &why)),
    }
    let path = dir.join(LOCK_FILE);
    let mut options = OpenOptions::new();
    options
        .create(true)
        .truncate(false)
        .write(true)
        .mode(PLAIN_MODE);
    let file = options.open(&path).map_err(io_error(&path))?;
    reached();
    file.lock().map_err(io_error(&path))?;
    Ok(EntryLock { _file: file })
}
