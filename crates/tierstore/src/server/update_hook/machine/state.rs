//! The state directory: the checks that make it trustworthy, the lock, and `state.json`.
//!
//! The directory is made when it is absent (mode 0700, its missing parents 0755), then opened
//! once, with `O_DIRECTORY | O_NOFOLLOW`, and every check is made on that descriptor: it must be
//! a directory owned by the hook's own user with mode exactly 0700. Every later open, the lock
//! file, `state.json` and its temporary file, the rename and the sync, is made relative to that
//! descriptor, so a swap of the path after the check changes nothing. The record must be a
//! regular file of the hook's user that no group or others can write. The lock is `flock` on a
//! file of the directory, taken without waiting; it is held as long as the descriptor lives, and
//! the kernel drops it when the process ends, so a crashed step leaves none behind. Nothing here
//! repairs a mode: a directory or file that is not as it must be is refused.

use std::cell::OnceCell;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, FileType, FlockOperation, Mode, OFlags, flock, fstat, fsync, openat, renameat,
    unlinkat,
};
use rustix::io::Errno;

use super::super::ports::{Lock, PortError, PortKind, StateDir};
use super::Fault;

const KIND: PortKind = PortKind::State;
/// The lock file, in the state directory.
pub const LOCK_FILE: &str = "lock";
/// The record, in the state directory.
pub const STATE_FILE: &str = "state.json";
const TEMP_FILE: &str = "state.json.tmp";
/// The one mode the state directory may have: the hook's alone.
const DIR_MODE: Mode = Mode::from_bits_truncate(0o700);
/// The mode of the parents of the directory when the hook makes them.
const PARENT_MODE: u32 = 0o755;

/// The permission bits of `stat` (`mode_t` is `u16` on macOS and `u32` on Linux).
pub(super) fn permissions(stat: &rustix::fs::Stat) -> Mode {
    Mode::from_raw_mode(stat.st_mode)
}

/// Whether `mode` lets a group or others write.
pub(super) fn others_write(mode: Mode) -> bool {
    mode.intersects(Mode::WGRP | Mode::WOTH)
}

/// The state directory `dir`.
pub struct StateFiles {
    dir: PathBuf,
    owner: u32,
    fd: OnceCell<OwnedFd>,
}

fn failed(what: &str, fault: &str) -> PortError {
    PortError::failed(KIND, format!("{what}: {fault}"))
}

