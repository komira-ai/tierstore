//! The copy of a release's binary that the units of the archive package run,
//! [`SERVICE_BINARY`]: hostkeeper keeps every package closed to every user but root, so the
//! service account cannot run the binary inside the release, and the hook, which runs as root,
//! puts a copy where the account can. The same copy, at [`CHECK_BINARY`], is what a root hook on a
//! Mac has the account check the node's file with when the release is of the installer package
//! ([`ServiceBinary::for_check`]).
//!
//! Every directory from the root down to the copy's is made when it is absent (mode 0755) and
//! must be a directory, not a link, owned by the owner (root on a host) and not writable by
//! others; the copy's own directory, which the hook makes, not by its group either. A directory
//! above it may be writable by its group, as `/usr/local` is (root:staff, mode 2775) on a host
//! that gives the group `staff` the right to install there: that group can put programs in root's
//! way anyway. Nobody else can replace the copy, or a directory above it. The copy
//! is written to a temporary file beside it (`.<name>.new`: one a crash left, or a link put there,
//! is removed first, and the new one is made with `O_EXCL | O_NOFOLLOW`), given mode 0755, synced
//! and renamed over the copy, and the directory is synced. A process that runs the old copy keeps
//! it: the rename gives the path a new file, and the old one lives on until its last user is gone.

use std::fs::{self, DirBuilder, File};
use std::io::{self, ErrorKind};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags, fsync, open, openat, renameat, unlinkat};
use rustix::io::Errno;

use super::super::{CHECK_BINARY, SERVICE_BINARY};
use super::Fault;

/// The mode of the copy and of every directory made for it: the owner writes, everyone reads and
/// runs.
const MODE: u32 = 0o755;

/// A copy of a release's binary under a root (`/` on a host), at an absolute path of the host,
/// and the user that must own it and every directory above it.
pub(super) struct ServiceBinary<'a> {
    root: &'a Path,
    owner: u32,
    at: &'static str,
}

/// The parts of the absolute path `at`: its directory's components below `/`, and its file name.
fn parts(at: &'static str) -> (Vec<&'static str>, &'static str) {
    let mut parts: Vec<&str> = at.split('/').filter(|p| !p.is_empty()).collect();
    let name = parts.pop().unwrap_or_default();
    (parts, name)
}

fn io_error(errno: Errno) -> io::Error {
    io::Error::from(errno)
}

impl<'a> ServiceBinary<'a> {
    /// The copy at [`SERVICE_BINARY`] under `root`, whose directories `owner` must own.
    pub(super) fn new(root: &'a Path, owner: u32) -> ServiceBinary<'a> {
        ServiceBinary {
            root,
            owner,
            at: SERVICE_BINARY,
        }
    }

    /// The copy at [`CHECK_BINARY`] under `root`, beside the one at [`SERVICE_BINARY`] and held
    /// to the same checks: the binary a root hook on a Mac runs a check as the service account
    /// with, when the release's own is inside a version directory the account cannot enter.
    pub(super) fn for_check(root: &'a Path, owner: u32) -> ServiceBinary<'a> {
        ServiceBinary {
            root,
            owner,
            at: CHECK_BINARY,
        }
    }

    /// The path of the copy under the root.
    pub(super) fn path(&self) -> PathBuf {
        self.root.join(self.at.trim_start_matches('/'))
    }

    /// The file name of the temporary file the copy is written to, beside it: `.<name>.new`.
    fn temp(&self) -> String {
        format!(".{}.new", parts(self.at).1)
    }

    /// Whether `path` is a directory, not a link, owned by the owner and not writable by others,
    /// nor by its group when `own` (the copy's own directory).
    fn check(&self, path: &Path, own: bool) -> Result<(), Fault> {
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
        if meta.mode() & 0o002 != 0 {
            return Err(format!("{} can be written by others", path.display()).into());
        }
        if own && meta.mode() & 0o020 != 0 {
            return Err(format!("{} can be written by its group", path.display()).into());
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

    /// The directory of the copy, every directory from the root down to it checked, and made
    /// first when `make` and it is absent.
    fn dir(&self, make: bool) -> Result<OwnedFd, Fault> {
        let components = parts(self.at).0;
        let mut path = self.root.to_path_buf();
        for (n, component) in components.iter().enumerate() {
            path.push(component);
            if make {
                ServiceBinary::make(&path)?;
            }
            self.check(&path, n + 1 == components.len())?;
        }
        ServiceBinary::open_dir(&path)
    }

    /// Writes the temporary file `temp` in `dir` anew from `source`, mode [`MODE`], synced.
    fn write_temp(dir: &OwnedFd, temp: &str, source: &mut File) -> io::Result<()> {
        let _ = unlinkat(dir, temp, AtFlags::empty());
        let fd = openat(
            dir,
            temp,
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
        let (_, name) = parts(self.at);
        let temp = self.temp();
        let dir = self.dir(true)?;
        let replaced = ServiceBinary::write_temp(&dir, &temp, source)
            .and_then(|()| renameat(&dir, temp.as_str(), &dir, name).map_err(io_error))
            .and_then(|()| fsync(&dir).map_err(io_error));
        replaced.map_err(|error| {
            let _ = unlinkat(&dir, temp.as_str(), AtFlags::empty());
            Fault(format!("{}: {error}", self.path().display()))
        })
    }

    /// Removes the copy and a temporary file beside it. When neither is there (a host that never
    /// had a copy: one of the Debian or RPM package, say) nothing is looked at or done; otherwise
    /// the directories are checked as for [`Self::install`] first.
    pub(super) fn remove(&self) -> Result<(), Fault> {
        let (_, name) = parts(self.at);
        let temp = self.temp();
        let absent = |path: &Path| {
            fs::symlink_metadata(path).is_err_and(|error| error.kind() == ErrorKind::NotFound)
        };
        if absent(&self.path()) && absent(&self.path().with_file_name(&temp)) {
            return Ok(());
        }
        let dir = self.dir(false)?;
        for file in [temp.as_str(), name] {
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
