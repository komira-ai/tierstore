//! What the host says of itself: the declaration, the boot, the connection tracking and the
//! capabilities of a staged release.
//!
//! The files are read under a root (`/` on a host) so that a test can give the host a root of
//! its own.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, open, openat, statat};
use rustix::io::Errno;

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

/// Why a directory above the declaration was refused.
enum Parent {
    /// It is one a non-root account could have changed.
    Untrusted(String),
    /// It could not be read.
    Failed(String),
}

/// The host, read under `root`.
pub struct SystemHost<'a> {
    root: PathBuf,
    runner: &'a dyn Runner,
    timeout: Duration,
    release_owner: u32,
    declaration_owner: u32,
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
            declaration_owner: 0,
        }
    }

    /// The same, with `uid` as the user that must own the binary of a release (root, 0, unless
    /// this says otherwise; tests say who they are).
    #[must_use]
    pub fn with_release_owner(mut self, uid: u32) -> SystemHost<'a> {
        self.release_owner = uid;
        self
    }

    /// The same, with `uid` as the user that must own the declaration (root, 0, unless this says
    /// otherwise; tests say who they are).
    #[must_use]
    pub fn with_declaration_owner(mut self, uid: u32) -> SystemHost<'a> {
        self.declaration_owner = uid;
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

    /// The directory `name` of `parent`, opened without following a link and checked: owned by
    /// the declaration owner, no group or other write bit. `at` is its path, for messages.
    fn open_parent(
        &self,
        parent: BorrowedFd<'_>,
        name: &OsStr,
        at: &Path,
    ) -> Result<OwnedFd, Parent> {
        let shown = at.display();
        let fd = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| {
            let link = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Symlink);
            if link {
                Parent::Untrusted(format!("{shown} is a symbolic link"))
            } else {
                Parent::Failed(format!("{shown}: {errno}"))
            }
        })?;
        let stat = fstat(&fd).map_err(|errno| Parent::Failed(errno.to_string()))?;
        if stat.st_uid != self.declaration_owner {
            return Err(Parent::Untrusted(format!(
                "{shown} is owned by user {}, not {}",
                stat.st_uid, self.declaration_owner
            )));
        }
        if stat.st_mode & 0o022 != 0 {
            return Err(Parent::Untrusted(format!(
                "{shown} can be written by a group or others"
            )));
        }
        Ok(fd)
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
    /// Reads the declaration, which names what a privileged account acts on, and so must be one
    /// that only the declaration owner (root, unless set otherwise) could have written. Each
    /// directory above it under the root (`/etc`, `/etc/updater.d`) is opened relative to the
    /// one before with `O_DIRECTORY | O_NOFOLLOW` and must be owned by that owner with no group
    /// or other write bit; then the file is opened relative to the last of them with
    /// `O_NOFOLLOW` (a link is refused) and the opened descriptor, not the path, must be a
    /// regular file of that owner with no group or other write bit. The root itself is the
    /// host's own and is not checked.
    fn declaration(&self) -> Result<String, PortError> {
        let path = self.root.join(DECLARATION_PATH.trim_start_matches('/'));
        let shown = path.display();
        let untrusted = |why: String| {
            PortError::failed(
                KIND,
                format!("the declaration {shown} is not trusted: {why}"),
            )
        };
        let declared = Path::new(DECLARATION_PATH);
        let mut at = self.root.clone();
        let mut dir = open(
            self.root.as_path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| {
            PortError::failed(KIND, format!("the host root {}: {errno}", at.display()))
        })?;
        for part in declared.parent().into_iter().flat_map(Path::components) {
            let Component::Normal(name) = part else {
                continue;
            };
            at.push(name);
            dir = self
                .open_parent(dir.as_fd(), name, &at)
                .map_err(|why| match why {
                    Parent::Untrusted(why) => untrusted(why),
                    Parent::Failed(text) => PortError::failed(
                        KIND,
                        format!("the declaration {shown} cannot be read: {text}"),
                    ),
                })?;
        }
        let file = declared.file_name().unwrap_or_default();
        let fd = openat(
            dir.as_fd(),
            file,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| {
            let why = if errno == Errno::LOOP {
                "is not trusted: it is a symbolic link".to_owned()
            } else {
                format!("cannot be read: {errno}")
            };
            PortError::failed(KIND, format!("the declaration {shown} {why}"))
        })?;
        let mut file = File::from(fd);
        let meta = file
            .metadata()
            .map_err(|error| PortError::failed(KIND, format!("read {shown}: {error}")))?;
        if !meta.file_type().is_file() {
            return Err(untrusted("it is not a regular file".to_owned()));
        }
        if meta.uid() != self.declaration_owner {
            return Err(untrusted(format!(
                "it is owned by user {}, not {}",
                meta.uid(),
                self.declaration_owner
            )));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(untrusted(
                "it can be written by a group or others".to_owned(),
            ));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|error| PortError::failed(KIND, format!("read {shown}: {error}")))?;
        Ok(text)
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
