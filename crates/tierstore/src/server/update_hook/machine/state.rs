//! The state directory: the checks that make it trustworthy, the lock, and `state.json`.
//!
//! The directory must be a real directory (not a link), owned by the expected user (the
//! updater's, root on a host), and writable by that user alone: with no group or other write
//! bit, the service, which runs as another user, cannot write there. The lock is `flock` on a
//! file of the directory, taken without waiting; it is held as long as the descriptor lives,
//! and the kernel drops it when the process ends, so a crashed step leaves none behind.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

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
/// The mode bits that let a group or others write.
const OTHERS_WRITE: u32 = 0o022;

/// The state directory `dir`, which `owner` (a user id) must own.
pub struct StateFiles {
    dir: PathBuf,
    owner: u32,
}

fn failed(what: &str, fault: &str) -> PortError {
    PortError::failed(KIND, format!("{what}: {fault}"))
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
        if meta.mode() & OTHERS_WRITE != 0 {
            return Err(format!("{shown} can be written by a group or others").into());
        }
        Ok(())
    }

    fn write_temp(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.dir.join(TEMP_FILE))?;
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
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
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
