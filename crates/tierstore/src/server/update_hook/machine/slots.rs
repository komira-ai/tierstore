//! The slot links: `<state_dir>/slots/<port>`, a symbolic link to a staged release.

use std::path::{Path, PathBuf};

use super::super::ports::{PortError, Slots};

/// The links of one state directory.
#[derive(Clone, Debug)]
pub struct SlotLinks {
    #[allow(dead_code)]
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
}

impl Slots for SlotLinks {
    fn target(&self, _port: u16) -> Result<Option<PathBuf>, PortError> {
        Ok(None)
    }

    fn point(&self, _port: u16, _release: &Path) -> Result<(), PortError> {
        Ok(())
    }

    fn remove(&self, _port: u16) -> Result<(), PortError> {
        Ok(())
    }
}
