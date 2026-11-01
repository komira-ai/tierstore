//! The state directory: the checks that make it trustworthy, the lock, and `state.json`.
//!
//! The directory must be a real directory (not a link), owned by the expected user (the hook's
//! own account on a host), writable by that user alone and closed to others: with no group
//! write bit the service, which runs as another user, cannot write there, and with no bit for
//! others it cannot read there either (a group may read). The lock is `flock` on a
//! file of the directory, taken without waiting; it is held as long as the descriptor lives,
//! and the kernel drops it when the process ends, so a crashed step leaves none behind.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
use rustix::io::Errno;

use super::super::ports::{Lock, PortError, PortKind, StateDir};
use super::{Fault, sync_dir};

const KIND: PortKind = PortKind::State;
/// The lock file, in the state directory.
pub const LOCK_FILE: &str = "lock";
/// The record, in the state directory.
pub const STATE_FILE: &str = "state.json";
const TEMP_FILE: &str = "state.json.tmp";
/// The mode bits a group or others must not have: the group's write bit and every bit of
/// others.
const FORBIDDEN: u32 = 0o027;

/// The state directory `dir`, which `owner` (a user id) must own.
pub struct StateFiles {
    dir: PathBuf,
    owner: u32,
}

fn failed(what: &str, fault: &str) -> PortError {
    PortError::failed(KIND, format!("{what}: {fault}"))
}

/// The bytes of the record, read through a descriptor opened with `O_NOFOLLOW`: a link at the
/// record's name is not followed.
fn read_record(path: &Path) -> io::Result<Vec<u8>> {
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut bytes = Vec::new();
    File::from(fd).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Takes the exclusive lock of `fd` without waiting: `Busy` when another descriptor holds it.
pub(super) fn try_lock(fd: BorrowedFd<'_>) -> Result<(), PortError> {
    match flock(fd, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(()),
        Err(Errno::WOULDBLOCK) => Err(PortError::Busy),
        Err(error) => Err(failed("lock", &error.to_string())),
    }
}

impl StateFiles {
    /// The directory `dir`, owned by the user id `owner`.
    #[must_use]
    pub fn new(dir: PathBuf, owner: u32) -> StateFiles {
        StateFiles { dir, owner }
    }

    fn check(&self) -> Result<(), Fault> {
        let shown = self.dir.display();
        let meta = fs::symlink_metadata(&self.dir)?;
        if meta.file_type().is_symlink() {
            return Err(format!("{shown} is a symbolic link").into());
        }
        if !meta.is_dir() {
            return Err(format!("{shown} is not a directory").into());
        }
        if meta.uid() != self.owner {
            return Err(format!(
                "{shown} is owned by user {}, not {}",
                meta.uid(),
                self.owner
            )
            .into());
        }
        if meta.mode() & FORBIDDEN != 0 {
            return Err(format!("{shown} can be written by a group or reached by others").into());
        }
        Ok(())
    }

    /// Writes the temporary file anew: one a crash left, or a link put there, is removed (the
    /// link itself, not what it points at) and the file is made with `O_EXCL | O_NOFOLLOW`.
    fn write_temp(&self, bytes: &[u8]) -> io::Result<()> {
        let path = self.dir.join(TEMP_FILE);
        let _ = fs::remove_file(&path);
        let fd = open(
            path.as_path(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )?;
        let mut file = File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()
    }
}

impl StateDir for StateFiles {
    fn lock(&self) -> Result<Lock, PortError> {
        self.check()
            .map_err(|fault| failed("state directory", &fault.to_string()))?;
        let path = self.dir.join(LOCK_FILE);
        let file = open(
            path.as_path(),
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|error| failed(&format!("open {}", path.display()), &error.to_string()))?;
        try_lock(file.as_fd())?;
        Ok(Lock::new(file))
    }

    fn load(&self) -> Result<Option<Vec<u8>>, PortError> {
        let path = self.dir.join(STATE_FILE);
        match read_record(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(failed(
                &format!("read {}", path.display()),
                &error.to_string(),
            )),
        }
    }

    fn save(&self, bytes: &[u8]) -> Result<(), PortError> {
        let temp = self.dir.join(TEMP_FILE);
        let path = self.dir.join(STATE_FILE);
        self.write_temp(bytes)
            .and_then(|()| fs::rename(&temp, &path))
            .and_then(|()| sync_dir(&self.dir))
            .map_err(|error| {
                let _ = fs::remove_file(&temp);
                failed(&format!("write {}", path.display()), &error.to_string())
            })
    }
}
