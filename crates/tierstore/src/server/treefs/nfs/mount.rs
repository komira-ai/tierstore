//! The mount of the export, and its unmount, by the commands of macOS (`mount_nfs`, `umount`), run
//! as the user the mount is made as. The commands go through a [`Runner`], so that a test runs
//! none.
//!
//! `mount_nfs` of a loopback export needs no root when the mount point belongs to the user who
//! runs it (`docs/design/virtual-build-fs.md` section 8), so the process checks, before it mounts,
//! that the mount point belongs to the mount user, and says who owns it if not. A process that is
//! root and is asked to mount as another user runs the commands through `sudo -u`; one that is
//! neither refuses.

use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// The program that lists the mounts.
const MOUNT: &str = "/sbin/mount";

/// The program that tells a user's number.
const ID: &str = "/usr/bin/id";

/// The program that runs a command as another user.
const SUDO: &str = "/usr/bin/sudo";

/// The mount options: NFS version 3 over TCP, read-only (without it every read ends with a
/// `SETATTR` of the access time), `READDIRPLUS`, local locks, attributes and names kept for an
/// hour (everything below a tree's root is immutable), and a hard mount, so that a client waits
/// for a server that restarts and does not fail. `port` and `mountport` are added.
const OPTIONS: &str = "vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard";

/// What a command said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Ran {
    /// Whether it exited with status 0.
    pub success: bool,
    /// What it wrote to its standard output.
    pub stdout: String,
    /// What it wrote to its standard error.
    pub stderr: String,
}

/// What runs a command, and waits.
pub(super) trait Runner: Send + Sync {
    /// Runs `argv` (the program first) and waits for it.
    ///
    /// # Errors
    /// What the system said when the program could not be run.
    fn run(&self, argv: &[String]) -> std::io::Result<Ran>;

    /// Waits `how_long` between two tries of a mount.
    fn pause(&self, how_long: Duration);
}

/// The commands of this machine.
pub(super) struct System;

impl Runner for System {
    fn run(&self, argv: &[String]) -> std::io::Result<Ran> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(Ran {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn pause(&self, how_long: Duration) {
        std::thread::sleep(how_long);
    }
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
        Self {
            runner,
            user: settings.mount_user.clone(),
            mountpoint,
            euid,
            tries: u32::try_from(tries).unwrap_or(u32::MAX).max(1),
            retry: settings.mount_retry,
        }
    }

    /// Runs `argv`, and gives what it said.
    fn run(&self, argv: &[String]) -> Result<Ran, NfsError> {
        self.runner.run(argv).map_err(|source| NfsError::Run {
            command: argv.join(" "),
            source,
        })
    }

    /// The user id of the mount user: the user of the process if no user is named.
    ///
    /// # Errors
    /// [`NfsError::User`] for a name the machine does not know.
    pub(super) fn user_id(&self) -> Result<u32, NfsError> {
        let Some(user) = &self.user else {
            return Ok(self.euid);
        };
        let ran = self.run(&[ID.to_owned(), "-u".to_owned(), user.clone()])?;
        let number = ran.stdout.trim().parse();
        match number {
            Ok(uid) if ran.success => Ok(uid),
            _ => Err(NfsError::User {
                user: user.clone(),
                said: ran.stderr.trim().to_owned(),
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
            format!("{OPTIONS},port={port},mountport={port}"),
            format!("{host}:/"),
            self.mountpoint.display().to_string(),
        ]
    }

    /// Whether something is mounted at the mount point now (a server that restarts finds the mount
    /// of the one before it, which the clients' hard mount waits on, and must not mount again on
    /// top of it).
    ///
    /// # Errors
    /// [`NfsError::Run`] for a listing that could not be run.
    pub(super) fn is_mounted(&self) -> Result<bool, NfsError> {
        let listing = self.run(&[MOUNT.to_owned()])?;
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

    /// Checks that the mount that is at the mount point was made from `address`, as the file of
    /// [`Mounter::record`] says. A mount with no such file (made by a release that did not write
    /// it) is taken to be this one's.
    ///
    /// # Errors
    /// [`NfsError::MountedFrom`] for a mount made from another address.
    pub(super) fn agrees(&self, runtime_dir: &Path, address: SocketAddr) -> Result<(), NfsError> {
        match std::fs::read_to_string(runtime_dir.join(MOUNTED)) {
            Ok(was) if was.trim() != address.to_string() => Err(NfsError::MountedFrom {
                path: self.mountpoint.clone(),
                was: was.trim().to_owned(),
                now: address,
            }),
            _ => Ok(()),
        }
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
            said = ran.stderr.trim().to_owned();
        }
        Err(NfsError::Mount {
            path: self.mountpoint.clone(),
            tries: self.tries,
            said,
        })
    }

    /// Unmounts the mount point, as the mount user.
    ///
    /// # Errors
    /// [`NfsError::Unmount`] for an unmount that failed.
    pub(super) fn unmount(&self) -> Result<(), NfsError> {
        let uid = self.user_id()?;
        let command = self.as_user(
            uid,
            vec![UMOUNT.to_owned(), self.mountpoint.display().to_string()],
        )?;
        let ran = self.run(&command)?;
        if ran.success {
            return Ok(());
        }
        Err(NfsError::Unmount {
            path: self.mountpoint.clone(),
            said: ran.stderr.trim().to_owned(),
        })
    }
}

#[cfg(test)]
#[path = "mount_tests.rs"]
mod tests;
