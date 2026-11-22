//! The mount of the export, and its unmount, by the commands of macOS (`mount_nfs`, `umount`,
//! `diskutil`), run as the user the mount is made as. The commands go through a [`Runner`], so that
//! a test runs none.
//!
//! Every command has a limit, `fs.nfs.unmount_wait`: one that has not ended by then is killed with
//! its process group, and counts as failed, so that no command keeps the process waiting (a command
//! that touches a mount whose server does not answer can wait in the kernel for ever).
//!
//! The mount is `hard` with `deadtimeout`: a server that restarts is waited for, and a mount whose
//! server has not answered for `fs.nfs.dead_timeout` is marked dead by the kernel, after which
//! every call on it fails (`ENXIO`) instead of waiting.
//!
//! An unmount is `umount`; `diskutil unmount force` when `umount` fails; and the `unmount(2)` of
//! this process with `MNT_FORCE` when that fails too, each within its limit. `umount` refuses a
//! mount that a process still uses (`Resource busy`); `diskutil` asks Disk Arbitration, which forces
//! the unmount of the mount user's own mount; `unmount(2)` runs no program at all. Once the server
//! of a busy mount is gone even `umount -f` was refused (`Operation not permitted`, to root as
//! well), so the unmount is made while this process still serves (`run.rs`). A mount left in place
//! is a mount whose server is gone: what touches it waits until the kernel marks it dead.
//!
//! `mount_nfs` of a loopback export needs no root when the mount point belongs to the user who
//! runs it (`docs/design/virtual-build-fs.md` section 8), so the process checks, before it mounts,
//! that the mount point belongs to the mount user, and says who owns it if not. A process that is
//! root and is asked to mount as another user runs the commands through `sudo -u`; one that is
//! neither refuses.

use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use super::config::NfsSettings;
use super::error::NfsError;
use crate::server::treefs::start::io;

/// The file in the runtime directory that says which address the mount was made from.
const MOUNTED: &str = "nfs-mounted";

/// The program that mounts.
const MOUNT_NFS: &str = "/sbin/mount_nfs";

/// The program that unmounts.
const UMOUNT: &str = "/sbin/umount";

/// The program that forces an unmount `umount` cannot make.
const DISKUTIL: &str = "/usr/sbin/diskutil";

/// The program that lists the mounts.
const MOUNT: &str = "/sbin/mount";

/// The program that tells a user's number.
const ID: &str = "/usr/bin/id";

/// The program that runs a command as another user.
const SUDO: &str = "/usr/bin/sudo";

/// The mount options: NFS version 3 over TCP, read-only (without it every read ends with a
/// `SETATTR` of the access time), `READDIRPLUS`, local locks, attributes and names kept for an
/// hour (everything below a tree's root is immutable), and a hard mount, so that a client waits
/// for a server that restarts and does not fail. `deadtimeout`, `port` and `mountport` are added.
const OPTIONS: &str = "vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard";

/// How long a command killed at its limit is waited for once more, to be collected. A command
/// asleep in the kernel ends only when it wakes: its thread is left to collect it.
pub(super) const REAP_WAIT: Duration = Duration::from_secs(5);

/// The pause between two unmounts of a process that ends, while every step of the unmount fails.
pub(super) const LEAVE_RETRY: Duration = Duration::from_secs(10);

/// What a command said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Ran {
    /// Whether it exited with status 0.
    pub success: bool,
    /// What it wrote to its standard output.
    pub stdout: String,
    /// What it wrote to its standard error.
    pub stderr: String,
    /// Whether it was still running at its limit, and was killed.
    pub timed_out: bool,
}

/// What runs a command, and waits.
pub(super) trait Runner: Send + Sync {
    /// Runs `argv` (the program first) and waits for it at most `limit`: one that has not ended
    /// by then is killed with its process group, and its [`Ran`] says `timed_out`.
    ///
    /// # Errors
    /// What the system said when the program could not be run.
    fn run_within(&self, argv: &[String], limit: Duration) -> std::io::Result<Ran>;

    /// The `unmount(2)` of `path` with `MNT_FORCE`, by this process: no program is run. It is
    /// waited for at most `limit`.
    ///
    /// # Errors
    /// What the system said, or [`std::io::ErrorKind::TimedOut`] for no answer within `limit`.
    fn force_unmount(&self, path: &Path, limit: Duration) -> std::io::Result<()>;

    /// Waits `how_long` between two tries of a mount, or of an unmount.
    fn pause(&self, how_long: Duration);
}

/// The commands of this machine.
pub(super) struct System;

impl Runner for System {
    fn run_within(&self, argv: &[String], limit: Duration) -> std::io::Result<Ran> {
        run_noting(argv, limit, |_| {})
    }

