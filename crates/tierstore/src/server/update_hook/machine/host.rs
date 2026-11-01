//! What the host says of itself: the declaration, the boot, the connection tracking and the
//! capabilities of a staged release.
//!
//! The files are read under a root (`/` on a host) so that a test can give the host a root of
//! its own.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::declaration::DECLARATION_PATH;
use super::super::ports::{Conntrack, Host, PortError, PortKind};
use super::Fault;
use super::runner::{Call, Runner, checked, run};

const KIND: PortKind = PortKind::Host;
/// The binary of a staged release: the release is a directory with the executable at its top.
pub const RELEASE_BINARY: &str = "tierstore";
const BOOT_ID: &str = "proc/sys/kernel/random/boot_id";
const CONNTRACK: &str = "proc/sys/net/netfilter";
/// The exit status of a binary that does not know `--capabilities`: the usage error of its
/// argument parser.
const UNKNOWN_FLAG: i32 = 2;

/// The host, read under `root`.
pub struct SystemHost<'a> {
    root: PathBuf,
    runner: &'a dyn Runner,
    timeout: Duration,
    release_owner: u32,
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
            release_owner: 0,
        }
    }

    /// The same, with `uid` as the user that must own the binary of a release (root, 0, unless
    /// this says otherwise; tests say who they are).
    #[must_use]
    pub fn with_release_owner(mut self, uid: u32) -> SystemHost<'a> {
        self.release_owner = uid;
        self
    }

    /// Whether `binary` is what may be run: a regular file (not a link) owned by the release
    /// owner and not writable by a group or others.
    fn check_binary(&self, binary: &Path) -> Result<(), Fault> {
        let meta = fs::symlink_metadata(binary)?;
        if !meta.file_type().is_file() {
            return Err("is not a regular file".into());
        }
        if meta.uid() != self.release_owner {
            return Err(format!(
                "is owned by user {}, not {}",
                meta.uid(),
                self.release_owner
            )
            .into());
        }
        if meta.mode() & 0o022 != 0 {
            return Err("can be written by a group or others".into());
        }
        Ok(())
    }

    fn read(&self, relative: &str) -> Result<String, PortError> {
        let path = self.root.join(relative.trim_start_matches('/'));
        std::fs::read_to_string(&path)
            .map_err(|error| PortError::failed(KIND, format!("read {}: {error}", path.display())))
    }

    fn conntrack_value(&self, name: &str) -> Result<u64, PortError> {
        let text = self.read(&format!("{CONNTRACK}/{name}"))?;
        text.trim()
            .parse()
            .map_err(|error| PortError::failed(KIND, format!("{name} is not a number: {error}")))
    }
}

impl Host for SystemHost<'_> {
    fn declaration(&self) -> Result<String, PortError> {
        self.read(DECLARATION_PATH)
    }

    fn boot_id(&self) -> Result<String, PortError> {
        Ok(self.read(BOOT_ID)?.trim().to_owned())
    }

    fn conntrack(&self) -> Result<Conntrack, PortError> {
        Ok(Conntrack {
            tcp_timeout_established_secs: self
                .conntrack_value("nf_conntrack_tcp_timeout_established")?,
            count: self.conntrack_value("nf_conntrack_count")?,
            max: self.conntrack_value("nf_conntrack_max")?,
        })
    }

    /// Runs `<release>/tierstore --capabilities`: exit 0 and one capability per line. Exit 2,
    /// the status of an unknown flag, means a release that predates the flag: no capabilities.
    /// Any other status, a binary that cannot be started and one that does not end are errors.
    ///
    /// The binary runs as the user the hook runs as; nothing here changes that. It must be the
    /// verified staged tree that the updater names in `updater.staged_release`, and so the
    /// binary is refused unless it is a regular file owned by the release owner (root, unless
    /// set otherwise) that no group or others can write. Its environment is empty but for a
    /// fixed `PATH` and `LC_ALL=C`.
    fn capabilities(&self, release: &Path) -> Result<Vec<String>, PortError> {
        let binary = release.join(RELEASE_BINARY);
        self.check_binary(&binary)
            .map_err(|fault| PortError::failed(KIND, format!("{}: {fault}", binary.display())))?;
        let program = binary.to_string_lossy();
        let call = Call::new(&program, &["--capabilities"], self.timeout);
        let output = run(self.runner, &call, KIND)?;
        if output.code == Some(UNKNOWN_FLAG) {
            return Ok(Vec::new());
        }
        let output = checked(output, &call, KIND)?;
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }
}
