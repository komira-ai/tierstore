//! The release links of a single-instance host: `current` and `previous` in one directory
//! (`/usr/local/tierstore` on a Mac), each a symbolic link to a staged release.
//!
//! The directory is opened once, with `O_DIRECTORY | O_NOFOLLOW`, and must be owned by the hook's
//! user with no group or other write bit: the service runs the binary `current` points at, so an
//! account that could change a link there could choose what the service runs. Every link is made,
//! read and removed relative to that descriptor; a new link is made beside the old under a name of
//! its own and renamed over it, and the directory is synced after it.
//!
//! A staged release is checked before the service is pointed at it ([`Releases::check`]): its
//! path is walked from the host's root one component at a time, each directory opened relative
//! to the one before with `O_DIRECTORY | O_NOFOLLOW` and checked on that descriptor, and the
//! binary opened relative to the last with `O_NOFOLLOW`. Nothing on the way may be a link, and
//! everything must be the release owner's (root) with no group or other write bit, so no other
//! account can change what the checked path names afterwards. The root itself is the host's own
//! and is not checked, as for the declaration.

use std::cell::OnceCell;
use std::ffi::OsStr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, fstat, fsync, open, openat, readlinkat, renameat, statat,
    symlinkat, unlinkat,
};
use rustix::io::Errno;

use super::super::plain::is_plain;
use super::super::ports::{Link, PortError, PortKind, Releases};
use super::Fault;
use super::host::RELEASE_BINARY;
use super::state::{others_write, permissions};

const KIND: PortKind = PortKind::Slots;

/// The links of the directory `dir`, which `owner` must own; staged releases are read under
/// `root` and must be `release_owner`'s.
pub struct ReleaseLinks {
    dir: PathBuf,
    owner: u32,
    root: PathBuf,
    release_owner: u32,
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
            root: PathBuf::from("/"),
            release_owner: 0,
            fd: OnceCell::new(),
        }
    }

    /// The same, reading staged releases under `root` (`/` unless this says otherwise) and
    /// requiring them to be `release_owner`'s (root, 0, unless this says otherwise).
    #[must_use]
    pub fn with_releases_under(mut self, root: PathBuf, release_owner: u32) -> ReleaseLinks {
        self.root = root;
        self.release_owner = release_owner;
        self
    }

    /// Whether the object open as `fd`, at `at`, is the release owner's and writable by no group
    /// or others.
    fn trusted(&self, fd: &OwnedFd, at: &Path) -> Result<FileType, String> {
        let stat = fstat(fd).map_err(|errno| format!("{}: {errno}", at.display()))?;
        if stat.st_uid != self.release_owner {
            return Err(format!(
                "{} is owned by user {}, not {}",
                at.display(),
                stat.st_uid,
                self.release_owner
            ));
        }
        if others_write(permissions(&stat)) {
            return Err(format!(
                "{} can be written by a group or others",
                at.display()
            ));
        }
        Ok(FileType::from_raw_mode(stat.st_mode))
    }

    /// Opens `name` in `dir`, at `at`, without following a link, saying which it was when it is
    /// one.
    fn open_in(
        dir: BorrowedFd<'_>,
        name: &OsStr,
        flags: OFlags,
        at: &Path,
    ) -> Result<OwnedFd, String> {
        openat(
            dir,
            name,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| {
            let link = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Symlink);
            if link {
                format!("{} is a symbolic link", at.display())
            } else {
                format!("{}: {errno}", at.display())
            }
        })
    }

    /// The walk of [`Releases::check`].
    fn walk(&self, release: &Path) -> Result<(), String> {
        if !is_plain(release) {
            return Err("it is not an absolute path of plain components".to_owned());
        }
        let mut dir = open(
            self.root.as_path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| format!("the host root {}: {errno}", self.root.display()))?;
        let mut at = PathBuf::from("/");
        for part in release.iter().skip(1) {
            at.push(part);
            dir = Self::open_in(dir.as_fd(), part, OFlags::RDONLY | OFlags::DIRECTORY, &at)?;
            self.trusted(&dir, &at)?;
        }
        at.push(RELEASE_BINARY);
        let binary = Self::open_in(dir.as_fd(), OsStr::new(RELEASE_BINARY), OFlags::RDONLY, &at)?;
        if self.trusted(&binary, &at)? != FileType::RegularFile {
            return Err(format!("{} is not a regular file", at.display()));
        }
        Ok(())
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

    fn check(&self, release: &Path) -> Result<(), PortError> {
        self.walk(release).map_err(|why| {
            PortError::failed(
                PortKind::Host,
                format!("the release {} is not trusted: {why}", release.display()),
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