fn io_error(errno: Errno) -> io::Error {
    io::Error::from(errno)
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
    /// The directory `dir`, owned by `owner` (the user the hook runs as), with mode 0700.
    #[must_use]
    pub fn new(dir: PathBuf, owner: u32) -> StateFiles {
        StateFiles {
            dir,
            owner,
            fd: OnceCell::new(),
        }
    }

    /// Makes the directory when it is absent: mode 0700, and the parents that are missing 0755.
    /// A directory that is there, or a path that cannot be looked at, is left for the checks.
    fn make_missing(&self) -> Result<(), Fault> {
        let absent = matches!(
            fs::symlink_metadata(&self.dir),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        );
        if absent {
            let parent = self.dir.parent().unwrap_or(Path::new("/"));
            DirBuilder::new()
                .recursive(true)
                .mode(PARENT_MODE)
                .create(parent)?;
            DirBuilder::new().mode(0o700).create(&self.dir)?;
        }
        Ok(())
    }

    /// The descriptor of the directory, opened and checked on first use. Every later call, and
    /// every file of the directory, goes through this one descriptor.
    pub(super) fn dir_fd(&self) -> Result<BorrowedFd<'_>, PortError> {
        if let Some(fd) = self.fd.get() {
            return Ok(fd.as_fd());
        }
        let fd = self
            .open_checked()
            .map_err(|fault| failed("state directory", &fault.to_string()))?;
        Ok(self.fd.get_or_init(|| fd).as_fd())
    }

    fn open_checked(&self) -> Result<OwnedFd, Fault> {
        self.make_missing()?;
        let shown = self.dir.display();
        let fd = rustix::fs::open(
            self.dir.as_path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| {
            let link =
                fs::symlink_metadata(&self.dir).is_ok_and(|meta| meta.file_type().is_symlink());
            if link {
                Fault(format!("{shown} is a symbolic link"))
            } else if errno == Errno::NOTDIR {
                Fault(format!("{shown} is not a directory"))
            } else {
                Fault(errno.to_string())
            }
        })?;
        let stat = fstat(&fd).map_err(io_error)?;
        if stat.st_uid != self.owner {
            return Err(format!(
                "{shown} is owned by user {}, not {}",
                stat.st_uid, self.owner
            )
            .into());
        }
        let found = permissions(&stat);
        if found != DIR_MODE {
            return Err(format!(
                "{shown} has mode {:04o}, not {:04o}",
                found.bits(),
                DIR_MODE.bits()
            )
            .into());
        }
        Ok(fd)
    }

    /// Checks the record opened as `fd`: a regular file of the hook's user that no group or
    /// others can write. The service must not be able to forge it.
    fn check_record(&self, fd: &OwnedFd) -> Result<(), Fault> {
        let stat = fstat(fd).map_err(io_error)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err("is not a regular file".into());
        }
        if stat.st_uid != self.owner {
            return Err(format!("is owned by user {}, not {}", stat.st_uid, self.owner).into());
        }
        if others_write(permissions(&stat)) {
            return Err("can be written by a group or others".into());
        }
        Ok(())
    }

    /// The record opened without following a link and checked; `None` when there is none.
    fn open_record(&self, dir: BorrowedFd<'_>) -> Result<Option<OwnedFd>, Fault> {
        match openat(
            dir,
            STATE_FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => self.check_record(&fd).map(|()| Some(fd)),
            Err(Errno::NOENT) => Ok(None),
            Err(errno) => Err(Fault(errno.to_string())),
        }
    }

    /// Writes the temporary file anew (one a crash left, or a link put there, is removed: the
    /// link itself, not what it points at), made with `O_EXCL | O_NOFOLLOW`.
    fn write_temp(&self, dir: BorrowedFd<'_>, bytes: &[u8]) -> io::Result<()> {
        let _ = unlinkat(dir, TEMP_FILE, AtFlags::empty());
        let fd = openat(
            dir,
            TEMP_FILE,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(io_error)?;
        let mut file = File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()
    }

    /// The record replaced by `bytes`, in the directory `dir`.
    fn replace(&self, dir: BorrowedFd<'_>, bytes: &[u8]) -> Result<(), Fault> {
        // An existing record that the service could have written is not replaced.
        self.open_record(dir)?;
        self.write_temp(dir, bytes)?;
        renameat(dir, TEMP_FILE, dir, STATE_FILE).map_err(io_error)?;
        fsync(dir).map_err(io_error)?;
        Ok(())
    }
}

impl StateDir for StateFiles {
    fn lock(&self) -> Result<Lock, PortError> {
        let dir = self.dir_fd()?;
        let file = openat(
            dir,
            LOCK_FILE,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|error| {
            failed(
                &format!("open {}", self.dir.join(LOCK_FILE).display()),
                &error.to_string(),
            )
        })?;
        try_lock(file.as_fd())?;
        Ok(Lock::new(file))
    }

    fn load(&self) -> Result<Option<Vec<u8>>, PortError> {
        let dir = self.dir_fd()?;
        let path = self.dir.join(STATE_FILE);
        let read = |fault: Fault| failed(&format!("read {}", path.display()), &fault.to_string());
        let Some(fd) = self.open_record(dir).map_err(read)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        File::from(fd)
            .read_to_end(&mut bytes)
            .map_err(|error| read(error.into()))?;
        Ok(Some(bytes))
    }

    fn save(&self, bytes: &[u8]) -> Result<(), PortError> {
        let dir = self.dir_fd()?;
        self.replace(dir, bytes).map_err(|fault| {
            let _ = unlinkat(dir, TEMP_FILE, AtFlags::empty());
            failed(
                &format!("write {}", self.dir.join(STATE_FILE).display()),
                &fault.to_string(),
            )
        })
    }
}