    fn force_unmount(&self, path: &Path, limit: Duration) -> std::io::Result<()> {
        let path = path.to_owned();
        answer_within(on_thread(move || unmount_now(&path))?, limit)
    }

    fn pause(&self, how_long: Duration) {
        std::thread::sleep(how_long);
    }
}

/// Runs `argv` in a process group of its own, tells `spawned` its process id, and waits at most
/// `limit` for it to end and to close its output; past that, kills its process group (with
/// whatever it started that keeps its output open), waits [`REAP_WAIT`] for it to be collected,
/// and says it timed out.
fn run_noting(argv: &[String], limit: Duration, spawned: impl FnOnce(u32)) -> std::io::Result<Ran> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()?;
    let pid = child.id();
    spawned(pid);
    // The thread reads both outputs to their end and then collects the child: until then the
    // child's id stays its own, so the kill below cannot reach another process.
    let answer = on_thread(move || child.wait_with_output())?;
    if let Ok(output) = answer.recv_timeout(limit) {
        let output = output?;
        return Ok(Ran {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            timed_out: false,
        });
    }
    let _ = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .map(|group| rustix::process::kill_process_group(group, rustix::process::Signal::KILL));
    let _ = answer.recv_timeout(REAP_WAIT);
    Ok(Ran {
        success: false,
        stdout: String::new(),
        stderr: String::new(),
        timed_out: true,
    })
}

/// Runs `work` on a thread of its own; the receiver of what it gives.
fn on_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<mpsc::Receiver<T>> {
    let (done, answer) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("tierstore-fs-command".to_owned())
        .spawn(move || {
            let _ = done.send(work());
        })?;
    Ok(answer)
}

/// What `answer` gives within `limit`, or a [`std::io::ErrorKind::TimedOut`] error; the work goes
/// on, and what it gives later is dropped.
fn answer_within(
    answer: mpsc::Receiver<std::io::Result<()>>,
    limit: Duration,
) -> std::io::Result<()> {
    answer.recv_timeout(limit).unwrap_or_else(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("no answer in {}ms", limit.as_millis()),
        ))
    })
}

/// The `unmount(2)` of `path` with `MNT_FORCE`.
#[cfg(target_os = "linux")]
fn unmount_now(path: &Path) -> std::io::Result<()> {
    rustix::mount::unmount(path, rustix::mount::UnmountFlags::FORCE).map_err(std::io::Error::from)
}

/// The `unmount(2)` of `path` with `MNT_FORCE`.
#[cfg(target_os = "macos")]
fn unmount_now(path: &Path) -> std::io::Result<()> {
    nix::mount::unmount(path, nix::mount::MntFlags::MNT_FORCE).map_err(std::io::Error::from)
}

/// No `unmount(2)` on a system this process is not built for.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unmount_now(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

/// What makes and ends the mount.
#[derive(Clone)]
pub(super) struct Mounter {
    runner: Arc<dyn Runner>,
    user: Option<String>,
    mountpoint: PathBuf,
    euid: u32,
    tries: u32,
    retry: Duration,
    /// The limit of each command.
    limit: Duration,
    /// The `deadtimeout` of the mount, in seconds.
    dead_after: u64,
    /// The user id of the mount user, once found: an unmount does not ask again.
    uid: Arc<OnceLock<u32>>,
}

impl Mounter {
    /// The mounter of `mountpoint`, run by the process of user id `euid`.
    pub(super) fn new(
        runner: Arc<dyn Runner>,
        settings: &NfsSettings,
        mountpoint: PathBuf,
        euid: u32,
    ) -> Self {
        let tries = settings.mount_wait.as_millis() / settings.mount_retry.as_millis().max(1);
        let dead = settings.dead_timeout;
        Self {
            runner,
            user: settings.mount_user.clone(),
            mountpoint,
            euid,
            tries: u32::try_from(tries).unwrap_or(u32::MAX).max(1),
            retry: settings.mount_retry,
            limit: settings.unmount_wait,
            dead_after: dead.as_secs() + u64::from(dead.subsec_nanos() > 0),
            uid: Arc::new(OnceLock::new()),
        }
    }

    /// The mount point.
    pub(super) fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Waits `how_long`, by the runner's pause.
    pub(super) fn pause(&self, how_long: Duration) {
        self.runner.pause(how_long);
    }

    /// Runs `argv` within the limit, and gives what it said.
    fn run(&self, argv: &[String]) -> Result<Ran, NfsError> {
        self.runner
            .run_within(argv, self.limit)
            .map_err(|source| NfsError::Run {
                command: argv.join(" "),
                source,
            })
    }

    /// What `ran` said: what the command wrote, or that it was killed at its limit.
    fn said(&self, ran: &Ran) -> String {
        if ran.timed_out {
            format!("did not end in {}ms and was killed", self.limit.as_millis())
        } else {
            ran.said()
        }
    }

