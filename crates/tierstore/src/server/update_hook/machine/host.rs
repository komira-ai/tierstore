//! What the host says of itself: the node's configuration file and the capabilities of a staged
//! release.
//!
//! The files are read under a root (`/` on a host) so that a test can give the host a root of
//! its own.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::RELEASE_BINARY;
use super::super::ports::{Host, PortError, PortKind};
use super::Fault;
use super::runner::{Call, Runner, checked, run};

const KIND: PortKind = PortKind::Host;
/// The most of a configuration file that is read; a larger one is an error.
pub const CONFIG_MAX: u64 = 1 << 20;
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

/// `path` below `root`: an absolute path is taken as if the root were `/`.
fn under(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix("/").unwrap_or(path))
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

    /// Whether `path` is what may be run or run from: of the kind `want` (not a link), owned by
    /// the release owner and not writable by a group or others.
    fn check(&self, path: &Path, want: fn(&fs::FileType) -> bool, noun: &str) -> Result<(), Fault> {
        let meta = fs::symlink_metadata(path)
            .map_err(|error| Fault(format!("{}: {error}", path.display())))?;
        if !want(&meta.file_type()) {
            return Err(format!("{} is not a {noun}", path.display()).into());
        }
        if meta.uid() != self.release_owner {
            return Err(format!(
                "{} is owned by user {}, not {}",
                path.display(),
                meta.uid(),
                self.release_owner
            )
            .into());
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!("{} can be written by a group or others", path.display()).into());
        }
        Ok(())
    }

    /// The binary of `release`, checked, and the directory it is in, checked too: neither a link.
    fn binary_of(&self, release: &Path) -> Result<PathBuf, Fault> {
        let binary = under(&self.root, &release.join(RELEASE_BINARY));
        let bin_dir = binary.parent().unwrap_or(&binary);
        self.check(bin_dir, fs::FileType::is_dir, "directory")?;
        self.check(&binary, fs::FileType::is_file, "regular file")?;
        Ok(binary)
    }

    fn read_config(&self, path: &Path) -> Result<Option<String>, Fault> {
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut text = String::new();
        let read = (&mut file).take(CONFIG_MAX + 1).read_to_string(&mut text)?;
        if read as u64 > CONFIG_MAX {
            return Err(format!("is larger than {CONFIG_MAX} bytes").into());
        }
        Ok(Some(text))
    }
}

impl Host for SystemHost<'_> {
    /// Reads the node's configuration file `path` under the root: at most [`CONFIG_MAX`] bytes of
    /// UTF-8. A file that is not there is `None`.
    fn config(&self, path: &Path) -> Result<Option<String>, PortError> {
        let at = under(&self.root, path);
        self.read_config(&at)
            .map_err(|fault| PortError::failed(KIND, format!("read {}: {fault}", at.display())))
    }

    /// Runs `<release>/bin/tierstore --capabilities`: exit 0 and one capability per line. Exit 2,
    /// the status of an unknown flag, means a release that predates the flag: no capabilities.
    /// Any other status, a binary that cannot be started and one that does not end are errors.
    ///
    /// The binary runs as the user the hook runs as; nothing here changes that. It must be the
    /// verified staged tree that the updater names in `updater.staged_release`, and so the
    /// binary and its `bin` directory are refused unless they are not links, are owned by the
    /// release owner (root, unless set otherwise) and no group or others can write them. Its
    /// environment is empty but for a fixed `PATH` and `LC_ALL=C`.
    fn capabilities(&self, release: &Path) -> Result<Vec<String>, PortError> {
        let binary = self
            .binary_of(release)
            .map_err(|fault| PortError::failed(KIND, fault.to_string()))?;
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
