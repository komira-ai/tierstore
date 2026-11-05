//! The start markers of the two instances: a file per instance in the service's state
//! directory, which the template's `ConditionPathExists=` names.
//!
//! A marker is a regular file; the hook looks at its name with `lstat` and never follows a link.
//! It is laid down with `O_NOFOLLOW`, and the directory is synced after a marker is made or
//! removed, so that a marker the hook reported done is there after a power loss.

use std::fs::{self, DirBuilder, File};
use std::io::{self, ErrorKind};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, open};

use super::super::marker_path;
use super::super::ports::{Instance, Markers, PortError, PortKind};
use super::Fault;

const KIND: PortKind = PortKind::Markers;
/// The mode of the directory when the hook makes it.
const DIR_MODE: u32 = 0o700;
/// The mode of a marker.
const FILE_MODE: Mode = Mode::from_bits_truncate(0o644);

/// The markers in the directory `dir`.
pub struct MarkerFiles {
    dir: PathBuf,
}

impl MarkerFiles {
    /// The markers in `dir`: [`MARKER_DIR`](super::super::MARKER_DIR) under the host's root.
    #[must_use]
    pub fn new(dir: PathBuf) -> MarkerFiles {
        MarkerFiles { dir }
    }

    /// The path of the instance's marker: the name [`marker_path`] gives, in this directory.
    fn path(&self, instance: Instance) -> PathBuf {
        self.dir
            .join(marker_path(instance).file_name().unwrap_or_default())
    }

    /// Makes the directory when it is absent.
    fn make_dir(&self) -> Result<(), Fault> {
        match DirBuilder::new().mode(DIR_MODE).create(&self.dir) {
            Err(error) if error.kind() != ErrorKind::AlreadyExists => Err(error.into()),
            _ => Ok(()),
        }
    }

    /// Syncs the directory.
    fn sync_dir(&self) -> io::Result<()> {
        File::open(&self.dir).and_then(|dir| dir.sync_all())
    }

    fn lay(&self, path: &Path) -> Result<(), Fault> {
        self.make_dir()?;
        let fd = open(
            path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            FILE_MODE,
        )
        .map_err(io::Error::from)?;
        File::from(fd)
            .sync_all()
            .and_then(|()| self.sync_dir())
            .map_err(Fault::from)
    }

    fn remove(&self, path: &Path) -> Result<(), Fault> {
        match fs::remove_file(path) {
            Ok(()) => self.sync_dir().map_err(Fault::from),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn failed(what: &str, path: &Path, fault: &Fault) -> PortError {
    PortError::failed(KIND, format!("{what} {}: {fault}", path.display()))
}

impl Markers for MarkerFiles {
    fn present(&self, instance: Instance) -> Result<bool, PortError> {
        let path = self.path(instance);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => Ok(true),
            Ok(_) => Err(failed("look at", &path, &"is not a regular file".into())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(failed("look at", &path, &error.into())),
        }
    }

    fn set(&self, instance: Instance) -> Result<(), PortError> {
        let path = self.path(instance);
        self.lay(&path)
            .map_err(|fault| failed("make", &path, &fault))
    }

    fn clear(&self, instance: Instance) -> Result<(), PortError> {
        let path = self.path(instance);
        self.remove(&path)
            .map_err(|fault| failed("remove", &path, &fault))
    }
}
