//! The slot links: `<state_dir>/slots/<port>`, a symbolic link to a staged release.
//!
//! The `slots` directory is opened relative to the state directory's descriptor (the one the
//! state port checked), without following a link, and every link is made, read and removed
//! relative to it.

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, Mode, OFlags, fsync, mkdirat, openat, readlinkat, renameat, symlinkat, unlinkat,
};
use rustix::io::Errno;

use super::super::ports::{PortError, PortKind, Slots};
use super::StateFiles;

const KIND: PortKind = PortKind::Slots;
const DIR: &str = "slots";

/// Why the `slots` directory could not be had.
enum DirError {
    /// The state directory is not as it must be.
    State(PortError),
    /// The `slots` directory could not be opened or made.
    Slots(Errno),
}

/// The links of one state directory.
pub struct SlotLinks<'a> {
    state: &'a StateFiles,
}

impl<'a> SlotLinks<'a> {
    /// The links under `<state_dir>/slots` of `state`.
    #[must_use]
    pub fn new(state: &'a StateFiles) -> SlotLinks<'a> {
        SlotLinks { state }
    }

    fn link(&self, port: u16) -> PathBuf {
        self.state.path().join(DIR).join(port.to_string())
    }

    /// The `slots` directory, opened relative to the state directory without following a link;
    /// with `create` it is made first when it is absent.
    fn open_dir(&self, create: bool) -> Result<OwnedFd, DirError> {
        let state = self.state.dir_fd().map_err(DirError::State)?;
        if create {
            match mkdirat(state, DIR, Mode::from_bits_truncate(0o750)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(errno) => return Err(DirError::Slots(errno)),
            }
        }
        openat(
            state,
            DIR,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(DirError::Slots)
    }

    /// The new link is made beside the old under a name of its own, so the rename replaces the
    /// old in one step, and the directory is synced after it.
    fn place(&self, dir: &OwnedFd, port: u16, release: &Path, temp: &str) -> io::Result<()> {
        // A link left by a crash between the two calls below; absent in the usual case.
        let _ = unlinkat(dir, temp, AtFlags::empty());
        symlinkat(release, dir, temp)?;
        renameat(dir, temp, dir, port.to_string().as_str())?;
        fsync(dir)?;
        Ok(())
    }
}

impl SlotLinks<'_> {
    /// The `slots` directory for a read or a removal: `None` when there is none, an error
    /// worded for `verb` when it cannot be opened.
    fn existing_dir(&self, verb: &str, port: u16) -> Result<Option<OwnedFd>, PortError> {
        match self.open_dir(false) {
            Ok(dir) => Ok(Some(dir)),
            Err(DirError::State(error)) => Err(error),
            Err(DirError::Slots(Errno::NOENT)) => Ok(None),
            Err(DirError::Slots(errno)) => Err(PortError::failed(
                KIND,
                format!("{verb} {}: {errno}", self.link(port).display()),
            )),
        }
    }

    /// Where the link of `port` in `dir` points; `None` when there is none.
    fn read_in(&self, dir: &OwnedFd, port: u16) -> Result<Option<PathBuf>, PortError> {
        let path = self.link(port);
        match readlinkat(dir, port.to_string().as_str(), Vec::new()) {
            Ok(target) => Ok(Some(PathBuf::from(OsStr::from_bytes(target.as_bytes())))),
            Err(Errno::NOENT) => Ok(None),
            // `readlink` of a path that is not a link: EINVAL.
            Err(Errno::INVAL) => Err(PortError::failed(
                KIND,
                format!("{} exists and is not a symbolic link", path.display()),
            )),
            Err(errno) => Err(PortError::failed(
                KIND,
                format!("read {}: {errno}", path.display()),
            )),
        }
    }
}

impl Slots for SlotLinks<'_> {
    fn target(&self, port: u16) -> Result<Option<PathBuf>, PortError> {
        match self.existing_dir("read", port)? {
            Some(dir) => self.read_in(&dir, port),
            None => Ok(None),
        }
    }

    fn point(&self, port: u16, release: &Path) -> Result<(), PortError> {
        let temp = format!(".{port}.tmp");
        let fail = |error: &dyn std::fmt::Display| {
            PortError::failed(
                KIND,
                format!(
                    "link {} to {}: {error}",
                    self.link(port).display(),
                    release.display()
                ),
            )
        };
        let dir = match self.open_dir(true) {
            Ok(dir) => dir,
            Err(DirError::State(error)) => return Err(error),
            Err(DirError::Slots(errno)) => return Err(fail(&errno)),
        };
        self.place(&dir, port, release, &temp).map_err(|error| {
            let _ = unlinkat(&dir, temp.as_str(), AtFlags::empty());
            fail(&error)
        })
    }

    fn remove(&self, port: u16) -> Result<(), PortError> {
        let Some(dir) = self.existing_dir("remove", port)? else {
            return Ok(());
        };
        if self.read_in(&dir, port)?.is_none() {
            return Ok(());
        }
        unlinkat(&dir, port.to_string().as_str(), AtFlags::empty()).map_err(|errno| {
            PortError::failed(
                KIND,
                format!("remove {}: {errno}", self.link(port).display()),
            )
        })
    }
}
