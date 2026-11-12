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

use rustix::process::geteuid;

use super::super::ports::{Host, PortError, PortKind};
use super::super::{PACKAGE_RELEASE_BINARY, RELEASE_BINARY, SERVICE_ACCOUNT};
use super::Fault;
use super::runner::{Call, Runner, checked, run};

const KIND: PortKind = PortKind::Host;
/// The most of a configuration file that is read; a larger one is an error.
pub const CONFIG_MAX: u64 = 1 << 20;
/// The exit status of a binary that does not know `--capabilities`: the usage error of its
/// argument parser.
const UNKNOWN_FLAG: i32 = 2;
/// The exit status of a release that refuses the configuration it was asked to check.
const CONFIG_REFUSED: i32 = 2;

/// The user and group ids of the account `name` in the text of a password file: the third and
/// fourth fields of its line.
fn account_ids(passwd: &str, name: &str) -> Option<(u32, u32)> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != name {
            return None;
        }
        let uid = fields.nth(1)?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        Some((uid, gid))
    })
}

/// The host, read under `root`.
pub struct SystemHost<'a> {
    root: PathBuf,
    runner: &'a dyn Runner,
    timeout: Duration,
    release_owner: u32,
    hook_uid: u32,
    account: String,
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
            hook_uid: geteuid().as_raw(),
            account: SERVICE_ACCOUNT.to_owned(),
        }
    }

    /// The same, with `uid` as the user the hook runs as (the user of the process unless this
    /// says otherwise; tests say who they pretend to be). Only a hook that runs as root (0)
    /// runs a release's check of the configuration as the service account.
    #[must_use]
    pub fn with_hook_user(mut self, uid: u32) -> SystemHost<'a> {
        self.hook_uid = uid;
        self
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
    ///
    /// An archive release has it at [`RELEASE_BINARY`], a release of the Debian or RPM package at
    /// [`PACKAGE_RELEASE_BINARY`]. The archive's path is taken when anything is at it or at its
    /// `bin` (so a link, a directory or a missing file there is refused, never passed over), the
    /// package's when nothing is at either and something is at the package's path, and neither is
    /// an error naming both.
    fn binary_of(&self, release: &Path) -> Result<PathBuf, Fault> {
        let archive = under(&self.root, &release.join(RELEASE_BINARY));
        let package = under(&self.root, &release.join(PACKAGE_RELEASE_BINARY));
        let there = |path: &Path| fs::symlink_metadata(path).is_ok();
        let archive_there = there(&archive) || archive.parent().is_some_and(there);
        let binary = match (archive_there, there(&package)) {
            (false, true) => package,
            (false, false) => {
                return Err(Fault(format!(
                    "{} is not there, nor {}",
                    archive.display(),
                    package.display()
                )));
            }
            (true, _) => archive,
        };
        let bin_dir = binary.parent().unwrap_or(&binary);
        self.check(bin_dir, fs::FileType::is_dir, "directory")?;
        self.check(&binary, fs::FileType::is_file, "regular file")?;
        Ok(binary)
    }

    /// The user and group ids of the service account, from `etc/passwd` under the root; `None`
    /// when the hook does not run as root, whose checks run as the hook itself.
    fn run_as(&self) -> Result<Option<(u32, u32)>, Fault> {
        if self.hook_uid != 0 {
            return Ok(None);
        }
        let path = self.root.join("etc/passwd");
        let text = self
            .read_config(&path)
            .map_err(|fault| Fault(format!("{}: {fault}", path.display())))?
            .unwrap_or_default();
        account_ids(&text, &self.account).map(Some).ok_or_else(|| {
            Fault(format!(
                "{} has no account {}",
                path.display(),
                self.account
            ))
        })
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

    /// Runs `<release>/bin/tierstore --capabilities` (or, for a release of the Debian or RPM
    /// package, its `tree/usr/libexec/tierstore/tierstore`): exit 0 and one capability per line. Exit 2,
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

    /// Runs `<release>/bin/tierstore --config <config> --check-config` (or the package's binary,
    /// as for `--capabilities`): exit 0 is a configuration it accepts, exit 2 one it refuses (its
    /// standard error is the line). Any other status, a binary that cannot be started and one
    /// that does not end are errors, so that a release that cannot say is not taken for one that
    /// refuses.
    ///
    /// The binary is held to the same checks as for `--capabilities`. When the hook runs as
    /// root it runs as the service account, found in `etc/passwd` under the root (an account
    /// that is not there is an error): the check refuses key files the user it runs as does not
    /// own, and the unit does not run as root. A hook that is not root runs it as itself.
    fn check_config(&self, release: &Path, config: &Path) -> Result<Option<String>, PortError> {
        let binary = self
            .binary_of(release)
            .map_err(|fault| PortError::failed(KIND, fault.to_string()))?;
        let run_as = self
            .run_as()
            .map_err(|fault| PortError::failed(KIND, fault.to_string()))?;
        let program = binary.to_string_lossy();
        let file = under(&self.root, config);
        let file = file.to_string_lossy();
        let mut call = Call::new(
            &program,
            &["--config", &file, "--check-config"],
            self.timeout,
        );
        if let Some((uid, gid)) = run_as {
            call = call.running_as(uid, gid);
        }
        let output = run(self.runner, &call, KIND)?;
        if output.code == Some(CONFIG_REFUSED) {
            return Ok(Some(output.stderr_text()));
        }
        checked(output, &call, KIND)?;
        Ok(None)
    }
}