    /// The user id of the mount user: the user of the process if no user is named. It is asked
    /// once: an unmount uses the answer of the start.
    ///
    /// # Errors
    /// [`NfsError::User`] for a name the machine does not know.
    pub(super) fn user_id(&self) -> Result<u32, NfsError> {
        let Some(user) = &self.user else {
            return Ok(self.euid);
        };
        if let Some(uid) = self.uid.get() {
            return Ok(*uid);
        }
        let ran = self.run(&[ID.to_owned(), "-u".to_owned(), user.clone()])?;
        let number = ran.stdout.trim().parse();
        match number {
            Ok(uid) if ran.success => Ok(*self.uid.get_or_init(|| uid)),
            _ => Err(NfsError::User {
                user: user.clone(),
                said: if ran.timed_out {
                    self.said(&ran)
                } else {
                    ran.stderr.trim().to_owned()
                },
            }),
        }
    }

    /// `argv`, run as the mount user: as it is if the process is that user, through `sudo` if the
    /// process is root.
    fn as_user(&self, uid: u32, mut argv: Vec<String>) -> Result<Vec<String>, NfsError> {
        let Some(user) = &self.user else {
            return Ok(argv);
        };
        if uid == self.euid {
            return Ok(argv);
        }
        if self.euid != 0 {
            return Err(NfsError::NotThatUser {
                euid: self.euid,
                user: user.clone(),
                uid,
            });
        }
        let mut full: Vec<String> = [SUDO, "-n", "-u", user, "--"]
            .iter()
            .map(|word| (*word).to_owned())
            .collect();
        full.append(&mut argv);
        Ok(full)
    }

    /// The command that mounts the export served on `port` of the loopback address `host`.
    pub(super) fn mount_command(&self, host: std::net::IpAddr, port: u16) -> Vec<String> {
        vec![
            MOUNT_NFS.to_owned(),
            "-o".to_owned(),
            format!(
                "{OPTIONS},deadtimeout={},port={port},mountport={port}",
                self.dead_after
            ),
            format!("{host}:/"),
            self.mountpoint.display().to_string(),
        ]
    }

    /// Whether something is mounted at the mount point now (a server that restarts finds the mount
    /// of the one before it, which the clients' hard mount waits on, and must not mount again on
    /// top of it).
    ///
    /// # Errors
    /// [`NfsError::Run`] for a listing that could not be run, or that did not end within its limit.
    pub(super) fn is_mounted(&self) -> Result<bool, NfsError> {
        let listing = self.run(&[MOUNT.to_owned()])?;
        if listing.timed_out {
            return Err(NfsError::Run {
                command: MOUNT.to_owned(),
                source: std::io::Error::new(std::io::ErrorKind::TimedOut, self.said(&listing)),
            });
        }
        let real =
            std::fs::canonicalize(&self.mountpoint).unwrap_or_else(|_| self.mountpoint.clone());
        let marks = [
            format!(" on {} (", self.mountpoint.display()),
            format!(" on {} (", real.display()),
        ];
        Ok(listing
            .stdout
            .lines()
            .any(|line| marks.iter().any(|mark| line.contains(mark.as_str()))))
    }

    /// Notes where the export that was just mounted is served, for the next start in this boot
    /// (`fs.runtime_dir/nfs-mounted`).
    ///
    /// # Errors
    /// [`NfsError::Start`] if the file cannot be written.
    pub(super) fn record(runtime_dir: &Path, address: SocketAddr) -> Result<(), NfsError> {
        std::fs::write(runtime_dir.join(MOUNTED), address.to_string())
            .map_err(io("writing the address of the mount"))?;
        Ok(())
    }

    /// The address the mount at the mount point was made from, as the file of [`Mounter::record`]
    /// says; none for a mount with no such file (made by hand, or by a release that did not write
    /// it), which is taken to be this instance's.
    pub(super) fn made_from(runtime_dir: &Path) -> Option<String> {
        std::fs::read_to_string(runtime_dir.join(MOUNTED))
            .ok()
            .map(|was| was.trim().to_owned())
    }

