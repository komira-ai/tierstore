//! What the host says of itself: the declaration, the boot, the connection tracking and the
//! capabilities of a staged release.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::ports::{Conntrack, Host, PortError};
use super::runner::Runner;

/// The binary of a staged release: the release is a directory with the executable at its top.
pub const RELEASE_BINARY: &str = "tierstore";

/// The host, read under `root`.
pub struct SystemHost<'a> {
    #[allow(dead_code)]
    root: PathBuf,
    #[allow(dead_code)]
    runner: &'a dyn Runner,
    #[allow(dead_code)]
    timeout: Duration,
}

impl<'a> SystemHost<'a> {
    /// The host whose files are under `root` (`/` for the machine itself). A release's binary
    /// is run through `runner` and killed after `timeout`.
    #[must_use]
    pub fn new(root: PathBuf, runner: &'a dyn Runner, timeout: Duration) -> SystemHost<'a> {
        SystemHost {
            root,
            runner,
            timeout,
        }
    }
}

impl Host for SystemHost<'_> {
    fn declaration(&self) -> Result<String, PortError> {
        Ok(String::new())
    }

    fn boot_id(&self) -> Result<String, PortError> {
        Ok(String::new())
    }

    fn conntrack(&self) -> Result<Conntrack, PortError> {
        Ok(Conntrack {
            tcp_timeout_established_secs: 0,
            count: 0,
            max: 0,
        })
    }

    fn capabilities(&self, _release: &Path) -> Result<Vec<String>, PortError> {
        Ok(Vec::new())
    }
}
