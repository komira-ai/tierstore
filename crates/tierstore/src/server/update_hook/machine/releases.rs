//! The release links of a single-instance host: `current` and `previous` in one directory
//! (`/usr/local/tierstore` on a Mac), each a symbolic link to a staged release.
//!
//! The directory is opened once, with `O_DIRECTORY | O_NOFOLLOW`, and must be owned by the hook's
//! user with no group or other write bit: the service runs the binary `current` points at, so an
//! account that could change a link there could choose what the service runs. Every link is made,
//! read and removed relative to that descriptor; a new link is made beside the old under a name of
//! its own and renamed over it, and the directory is synced after it.

use std::cell::OnceCell;
use std::ffi::OsStr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags, fstat, fsync, readlinkat, renameat, symlinkat, unlinkat};
use rustix::io::Errno;

use super::super::ports::{Link, PortError, PortKind, Releases};
use super::Fault;
use super::state::{others_write, permissions};

const KIND: PortKind = PortKind::Slots;

/// The links of the directory `dir`, which `owner` must own.
pub struct ReleaseLinks {
    dir: PathBuf,
    owner: u32,
    fd: OnceCell<OwnedFd>,
}

fn failed(what: impl std::fmt::Display, fault: impl std::fmt::Display) -> PortError {
    PortError::failed(KIND, format!("{what}: {fault}"))
}

impl ReleaseLinks {
    /// The links in `dir`, a directory `owner` must own with no group or other write bit.
    #[must_use]
    pub fn new(dir: PathBuf, owner: u32) -> ReleaseLinks {
        ReleaseLinks {
            dir,
            owner,
            fd: OnceCell::new(),
        }
    }

    fn path(&self, link: Link) -> PathBuf {
        self.dir.join(link.name())
    }

    /// The descriptor of the directory, opened and checked on first use.
    fn dir_fd(&self) -> Result<BorrowedFd<'_>, PortError> {
        if let Some(fd) = self.fd.get() {
            return Ok(fd.as_fd());
        }
        let fd = self.open_checked().map_err(|fault| {
            failed(format!("the links directory {}", self.dir.display()), fault)
        })?;
        Ok(self.fd.get_or_init(|| fd).as_fd())
    }

    fn open_checked(&self) -> Result<OwnedFd, Fault> {
        let fd = rustix::fs::open(
            self.dir.as_path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| match errno {
            // A link at the path itself: O_NOFOLLOW refuses it with ELOOP (Linux) or ENOTDIR
            // (macOS, with O_DIRECTORY); either way it is not the directory.
            Errno::LOOP | Errno::NOTDIR => Fault("is a symbolic link or not a directory".into()),
            errno => Fault(errno.to_string()),
        })?;
        let stat = fstat(&fd).map_err(|errno| Fault(errno.to_string()))?;
        if stat.st_uid != self.owner {
            return Err(format!("is owned by user {}, not {}", stat.st_uid, self.owner).into());
        }
        if others_write(permissions(&stat)) {
            return Err("can be written by a group or others".into());
        }
        Ok(fd)
    }

    /// Where `link` points; `None` when there is none.
    fn read_in(&self, dir: BorrowedFd<'_>, link: Link) -> Result<Option<PathBuf>, PortError> {
        let path = self.path(link);
        match readlinkat(dir, link.name(), Vec::new()) {
            Ok(target) => Ok(Some(PathBuf::from(OsStr::from_bytes(target.as_bytes())))),
            Err(Errno::NOENT) => Ok(None),
            // `readlink` of a path that is not a link: EINVAL.
            Err(Errno::INVAL) => Err(PortError::failed(
                KIND,
                format!("{} exists and is not a symbolic link", path.display()),
            )),
            Err(errno) => Err(failed(format!("read {}", path.display()), errno)),
        }
    }
}

impl Releases for ReleaseLinks {
    fn target(&self, link: Link) -> Result<Option<PathBuf>, PortError> {
        self.read_in(self.dir_fd()?, link)
    }

    fn point(&self, link: Link, release: &Path) -> Result<(), PortError> {
        let dir = self.dir_fd()?;
        let temp = format!(".{}.tmp", link.name());
        // A link left by a crash between the two calls below; absent in the usual case.
        let _ = unlinkat(dir, temp.as_str(), AtFlags::empty());
        let placed = symlinkat(release, dir, temp.as_str())
            .and_then(|()| renameat(dir, temp.as_str(), dir, link.name()))
            .and_then(|()| fsync(dir));
        placed.map_err(|errno| {
            let _ = unlinkat(dir, temp.as_str(), AtFlags::empty());
            failed(
                format!(
                    "link {} to {}",
                    self.path(link).display(),
                    release.display()
                ),
                errno,
            )
        })
    }

    fn remove(&self, link: Link) -> Result<(), PortError> {
        let dir = self.dir_fd()?;
        if self.read_in(dir, link)?.is_none() {
            return Ok(());
        }
        unlinkat(dir, link.name(), AtFlags::empty())
            .and_then(|()| fsync(dir))
            .map_err(|errno| failed(format!("remove {}", self.path(link).display()), errno))
    }
}
