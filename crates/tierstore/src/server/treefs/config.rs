//! The settings of `tierstore fs`: the `[fs]` table of the file `--config` names, the file of this
//! process alone (the S3 instance reads a file of its own, which has no `[fs]` table).
//!
//! | key | meaning | default |
//! |---|---|---|
//! | `fs.mountpoint` | where the one FUSE mount is made | `/run/tierstore/fs` |
//! | `fs.runtime_dir` | the state that outlives the process | `/run/tierstore-fs` |
//! | `fs.control_socket` | the socket clients attach on | `<runtime_dir>/control.sock` |
//! | `fs.client_group` | the group (name or number) that may use that socket | needed |
//! | `fs.owner` | `uid:gid` every entry of every tree reports | needed |
//! | `fs.fill_socket` | the fill socket of the node's tierstore | needed |
//! | `fs.cache_path` | the node's `local_cache.path` | needed |
//! | `fs.fetch_timeout` | how long a fill, or a closed fill socket, is waited for | `60s` |
//! | `fs.threads` | threads that serve the kernel's requests (a fill blocks one) | `16` |

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::server::config::{ConfigError, invalid, require, within};
use crate::server::config_handoff::MOST_SOCKET_PATH;
use crate::server::config_syntax::syntax;
use crate::server::config_tuning::{HOUR, MS};
use crate::server::config_units::Dur;
use crate::server::config_unknown::fields_of;

/// The mount point when the file leaves out `fs.mountpoint`.
const DEFAULT_MOUNTPOINT: &str = "/run/tierstore/fs";

/// The runtime directory when the file leaves out `fs.runtime_dir`: the `RuntimeDirectory=` of the
/// unit.
const DEFAULT_RUNTIME_DIR: &str = "/run/tierstore-fs";

/// The name of the control socket in the runtime directory when the file leaves out
/// `fs.control_socket`.
const DEFAULT_CONTROL_SOCKET: &str = "control.sock";

/// The groups of the host, where a group name is looked up.
pub(super) const GROUP_FILE: &str = "/etc/group";

/// The most time a fill is waited for by default: a fill that fails sooner is not waited for.
const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// The threads that serve the kernel's requests when the file leaves out `fs.threads`.
const DEFAULT_THREADS: u32 = 16;

/// The file: only its `[fs]` table is read.
#[derive(Debug, Deserialize)]
struct File {
    fs: FsTable,
}

/// `[fs]`.
#[derive(Debug, Deserialize)]
pub(super) struct FsTable {
    mountpoint: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    control_socket: Option<PathBuf>,
    client_group: Option<String>,
    owner: Option<String>,
    fill_socket: Option<PathBuf>,
    cache_path: Option<PathBuf>,
    fetch_timeout: Option<Dur>,
    threads: Option<u32>,
}

/// What `tierstore fs` runs with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct FsSettings {
    pub mountpoint: PathBuf,
    pub runtime_dir: PathBuf,
    pub control_socket: PathBuf,
    /// The group id of the control socket.
    pub client_group: u32,
    /// The user and group id every entry reports.
    pub owner: (u32, u32),
    pub fill_socket: PathBuf,
    pub cache_path: PathBuf,
    pub fetch_timeout: Duration,
    pub threads: usize,
    /// Whether users other than the one who mounts may use the mount: always, but in the tests,
    /// where the mounting user is not root and `fusermount3` allows it only by `/etc/fuse.conf`.
    pub allow_other: bool,
}

/// `path`, which is the value of `key`, if it is absolute.
fn absolute(key: &str, path: PathBuf) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(invalid(key, "must be an absolute path"))
    }
}

/// `uid:gid` as two numbers.
fn owner_of(text: &str) -> Result<(u32, u32), ConfigError> {
    let numbers = text
        .split_once(':')
        .and_then(|(uid, gid)| Some((uid.parse().ok()?, gid.parse().ok()?)));
    numbers.ok_or_else(|| invalid("fs.owner", "must be uid:gid, two numbers"))
}