    /// Mounts the export served on `host:port` at the mount point, as the mount user, trying again
    /// for as long as `fs.nfs.mount_wait` says.
    ///
    /// # Errors
    /// [`NfsError::MountPointOwner`] for a mount point the mount user does not own,
    /// [`NfsError::Mount`] for a mount that failed every time.
    pub(super) fn mount(&self, host: std::net::IpAddr, port: u16) -> Result<(), NfsError> {
        let uid = self.user_id()?;
        let owner = std::fs::metadata(&self.mountpoint)
            .map_err(|source| NfsError::Run {
                command: format!("stat {}", self.mountpoint.display()),
                source,
            })?
            .uid();
        if owner != uid {
            return Err(NfsError::MountPointOwner {
                path: self.mountpoint.clone(),
                owner,
                user: uid,
            });
        }
        let command = self.as_user(uid, self.mount_command(host, port))?;
        let mut said = String::new();
        for attempt in 0..self.tries {
            if attempt > 0 {
                self.runner.pause(self.retry);
            }
            let ran = self.run(&command)?;
            if ran.success {
                return Ok(());
            }
            said = if ran.timed_out {
                self.said(&ran)
            } else {
                ran.stderr.trim().to_owned()
            };
        }
        Err(NfsError::Mount {
            path: self.mountpoint.clone(),
            tries: self.tries,
            said,
        })
    }

    /// One command of an unmount, as the mount user: none if it unmounted, else what it said
    /// (that it could not be run, failed, or was killed at its limit). Each says so in a line.
    fn step(&self, uid: u32, argv: Vec<String>) -> Result<Option<String>, NfsError> {
        let command = self.as_user(uid, argv)?;
        let shown = command.join(" ");
        let said = match self.run(&command) {
            Ok(ran) if ran.success => None,
            Ok(ran) => Some(self.said(&ran)),
            Err(why) => Some(why.to_string()),
        };
        match &said {
            None => tracing::info!("tierstore fs: {shown}: unmounted"),
            Some(why) => tracing::info!("tierstore fs: {shown}: {why}"),
        }
        Ok(said)
    }

    /// Unmounts the mount point: `umount`, then `diskutil unmount force`, then the `unmount(2)` of
    /// this process with `MNT_FORCE` (see the module's text), each within its limit, the commands
    /// as the mount user. Nothing mounted there is nothing to do; a listing that cannot be read is
    /// taken as a mount, and the steps are tried. Each step says in a line what it found, and an
    /// unmount that needed force says so once as a warning.
    ///
    /// # Errors
    /// [`NfsError::Unmount`] with what every step said, when none unmounted.
    pub(super) fn unmount(&self) -> Result<(), NfsError> {
        let path = self.mountpoint.display().to_string();
        match self.is_mounted() {
            Ok(false) => {
                tracing::info!("tierstore fs: nothing is mounted at {path}");
                return Ok(());
            }
            Ok(true) => {}
            Err(why) => tracing::warn!("tierstore fs: {why}; unmounting {path} all the same"),
        }
        let uid = self.user_id()?;
        let Some(plain) = self.step(uid, vec![UMOUNT.to_owned(), path.clone()])? else {
            return Ok(());
        };
        let force = [DISKUTIL, "unmount", "force", &path].map(str::to_owned);
        let forced = |how: &str| {
            tracing::warn!(
                "tierstore fs: umount of {path} failed ({plain}); unmounted by {how}, so what \
                 still reads the mount gets an error, not a wait"
            );
        };
        let Some(diskutil) = self.step(uid, force.to_vec())? else {
            forced("diskutil unmount force");
            return Ok(());
        };
        match self.runner.force_unmount(&self.mountpoint, self.limit) {
            Ok(()) => {
                tracing::info!("tierstore fs: unmount(2) of {path} with MNT_FORCE: unmounted");
                forced("this process's unmount(2) with MNT_FORCE");
                Ok(())
            }
            Err(why) => {
                tracing::info!("tierstore fs: unmount(2) of {path} with MNT_FORCE: {why}");
                Err(NfsError::Unmount {
                    path: self.mountpoint.clone(),
                    said: format!(
                        "{plain}; diskutil unmount force: {diskutil}; unmount(2) with \
                         MNT_FORCE: {why}"
                    ),
                })
            }
        }
    }

    /// Unmounts as [`Mounter::unmount`] does, and while it fails, says why and tries again after
    /// [`LEAVE_RETRY`]: the caller serves the mount meanwhile, and returns only once it is gone. A
    /// process that ends this way is never one that leaves a mount without its server (if it is
    /// killed first, the mount's dead timeout ends the mount).
    pub(super) fn unmount_or_retry(&self) {
        while let Err(why) = self.unmount() {
            tracing::error!(
                "tierstore fs: {why}; the process goes on serving the mount and tries again in \
                 {}s",
                LEAVE_RETRY.as_secs()
            );
            self.pause(LEAVE_RETRY);
        }
    }
}

impl Ran {
    /// What the command said: its standard error, or its standard output if it wrote no error
    /// (`diskutil` says why it failed on its standard output).
    fn said(&self) -> String {
        let error = self.stderr.trim();
        if error.is_empty() {
            self.stdout.trim().to_owned()
        } else {
            error.to_owned()
        }
    }
}

#[cfg(test)]
#[path = "mount_tests.rs"]
mod tests;
