//! The job of `tierstore fs` in the launchd arm.
//!
//! A Mac serves the file tree through an NFS mount that its fs job makes and keeps, and a mount
//! left without its server blocks every process that touches it. So the steps that restart the
//! main job never leave the file service stopped with its mount in place: the fs job is booted out
//! first (its process unmounts as it ends), the mount list is read, and only a mount that is gone
//! lets the step go on. A mount that is still listed ends the step, with the main job left as it
//! is, and the fs job is booted in again, which takes the mount over. After the main job serves,
//! the fs job is booted in.
//!
//! The mount point is read from [`FS_CONFIG_FILE`]. Everything here is idle when the params name
//! no fs job.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::report::{Done, Reason};
use super::super::{FS_CONFIG_FILE, FS_LAUNCHD_PLIST};
use super::Ctx;

/// The mount point when `fs.toml` leaves out `[fs] mountpoint`: the default of `tierstore fs`.
const DEFAULT_MOUNTPOINT: &str = "/run/tierstore/fs";
/// The longest `launchctl bootout` of the fs job may run: the 150 s its plist gave the process to
/// exit (`ExitTimeOut`; the packages that shipped that plist, the last one before
/// komira-ai/tierstore#624) and a margin of 30 s.
pub(in crate::server::update_hook) const FS_BOOTOUT_TIMEOUT: Duration = Duration::from_secs(180);
/// How many looks, after that call, the domain is given to let the fs job go.
pub(in crate::server::update_hook) const FS_GONE_ATTEMPTS: u32 = 30;

/// The mount point `[fs] mountpoint` of the text of `fs.toml` names; the default when the key, or
/// the table, is left out. The error says what is wrong.
pub(in crate::server) fn mountpoint_of(text: &str) -> Result<PathBuf, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|error: toml::de::Error| format!("not TOML: {}", error.message()))?;
    let default = PathBuf::from(DEFAULT_MOUNTPOINT);
    let Some(fs) = table.get("fs") else {
        return Ok(default);
    };
    let Some(fs) = fs.as_table() else {
        return Err("fs is not a table".to_owned());
    };
    match fs.get("mountpoint") {
        None => Ok(default),
        Some(toml::Value::String(path)) if Path::new(path).is_absolute() => Ok(PathBuf::from(path)),
        Some(_) => Err("fs.mountpoint is not an absolute path".to_owned()),
    }
}

impl Ctx<'_, '_> {
    /// The mount point the fs job's configuration names.
    fn fs_mountpoint(&self) -> Result<PathBuf, Done> {
        let Some(text) = self.ports.host.config(Path::new(FS_CONFIG_FILE))? else {
            return Err(Done::fail(
                Reason::ConfigMissing,
                format!(
                    "there is no configuration file at {FS_CONFIG_FILE}, so the mount point of \
                     the file service is not known: nothing was stopped"
                ),
            ));
        };
        mountpoint_of(&text).map_err(|why| {
            Done::fail(
                Reason::ConfigRefused,
                format!(
                    "the mount point cannot be read from {FS_CONFIG_FILE} ({why}): nothing was \
                     stopped"
                ),
            )
        })
    }

    /// Loads the fs job's plist into the system domain.
    fn bootstrap_fs(&self) -> Result<(), Done> {
        Ok(self.ports.launchd.bootstrap(Path::new(FS_LAUNCHD_PLIST))?)
    }

    /// Boots the fs job out when the params name one and it is loaded, and returns once its mount
    /// is gone. A mount that is still listed (or a list that cannot be had) ends the step, after
    /// the fs job is booted in again so that the mount has a server; nothing else is stopped. A
    /// configuration whose mount point is unknown is refused before anything is stopped.
    pub(super) fn stop_fs(&self) -> Result<(), Done> {
        let Some(fs) = self.fs else {
            return Ok(());
        };
        if !self.ports.launchd.loaded(fs)? {
            return Ok(());
        }
        let mountpoint = self.fs_mountpoint()?;
        self.ports
            .launchd
            .bootout_within(fs, FS_BOOTOUT_TIMEOUT, FS_GONE_ATTEMPTS)?;
        let listed = self.ports.host.mounted(&mountpoint);
        if matches!(listed, Ok(false)) {
            return Ok(());
        }
        self.bootstrap_fs()?;
        match listed {
            Err(error) => Err(error.into()),
            Ok(_) => Err(Done::fail(
                Reason::NotReady,
                format!(
                    "{} is still mounted after {fs} stopped: {} is left as it is, and {fs} is \
                     loaded again",
                    mountpoint.display(),
                    self.label
                ),
            )),
        }
    }

    /// Boots the fs job in when the params name one and it is not loaded.
    pub(super) fn start_fs(&self) -> Result<(), Done> {
        let Some(fs) = self.fs else {
            return Ok(());
        };
        if self.ports.launchd.loaded(fs)? {
            return Ok(());
        }
        self.bootstrap_fs()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::mountpoint_of;

    /// Asserts that the mount point is `[fs] mountpoint` when it is an absolute path, the default
    /// of `tierstore fs` when the key or the table is left out, and an error naming what is wrong
    /// for a text that is not TOML, an `fs` that is no table and a mount point that is not an
    /// absolute path. Catches a mount check made on the wrong path, and a relative path taken for
    /// a mount point.
    #[test]
    fn the_mount_point_is_read_from_the_fs_table() {
        let of = mountpoint_of;
        assert_eq!(
            of("[fs]\nmountpoint = \"/private/var/tierstore/fs\"\n"),
            Ok(PathBuf::from("/private/var/tierstore/fs"))
        );
        let default = Ok(PathBuf::from("/run/tierstore/fs"));
        assert_eq!(of(""), default);
        assert_eq!(of("[fs]\nother = 1\n"), default);
        assert!(of("= nope").unwrap_err().starts_with("not TOML: "));
        assert_eq!(of("fs = 1\n"), Err("fs is not a table".to_owned()));
        let not_absolute = Err("fs.mountpoint is not an absolute path".to_owned());
        assert_eq!(of("[fs]\nmountpoint = \"fs\"\n"), not_absolute);
        assert_eq!(of("[fs]\nmountpoint = 5\n"), not_absolute);
    }
}