/// The group id `group` names: a number, or a name in the lines `name:x:gid:...` of `groups`.
fn group_of(group: &str, groups: &str) -> Result<u32, ConfigError> {
    if let Ok(gid) = group.parse() {
        return Ok(gid);
    }
    let found = groups.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        match fields[..] {
            [name, _, gid, ..] if name == group => gid.parse().ok(),
            _ => None,
        }
    });
    found.ok_or_else(|| {
        invalid(
            "fs.client_group",
            "is neither a group id nor a name in the group file",
        )
    })
}

impl FsSettings {
    /// The settings of `table`; `groups` is the text of the group file.
    ///
    /// # Errors
    /// A [`ConfigError`] that names the key: one needed and left out, a path that is not
    /// absolute, an owner or a group that is not one, a value out of its range.
    pub(super) fn of(table: FsTable, groups: &str) -> Result<Self, ConfigError> {
        let mountpoint = table
            .mountpoint
            .unwrap_or_else(|| PathBuf::from(DEFAULT_MOUNTPOINT));
        let runtime_dir = table
            .runtime_dir
            .unwrap_or_else(|| PathBuf::from(DEFAULT_RUNTIME_DIR));
        let control_socket = table
            .control_socket
            .unwrap_or_else(|| runtime_dir.join(DEFAULT_CONTROL_SOCKET));
        let socket_length = control_socket.as_os_str().len();
        if socket_length > MOST_SOCKET_PATH {
            let why = format!(
                "is {socket_length} bytes; the path of a Unix socket is at most {MOST_SOCKET_PATH}"
            );
            return Err(invalid("fs.control_socket", why));
        }
        let timeout = table
            .fetch_timeout
            .map_or(DEFAULT_FETCH_TIMEOUT, |dur| dur.0);
        if timeout < MS || timeout > HOUR {
            let why = format!("{}ms is not between 1ms and 1h", timeout.as_millis());
            return Err(invalid("fs.fetch_timeout", why));
        }
        let threads = within(
            "fs",
            "threads",
            table.threads.unwrap_or(DEFAULT_THREADS),
            1,
            1024,
        )?;
        let group = require("fs", "client_group", table.client_group)?;
        let owner = require("fs", "owner", table.owner)?;
        Ok(Self {
            mountpoint: absolute("fs.mountpoint", mountpoint)?,
            runtime_dir: absolute("fs.runtime_dir", runtime_dir)?,
            control_socket: absolute("fs.control_socket", control_socket)?,
            client_group: group_of(&group, groups)?,
            owner: owner_of(&owner)?,
            fill_socket: absolute(
                "fs.fill_socket",
                require("fs", "fill_socket", table.fill_socket)?,
            )?,
            cache_path: absolute(
                "fs.cache_path",
                require("fs", "cache_path", table.cache_path)?,
            )?,
            fetch_timeout: timeout,
            threads: threads as usize,
            allow_other: true,
        })
    }
}

/// The settings in the text of a file, and a warning for each key of the table that this release
/// does not know (a key a later release added is not a fault: the node starts without it).
///
/// # Errors
/// [`ConfigError::Syntax`] for a file that is not TOML or has no `[fs]` table or a value of the
/// wrong type, and those of [`FsSettings::of`].
pub(super) fn from_text(text: &str, groups: &str) -> Result<FsSettings, ConfigError> {
    let file: File = toml::from_str(text).map_err(|error| syntax(text, &error))?;
    let whole = text.parse::<toml::Table>().unwrap_or_default();
    let known = fields_of::<FsTable>();
    let table = whole.get("fs").and_then(toml::Value::as_table);
    for key in table.into_iter().flat_map(|table| table.keys()) {
        if !known.contains(&key.as_str()) {
            tracing::warn!("tierstore fs: fs.{key} is not a key of this release and is ignored");
        }
    }
    FsSettings::of(file.fs, groups)
}

/// The settings of the file at `path`.
///
/// # Errors
/// [`ConfigError::Read`] for a file that cannot be read, and those of [`from_text`]; a group file
/// that cannot be read is an empty one (a group given by number needs none).
pub(in crate::server) fn load(path: &Path) -> Result<FsSettings, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let groups = std::fs::read_to_string(GROUP_FILE).unwrap_or_default();
    from_text(&text, &groups)
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
