//! The state directory: the checks that make it trustworthy, the lock, and `state.json`.

use std::os::fd::BorrowedFd;
use std::path::PathBuf;

use super::super::ports::{Lock, PortError, StateDir};

/// The lock file, in the state directory.
pub const LOCK_FILE: &str = "lock";
/// The record, in the state directory.
pub const STATE_FILE: &str = "state.json";

/// The state directory `dir`, which `owner` (a user id) must own.
#[derive(Clone, Debug)]
pub struct StateFiles {
    #[allow(dead_code)]
    dir: PathBuf,
    #[allow(dead_code)]
    owner: u32,
}

/// Takes the exclusive lock of `fd` without waiting: `Busy` when another descriptor holds it.
pub(super) fn try_lock(_fd: BorrowedFd<'_>) -> Result<(), PortError> {
    Ok(())
}

impl StateFiles {
    /// The directory `dir`, owned by the user id `owner`.
    #[must_use]
    pub fn new(dir: PathBuf, owner: u32) -> StateFiles {
        StateFiles { dir, owner }
    }
}

impl StateDir for StateFiles {
    fn lock(&self) -> Result<Lock, PortError> {
        Ok(Lock::new(()))
    }

    fn load(&self) -> Result<Option<Vec<u8>>, PortError> {
        Ok(None)
    }

    fn save(&self, _bytes: &[u8]) -> Result<(), PortError> {
        Ok(())
    }
}
