//! The slot links: `<state_dir>/slots/<port>`, a symbolic link to a staged release.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::super::ports::{PortError, PortKind, Slots};
use super::sync_dir;

const KIND: PortKind = PortKind::Slots;

/// The links of one state directory.
pub struct SlotLinks {
    dir: PathBuf,
}

impl SlotLinks {
    /// The links under `<state_dir>/slots`.
    #[must_use]
    pub fn new(state_dir: &Path) -> SlotLinks {
        SlotLinks {
            dir: state_dir.join("slots"),
        }
    }

    fn link(&self, port: u16) -> PathBuf {
        self.dir.join(port.to_string())
    }

    /// The new link is made beside the old under a name of its own, so the rename replaces the
    /// old in one step, and the directory is synced after it.
    fn place(&self, port: u16, release: &Path, temp: &Path) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        // A link left by a crash between the two calls below; absent in the usual case.
        let _ = fs::remove_file(temp);
        symlink(release, temp)?;
        fs::rename(temp, self.link(port))?;
        sync_dir(&self.dir)
    }
}

impl Slots for SlotLinks {
    fn target(&self, port: u16) -> Result<Option<PathBuf>, PortError> {
        let path = self.link(port);
        match fs::read_link(&path) {
            Ok(target) => Ok(Some(target)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            // `readlink` of a path that is not a link: EINVAL.
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => Err(PortError::failed(
                KIND,
                format!("{} exists and is not a symbolic link", path.display()),
            )),
            Err(error) => Err(PortError::failed(
                KIND,
                format!("read {}: {error}", path.display()),
            )),
        }
    }

    fn point(&self, port: u16, release: &Path) -> Result<(), PortError> {
        let temp = self.dir.join(format!(".{port}.tmp"));
        self.place(port, release, &temp).map_err(|error| {
            let _ = fs::remove_file(&temp);
            PortError::failed(
                KIND,
                format!(
                    "link {} to {}: {error}",
                    self.link(port).display(),
                    release.display()
                ),
            )
        })
    }

    fn remove(&self, port: u16) -> Result<(), PortError> {
        if self.target(port)?.is_none() {
            return Ok(());
        }
        let path = self.link(port);
        fs::remove_file(&path)
            .map_err(|error| PortError::failed(KIND, format!("remove {}: {error}", path.display())))
    }
}
