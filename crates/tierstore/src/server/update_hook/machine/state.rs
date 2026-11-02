//! The state directory: the checks that make it trustworthy, the lock, and `state.json`.
//!
//! The directory is opened once, with `O_DIRECTORY | O_NOFOLLOW`, and every check is made on
//! that descriptor: it must be a directory owned by the hook's own user and group with mode
//! exactly 0750 (the group, which holds the service account, may read and search it, and nobody
//! may write to it but the hook), and the service account must be in that group: its primary group, or listed in it. A
//! private directory ([`StateFiles::private`], a launchd host's) must be owned by the hook's
//! user with mode exactly 0700, and its group is not looked at. Every
//! later open, the lock file, `state.json` and its temporary file, the rename and the sync, is
//! made relative to that descriptor, so a swap of the path after the check changes nothing.
//! The record must be a regular file of the hook's user that no group or others can write. The
//! lock is `flock` on a file of the directory, taken without waiting; it is held as long as the
//! descriptor lives, and the kernel drops it when the process ends, so a crashed step leaves
//! none behind. Nothing here repairs a mode: a directory or file that is not as it must be is
//! refused.

use std::cell::OnceCell;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;

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
/// The one mode the state directory may have when the service's group may reach it.
const DIR_MODE: Mode = Mode::from_bits_truncate(0o750);
/// The one mode a private state directory may have: the hook's alone.
const PRIVATE_MODE: Mode = Mode::from_bits_truncate(0o700);

/// The permission bits of `stat` (`mode_t` is `u16` on macOS and `u32` on Linux).
pub(super) fn permissions(stat: &rustix::fs::Stat) -> Mode {
    Mode::from_raw_mode(stat.st_mode)
}

/// Whether `mode` lets a group or others write.
pub(super) fn others_write(mode: Mode) -> bool {
    mode.intersects(Mode::WGRP | Mode::WOTH)
}

/// Who the state directory belongs to and who must be able to reach it.
pub struct Account {
    /// The user id that must own the directory and the record: the hook's.
    pub owner: u32,
    /// The group id that must own the directory: the hook's.
    pub group: u32,
    /// The name of the service account, which must be a member of `group`.
    pub service: String,
    /// The file that lists the groups' members (`/etc/group`, under the host's root); the
    /// `passwd` file that gives the primary groups is read beside it.
    pub group_file: PathBuf,
}

/// Who besides the hook reaches the state directory.
enum Reach {
    /// The service, through the directory's group (the slot links of Linux): mode 0750.
    Shared(Account),
    /// Nobody: the hook's user alone, the user id given (a launchd host, whose service runs
    /// from the release links and never reads the state directory): mode 0700, no group check.
    Private(u32),
}

/// The state directory `dir`.
pub struct StateFiles {
    dir: PathBuf,
    reach: Reach,
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

/// The members of the groups numbered `gid` in the text of a group file (`name:password:gid:a,b`).
fn members_of(text: &str, gid: u32) -> impl Iterator<Item = &str> {
    text.lines()
        .filter_map(move |line| {
            let parts: Vec<&str> = line.splitn(4, ':').collect();
            let [_, _, id, members] = parts[..] else {
                return None;
            };
            (id.parse() == Ok(gid)).then_some(members)
        })
        .flat_map(|members| members.split(','))
}

/// The primary group of the account `name` in the text of a passwd file
/// (`name:password:uid:gid:...`).
fn primary_group(text: &str, name: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next() == Some(name))
            .then(|| fields.nth(2)?.parse().ok())
            .flatten()
    })
}

/// Whether the service account is a member of the directory's group: the service reaches the
/// slot links through that membership. It is a member when the group is its primary group
/// (the `gid` field of its line in `passwd`, next to the group file) or when the group's
/// member list names it. Only those two files are read, not the name service switch.
fn check_service(account: &Account) -> Result<(), Fault> {
    let file = &account.group_file;
    let text = fs::read_to_string(file)
        .map_err(|error| Fault(format!("cannot read {}: {error}", file.display())))?;
    let service = account.service.as_str();
    if members_of(&text, account.group).any(|member| member == service) {
        return Ok(());
    }
    let passwd = file.with_file_name("passwd");
    let primary = fs::read_to_string(&passwd)
        .ok()
        .and_then(|lines| primary_group(&lines, service));
    if primary == Some(account.group) {
        return Ok(());
    }
    Err(format!(
        "the service account {service} is not a member of group {} in {} and its primary group in {} is not that group",
        account.group,
        file.display(),
        passwd.display()
    )
    .into())
}

impl StateFiles {
    /// The directory `dir`, held by `account`.
    #[must_use]
    pub fn new(dir: PathBuf, account: Account) -> StateFiles {
        StateFiles {
            dir,
            reach: Reach::Shared(account),
            fd: OnceCell::new(),
        }
    }

    /// The directory `dir`, the hook's alone: owned by `owner` with mode 0700. Its group is not
    /// checked, since the mode gives the group nothing.
    #[must_use]
    pub fn private(dir: PathBuf, owner: u32) -> StateFiles {
        StateFiles {
            dir,
            reach: Reach::Private(owner),
            fd: OnceCell::new(),
        }
    }

    /// The user that must own the directory and the record: the hook's.
    fn owner(&self) -> u32 {
        match &self.reach {
            Reach::Shared(account) => account.owner,
            Reach::Private(owner) => *owner,
        }
    }

    /// The path of the directory, for the messages of the slot links (Linux's).
    #[cfg(target_os = "linux")]
    pub(super) fn path(&self) -> &std::path::Path {
        &self.dir
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
        if stat.st_uid != self.owner() {
            return Err(format!(
                "{shown} is owned by user {}, not {}",
                stat.st_uid,
                self.owner()
            )
            .into());
        }
        let (mode, account) = match &self.reach {
            Reach::Shared(account) => (DIR_MODE, Some(account)),
            Reach::Private(_) => (PRIVATE_MODE, None),
        };
        if let Some(account) = account
            && stat.st_gid != account.group
        {
            return Err(format!(
                "{shown} belongs to group {}, not {}",
                stat.st_gid, account.group
            )
            .into());
        }
        let found = permissions(&stat);
        if found != mode {
            return Err(format!(
                "{shown} has mode {:04o}, not {:04o}",
                found.bits(),
                mode.bits()
            )
            .into());
        }
        if let Some(account) = account {
            check_service(account)?;
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
        if stat.st_uid != self.owner() {
            return Err(format!("is owned by user {}, not {}", stat.st_uid, self.owner()).into());
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
