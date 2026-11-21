//! The copy of a release's binary that the units of the archive package run,
//! [`SERVICE_BINARY`]: hostkeeper keeps every package closed to every user but root, so the
//! service account cannot run the binary inside the release, and the hook, which runs as root,
//! puts a copy where the account can.
//!
//! Every directory from the root down to the copy's is made when it is absent (mode 0755) and
//! must be a directory, not a link, owned by the owner (root on a host) and not writable by a
//! group or others: nobody but root can then replace the copy, or a directory above it. The copy
//! is written to a temporary file beside it (one a crash left, or a link put there, is removed
//! first, and the new one is made with `O_EXCL | O_NOFOLLOW`), given mode 0755, synced and renamed
//! over the copy, and the directory is synced. A process that runs the old copy keeps it: the
//! rename gives the path a new file, and the old one lives on until its last user is gone.

use std::fs::{self, DirBuilder, File};
use std::io::{self, ErrorKind};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags, fsync, open, openat, renameat, unlinkat};
use rustix::io::Errno;

use super::super::SERVICE_BINARY;
use super::Fault;

/// The mode of the copy and of every directory made for it: the owner writes, everyone reads and
/// runs.
const MODE: u32 = 0o755;
/// The file name of the temporary file the copy is written to, beside it.
const TEMP: &str = ".tierstore.new";

/// The copy of [`SERVICE_BINARY`] under a root (`/` on a host), and the user that must own it and
/// every directory above it.
pub(super) struct ServiceBinary<'a> {
    root: &'a Path,
    owner: u32,
}

/// The parts of [`SERVICE_BINARY`]: its directory's components below `/`, and its file name.
fn parts() -> (Vec<&'static str>, &'static str) {
    let mut parts: Vec<&str> = SERVICE_BINARY
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    let name = parts.pop().unwrap_or_default();
    (parts, name)
}

fn io_error(errno: Errno) -> io::Error {
    io::Error::from(errno)
}

impl<'a> ServiceBinary<'a> {
    /// The copy under `root`, whose directories `owner` must own.
    pub(super) fn new(root: &'a Path, owner: u32) -> ServiceBinary<'a> {
        ServiceBinary { root, owner }
    }

    /// The path of the copy under the root.
    pub(super) fn path(&self) -> PathBuf {
        self.root.join(SERVICE_BINARY.trim_start_matches('/'))
    }

    /// Whether `path` is a directory, not a link, owned by the owner and not writable by a group
    /// or others.
    fn check(&self, path: &Path) -> Result<(), Fault> {
        let meta = fs::symlink_metadata(path)
            .map_err(|error| Fault(format!("{}: {error}", path.display())))?;
        if !meta.file_type().is_dir() {
            return Err(format!("{} is not a directory", path.display()).into());
        }
        if meta.uid() != self.owner {
            return Err(format!(
                "{} is owned by user {}, not {}",
                path.display(),
                meta.uid(),
                self.owner
            )
            .into());
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!("{} can be written by a group or others", path.display()).into());
        }
        Ok(())
    }

    /// Makes `path`, a directory with mode [`MODE`] whatever the umask; one that is there is
    /// left as it is.
    fn make(path: &Path) -> Result<(), Fault> {
        match DirBuilder::new().mode(MODE).create(path) {
            Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(MODE))
                .map_err(|error| Fault(format!("{}: {error}", path.display()))),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(Fault(format!("{}: {error}", path.display()))),
        }
    }

    /// The directory of the copy, opened without following a link.
    fn open_dir(path: &Path) -> Result<OwnedFd, Fault> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        open(path, flags, Mode::empty())
            .map_err(|error| Fault(format!("{}: {error}", path.display())))
    }

    /// The directory of the copy, every directory from the root down to it made when it is
    /// absent and checked.
    fn made_dir(&self) -> Result<OwnedFd, Fault> {
        let mut path = self.root.to_path_buf();
        for component in parts().0 {
            path.push(component);
            ServiceBinary::make(&path)?;
            self.check(&path)?;
        }
        ServiceBinary::open_dir(&path)
    }

    /// The directory of the copy, every directory from the root down to it checked; `None` when
    /// one of them is not there.
    fn found_dir(&self) -> Result<Option<OwnedFd>, Fault> {
        let mut path = self.root.to_path_buf();
        for component in parts().0 {
            path.push(component);
            if fs::symlink_metadata(&path).is_err_and(|e| e.kind() == ErrorKind::NotFound) {
                return Ok(None);
            }
            self.check(&path)?;
        }
        ServiceBinary::open_dir(&path).map(Some)
    }

    /// Writes the temporary file in `dir` anew from `source`, mode [`MODE`], synced.
    fn write_temp(dir: &OwnedFd, source: &mut File) -> io::Result<()> {
        let _ = unlinkat(dir, TEMP, AtFlags::empty());
        let fd = openat(
            dir,
            TEMP,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o700),
        )
        .map_err(io_error)?;
        let mut file = File::from(fd);
        io::copy(source, &mut file)?;
        file.set_permissions(fs::Permissions::from_mode(MODE))?;
        file.sync_all()
    }

    /// Replaces the copy with the bytes of `source`, read from where it stands to its end.
    pub(super) fn install(&self, source: &mut File) -> Result<(), Fault> {
        let (_, name) = parts();
        let dir = self.made_dir()?;
        let replaced = ServiceBinary::write_temp(&dir, source)
            .and_then(|()| renameat(&dir, TEMP, &dir, name).map_err(io_error))
            .and_then(|()| fsync(&dir).map_err(io_error));
        replaced.map_err(|error| {
            let _ = unlinkat(&dir, TEMP, AtFlags::empty());
            Fault(format!("{}: {error}", self.path().display()))
        })
    }

    /// Removes the copy and a temporary file beside it; neither being there is fine, and so is a
    /// directory above it that is not there. A directory that is there is checked as for
    /// [`Self::install`] first.
    pub(super) fn remove(&self) -> Result<(), Fault> {
        let (_, name) = parts();
        let Some(dir) = self.found_dir()? else {
            return Ok(());
        };
        for file in [TEMP, name] {
            match unlinkat(&dir, file, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => {
                    return Err(Fault(format!(
                        "{}: {error}",
                        self.path().with_file_name(file).display()
                    )));
                }
            }
        }
        fsync(&dir).map_err(|error| Fault(format!("{}: {error}", self.path().display())))
    }
}
