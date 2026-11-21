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
use super::super::{
    MACOS_SERVICE_ACCOUNT, PACKAGE_RELEASE_BINARY, RELEASE_BINARY, SERVICE_ACCOUNT, SERVICE_BINARY,
};
use super::Fault;
use super::runner::{Call, Runner, checked, run};
use super::service_binary::ServiceBinary;

const KIND: PortKind = PortKind::Host;
/// The most of a configuration file that is read; a larger one is an error.
pub const CONFIG_MAX: u64 = 1 << 20;
/// The exit status of a binary that does not know `--capabilities`: the usage error of its
/// argument parser.
const UNKNOWN_FLAG: i32 = 2;
/// The exit status of a release that refuses the configuration it was asked to check.
const CONFIG_REFUSED: i32 = 2;

/// Where macOS has `dscl`, the command line of the directory service.
pub(crate) const DSCL_PROGRAM: &str = "/usr/bin/dscl";
/// Where macOS has `sudo`, through which a root hook on a Mac runs a program as the service
/// account (see [`SystemHost::check_config`]).
pub(crate) const SUDO_PROGRAM: &str = "/usr/bin/sudo";

/// Where a hook that runs as root finds the user and group ids of the service account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Accounts {
    /// `etc/passwd` under the root: a Linux node, whose unit runs as [`SERVICE_ACCOUNT`].
    PasswdFile,
    /// The local directory service of a Mac, through `dscl`: its job runs as
    /// [`MACOS_SERVICE_ACCOUNT`], which the package's postinstall makes there and not in
    /// `/etc/passwd`.
    DirectoryService,
}

/// The arguments of `dscl` that read the user id and the primary group id of the user `name`
/// from the local directory (`.`, where the package's postinstall makes the account).
fn dscl_args(name: &str) -> [String; 5] {
    [
        ".".to_owned(),
        "-read".to_owned(),
        format!("/Users/{name}"),
        "UniqueID".to_owned(),
        "PrimaryGroupID".to_owned(),
    ]
}

/// The value of the attribute `key` in what `dscl -read` printed: `key: value` on one line, or
/// `key:` with the value on the next line (as `dscl` prints a long value).
fn dscl_value<'t>(text: &'t str, key: &str) -> Option<&'t str> {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            continue;
        };
        let rest = rest.trim();
        return Some(if rest.is_empty() {
            lines.next()?.trim()
        } else {
            rest
        });
    }
    None
}

/// The user and group ids in what `dscl . -read /Users/<name> UniqueID PrimaryGroupID` printed.
fn dscl_ids(text: &str) -> Option<(u32, u32)> {
    let uid = dscl_value(text, "UniqueID")?.parse().ok()?;
    let gid = dscl_value(text, "PrimaryGroupID")?.parse().ok()?;
    Some((uid, gid))
}

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
    accounts: Accounts,
    /// The one binary every release is answered by, when set: the config hook's.
    binary: Option<PathBuf>,
    /// A directory on the host that only root may enter, when set: a configuration file under
    /// it is read by the hook and handed over to a check ([`handed_over`]).
    root_only: Option<PathBuf>,
}

/// A configuration file the hook read, handed to a check that cannot reach its path.
struct Handed {
    /// On Linux, a file in memory holding the text, which the child inherits.
    #[cfg(target_os = "linux")]
    _file: fs::File,
    /// The path the check reads it at.
    path: String,
    /// What the check is given on its standard input, if that is the way.
    stdin: Option<Vec<u8>>,
}

/// On Linux, `text` in a file in memory (`memfd_create`) that the child of the next call
/// inherits (the descriptor alone is not closed on exec, and is at least 3), read by the check
/// at `/proc/self/fd/<n>`. A file in memory is open to every user (the kernel makes it mode
/// 0777), so the check reads it after it has taken the service account's ids; standard input
/// would not do, as a reopen of `/dev/stdin` under the account's ids is refused for a pipe that
/// root made. The descriptor reaches only that child: the hook starts one program at a time.
#[cfg(target_os = "linux")]
fn handed_over(text: &str) -> Result<Handed, Fault> {
    use rustix::fs::{MemfdFlags, memfd_create};
    use rustix::io::{FdFlags, fcntl_dupfd_cloexec, fcntl_setfd};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let fault = |error: &dyn std::fmt::Display| {
        Fault(format!("a file in memory for the configuration: {error}"))
    };
    let fd = memfd_create("tierstore-config", MemfdFlags::CLOEXEC).map_err(|e| fault(&e))?;
    let mut file = fs::File::from(fcntl_dupfd_cloexec(&fd, 3).map_err(|e| fault(&e))?);
    file.write_all(text.as_bytes()).map_err(|e| fault(&e))?;
    fcntl_setfd(&file, FdFlags::empty()).map_err(|e| fault(&e))?;
    Ok(Handed {
        path: format!("/proc/self/fd/{}", file.as_raw_fd()),
        _file: file,
        stdin: None,
    })
}

/// Elsewhere (a Mac), `text` on the check's standard input, read at `/dev/stdin`: there that
/// is the descriptor itself, whoever the check runs as, and `sudo` hands its input on.
#[cfg(not(target_os = "linux"))]
fn handed_over(text: &str) -> Result<Handed, Fault> {
    Ok(Handed {
        path: "/dev/stdin".to_owned(),
        stdin: Some(text.as_bytes().to_vec()),
    })
}

/// The binary of a check as the child executes it: on Linux an open file of the hook's, which
/// the child reaches as `/proc/self/fd/<n>` without searching the directories above the file.
struct Opened {
    /// The open binary, held until the child has executed it.
    #[cfg(target_os = "linux")]
    _file: fs::File,
    /// The path the child executes.
    exec: String,
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
            accounts: Accounts::PasswdFile,
            binary: None,
            root_only: None,
        }
    }

    /// The same, with `binary` (an absolute path on the host) as the binary of every release:
    /// the config hook asks the binary that serves, whatever release a step names. It is checked
    /// as a release's binary is.
    #[must_use]
    pub fn with_binary(mut self, binary: PathBuf) -> SystemHost<'a> {
        self.binary = Some(binary);
        self
    }

    /// The same, with `dir` (an absolute path on the host) as a directory that only root may
    /// enter: the config hook's staged package, whose copy of the configuration file the service
    /// account cannot read. A check of a file under it is handed the file's text, read by the
    /// hook ([`handed_over`]).
    #[must_use]
    pub(crate) fn with_root_only(mut self, dir: PathBuf) -> SystemHost<'a> {
        self.root_only = Some(dir);
        self
    }

    /// The same, with `uid` as the user the hook runs as (the user of the process unless this
    /// says otherwise; tests say who they pretend to be). Only a hook that runs as root (0)
    /// runs a release's check of the configuration as the service account.
    #[must_use]
    pub fn with_hook_user(mut self, uid: u32) -> SystemHost<'a> {
        self.hook_uid = uid;
        self
    }

    /// The same, for a Mac: a root hook runs the checks as [`MACOS_SERVICE_ACCOUNT`], the
    /// launchd job's `UserName`, whose ids it asks the directory service for with
    /// `dscl . -read /Users/_tierstore UniqueID PrimaryGroupID` (through the runner, within the
    /// host's time limit). Without this the account is [`SERVICE_ACCOUNT`] in `etc/passwd`.
    #[must_use]
    pub(crate) fn with_directory_service(mut self) -> SystemHost<'a> {
        self.account = MACOS_SERVICE_ACCOUNT.to_owned();
        self.accounts = Accounts::DirectoryService;
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
        self.held_to(&meta, path, want, noun)
    }

    /// Whether `meta`, of `path`, is of the kind `want`, owned by the release owner and not
    /// writable by a group or others.
    fn held_to(
        &self,
        meta: &fs::Metadata,
        path: &Path,
        want: fn(&fs::FileType) -> bool,
        noun: &str,
    ) -> Result<(), Fault> {
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
        if let Some(binary) = &self.binary {
            let binary = under(&self.root, binary);
            self.check(
                binary.parent().unwrap_or(&binary),
                fs::FileType::is_dir,
                "directory",
            )?;
            self.check(&binary, fs::FileType::is_file, "regular file")?;
            return Ok(binary);
        }
        let (binary, _) = self.layout_of(release)?;
        let bin_dir = binary.parent().unwrap_or(&binary);
        self.check(bin_dir, fs::FileType::is_dir, "directory")?;
        self.check(&binary, fs::FileType::is_file, "regular file")?;
        Ok(binary)
    }

    /// Where the binary of `release` is, unchecked, and whether it is the archive's.
    ///
    /// An archive release has it at [`RELEASE_BINARY`], a release of the Debian or RPM package at
    /// [`PACKAGE_RELEASE_BINARY`]. The archive's path is taken when anything is at it or at its
    /// `bin` (so a link, a directory or a missing file there is refused, never passed over), the
    /// package's when nothing is at either and something is at the package's path, and neither is
    /// an error naming both.
    fn layout_of(&self, release: &Path) -> Result<(PathBuf, bool), Fault> {
        let (archive, archive_there) = self.archive_of(release);
        let package = under(&self.root, &release.join(PACKAGE_RELEASE_BINARY));
        match (archive_there, fs::symlink_metadata(&package).is_ok()) {
            (false, true) => Ok((package, false)),
            (false, false) => Err(Fault(format!(
                "{} is not there, nor {}",
                archive.display(),
                package.display()
            ))),
            (true, _) => Ok((archive, true)),
        }
    }

    /// The path of the archive binary of `release` under the root, and whether anything is at it
    /// or at its `bin`.
    fn archive_of(&self, release: &Path) -> (PathBuf, bool) {
        let archive = under(&self.root, &release.join(RELEASE_BINARY));
        let there = |path: &Path| fs::symlink_metadata(path).is_ok();
        let archive_there = there(&archive) || archive.parent().is_some_and(there);
        (archive, archive_there)
    }

    /// Puts the archive binary of `release` at [`SERVICE_BINARY`] under the root
    /// ([`ServiceBinary`]). A release that is not the archive's (nothing at `bin/tierstore` or
    /// `bin`: a release of the Debian or RPM package, or of the macOS installer package) has none
    /// to put, and its package manager puts its binary in place.
    fn install_from(&self, release: &Path) -> Result<(), Fault> {
        let (binary, archive) = self.archive_of(release);
        if !archive {
            return Ok(());
        }
        self.check(
            binary.parent().unwrap_or(&binary),
            fs::FileType::is_dir,
            "directory",
        )?;
        let mut file = self.open_held(&binary)?;
        ServiceBinary::new(&self.root, self.release_owner).install(&mut file)
    }

    /// The user and group ids of the service account: from `etc/passwd` under the root, or from
    /// the directory service of a Mac ([`Self::with_directory_service`]). `None` when the hook
    /// does not run as root, whose checks run as the hook itself. An account that is not there
    /// is an error naming it and where it was looked for.
    fn run_as(&self) -> Result<Option<(u32, u32)>, Fault> {
        if self.hook_uid != 0 {
            return Ok(None);
        }
        match self.accounts {
            Accounts::PasswdFile => self.passwd_ids().map(Some),
            Accounts::DirectoryService => self.directory_ids().map(Some),
        }
    }

    /// The ids of the service account in `etc/passwd` under the root.
    fn passwd_ids(&self) -> Result<(u32, u32), Fault> {
        let path = self.root.join("etc/passwd");
        let text = self
            .read_config(&path)
            .map_err(|fault| Fault(format!("{}: {fault}", path.display())))?
            .unwrap_or_default();
        account_ids(&text, &self.account).ok_or_else(|| {
            Fault(format!(
                "{} has no account {}",
                path.display(),
                self.account
            ))
        })
    }

    /// The ids of the service account in the local directory service, as `dscl` reads them.
    fn directory_ids(&self) -> Result<(u32, u32), Fault> {
        let args = dscl_args(&self.account);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let call = Call::new(DSCL_PROGRAM, &args, self.timeout);
        let missing = |how: String| {
            Fault(format!(
                "the directory service has no account {} ({}: {how})",
                self.account,
                call.describe()
            ))
        };
        let output = self
            .runner
            .run(&call)
            .map_err(|error| missing(error.to_string()))?;
        match output.code {
            Some(0) => {}
            Some(code) => {
                return Err(missing(format!("exited {code}: {}", output.stderr_text())));
            }
            None => return Err(missing("ended by a signal".to_owned())),
        }
        dscl_ids(&String::from_utf8_lossy(&output.stdout))
            .ok_or_else(|| missing("no numeric UniqueID and PrimaryGroupID".to_owned()))
    }

    /// Opens `binary` for reading, without following a link, and holds what was opened to the
    /// checks of [`Self::binary_of`]: a regular file, owned by the release owner and not writable
    /// by a group or others.
    fn open_held(&self, binary: &Path) -> Result<fs::File, Fault> {
        use rustix::fs::{Mode, OFlags, open};
        let fault = |error: &dyn std::fmt::Display| Fault(format!("{}: {error}", binary.display()));
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let file =
            fs::File::from(open(binary, flags, Mode::empty()).map_err(|error| fault(&error))?);
        let meta = file.metadata().map_err(|error| fault(&error))?;
        self.held_to(&meta, binary, fs::FileType::is_file, "regular file")?;
        Ok(file)
    }

    /// Opens `binary` for a child to execute, and holds what was opened to the checks of
    /// [`Self::binary_of`] ([`Self::open_held`]: the open follows no link). The descriptor is at
    /// least 3, so that the child's standard streams never take its place.
    #[cfg(target_os = "linux")]
    fn open_binary(&self, binary: &Path) -> Result<Opened, Fault> {
        use std::os::fd::AsRawFd;
        let fault = |error: &dyn std::fmt::Display| Fault(format!("{}: {error}", binary.display()));
        let held = self.open_held(binary)?;
        let fd = rustix::io::fcntl_dupfd_cloexec(&held, 3).map_err(|error| fault(&error))?;
        let file = fs::File::from(fd);
        Ok(Opened {
            exec: format!("/proc/self/fd/{}", file.as_raw_fd()),
            _file: file,
        })
    }

    /// Elsewhere the child executes `binary` by its path, checked again as it is now.
    #[cfg(not(target_os = "linux"))]
    fn open_binary(&self, binary: &Path) -> Result<Opened, Fault> {
        self.check(binary, fs::FileType::is_file, "regular file")?;
        Ok(Opened {
            exec: binary.to_string_lossy().into_owned(),
        })
    }

    /// The text of the configuration file `config` (`file` under the root) when it is under the
    /// directory of [`Self::with_root_only`], read by the hook; `None` for a file the check is to
    /// read itself. A file that is not there, or cannot be read, is an error naming it.
    fn root_only_text(&self, config: &Path, file: &Path) -> Result<Option<String>, Fault> {
        if !self
            .root_only
            .as_ref()
            .is_some_and(|dir| config.starts_with(dir))
        {
            return Ok(None);
        }
        let text = self
            .read_config(file)
            .map_err(|fault| Fault(format!("{}: {fault}", file.display())))?;
        text.map(Some)
            .ok_or_else(|| Fault(format!("{} is not there", file.display())))
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
    /// refuses. An error names the command, the user it ran as and how it failed.
    ///
    /// The binary is held to the same checks as for `--capabilities`. When the hook runs as
    /// root it runs as the service account: `tierstore`, found in `etc/passwd` under the root,
    /// or on a Mac `_tierstore`, found in the directory service (an account that is not there is
    /// an error): the check refuses key files the user it runs as does not own, and the service
    /// does not run as root. A hook that is not root runs it as itself.
    ///
    /// hostkeeper keeps a staged package closed to every user but root (it may hold a secret),
    /// so the service account cannot reach a binary or a file under it by its path. On Linux the
    /// hook opens the binary itself, checks what it opened as it checked the path, and the child
    /// is started under the account's user and group ids and executes that open file
    /// (`/proc/self/fd/<n>`, which needs no search of the directories above it): the account
    /// needs only the right to execute the file. A configuration file under the directory of
    /// [`Self::with_root_only`] (the copy a config package stages) is read by the hook and
    /// handed over in a file in memory the child inherits (`--config /proc/self/fd/<n>`, see
    /// [`handed_over`]); any other one is named by its path, so that the check still proves the
    /// account can read it, and every key file it names.
    ///
    /// On a Mac it is started as `sudo -n -u #<uid> -- <binary> ...`, which gives it the
    /// account's primary group and its other groups from the directory service, as launchd gives
    /// the job. Setting the ids on the child directly does not work there: the standard library
    /// clears the supplementary groups (`setgroups(0, NULL)`) after it sets the group id, and on
    /// macOS that also resets the effective group to 0, so the check could not enter
    /// `/usr/local/tierstore/etc` (root:_tierstore, 0750) and refused every configuration with
    /// "Permission denied". The binary a Mac checks with is the installed one, outside every
    /// staged package (`sudo` closes every descriptor but the standard three, so a descriptor
    /// could not be handed on); a configuration file under the root-only directory goes on its
    /// standard input (`--config /dev/stdin`), which on macOS is the descriptor itself.
    fn check_config(&self, release: &Path, config: &Path) -> Result<Option<String>, PortError> {
        let failed = |fault: Fault| PortError::failed(KIND, fault.to_string());
        let binary = self.binary_of(release).map_err(failed)?;
        let run_as = self.run_as().map_err(failed)?;
        let file = under(&self.root, config);
        let text = self.root_only_text(config, &file).map_err(failed)?;
        let handed = text
            .as_deref()
            .map(handed_over)
            .transpose()
            .map_err(failed)?;
        let shown_config = file.to_string_lossy().into_owned();
        let named = handed
            .as_ref()
            .map_or(shown_config.as_str(), |handed| handed.path.as_str());
        let check = ["--config", named, "--check-config"];
        let who = match run_as {
            None => format!("the hook's own user (uid {})", self.hook_uid),
            Some((uid, gid)) => format!("{} (uid {uid}, gid {gid})", self.account),
        };
        let program = binary.to_string_lossy().into_owned();
        let opened;
        let (call, by_descriptor) = match (run_as, self.accounts) {
            // `sudo` takes the primary group from the account record itself; the
            // `PrimaryGroupID` read above is still required, so that an account record
            // without one fails as a missing account rather than at the check.
            (Some((uid, _)), Accounts::DirectoryService) => {
                let user = format!("#{uid}");
                let mut args = vec!["-n", "-u", user.as_str(), "--", program.as_str()];
                args.extend(check);
                (Call::new(SUDO_PROGRAM, &args, self.timeout), false)
            }
            (run_as, _) => {
                opened = self.open_binary(&binary).map_err(failed)?;
                let call = Call::new(&opened.exec, &check, self.timeout);
                let call = match run_as {
                    Some((uid, gid)) => call.running_as(uid, gid),
                    None => call,
                };
                (call, cfg!(target_os = "linux"))
            }
        };
        // What was run and as whom, for every message: the binary by the path it was found at
        // (also when the child executed the file the hook opened), and a configuration the hook
        // handed over by its own path.
        let mut shown = if by_descriptor {
            format!(
                "{program} --config {shown_config} --check-config (executed from the file the \
                 hook opened)"
            )
        } else {
            call.describe()
        };
        if handed.is_some() {
            shown.push_str(&format!(
                ", {shown_config} read by the hook and handed over"
            ));
        }
        shown.push_str(&format!(", as {who}"));
        let call = match handed.as_ref().and_then(|handed| handed.stdin.as_deref()) {
            Some(bytes) => call.with_stdin(bytes),
            None => call,
        };
        let output = self
            .runner
            .run(&call)
            .map_err(|error| PortError::failed(KIND, format!("{shown}: {error}")))?;
        let how = match output.code {
            Some(0) => return Ok(None),
            Some(CONFIG_REFUSED) => return Ok(Some(output.stderr_text())),
            Some(code) => format!("exited {code}"),
            None => "ended by a signal".to_owned(),
        };
        Err(PortError::failed(
            KIND,
            format!("{shown}: {how}: {}", output.stderr_text()),
        ))
    }

    /// Copies the archive binary of `release` (its `bin/tierstore`, checked as for a check of
    /// the configuration, and read through a descriptor opened without following a link) to
    /// [`SERVICE_BINARY`] under the root; any other release (of the Debian or RPM package, or of
    /// the macOS installer package) is left to its package manager. The copy's directories must
    /// be the release owner's (root on a host).
    fn install_binary(&self, release: &Path) -> Result<(), PortError> {
        self.install_from(release).map_err(|fault| {
            PortError::failed(
                KIND,
                format!(
                    "the binary of {} cannot be put at {SERVICE_BINARY}: {fault}",
                    release.display()
                ),
            )
        })
    }

    fn remove_binary(&self) -> Result<(), PortError> {
        ServiceBinary::new(&self.root, self.release_owner)
            .remove()
            .map_err(|fault| {
                PortError::failed(KIND, format!("{SERVICE_BINARY} cannot be removed: {fault}"))
            })
    }
}

#[cfg(test)]
#[path = "host_root_only_tests.rs"]
mod root_only_tests;
