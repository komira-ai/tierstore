//! The settings of the file system: the params of the `fs` interface of a build node's file, which
//! `config_fs.rs` reads and which [`FsSettings::of_interface`] checks and completes.
//!
//! | param | meaning | default |
//! |---|---|---|
//! | `mountpoint` | where the one FUSE mount is made | `/run/tierstore/fs` |
//! | `runtime_dir` | the state that outlives the process | `/run/tierstore-fs` |
//! | `control_socket` | the socket clients attach on | `<runtime_dir>/control.sock` |
//! | `client_group` | the group (a name of this system, or a number) that may use that socket | needed |
//! | `owner` | `user:group` every entry of every tree reports, each side a name of this system or a number | needed |
//! | `fetch_timeout` | how long a fill, or the write role of the node's cache, is waited for | `60s` |
//! | `threads` | threads that serve the kernel's requests, and as many that make the opens that wait on a fill (`fetches.rs`), at most 252 | `16` |
//! | `nfs` | the settings of the NFS frontend (macOS), see `nfs/config.rs` | its defaults |
//! | `handoff_wait` | how long the hand-over of the mount may last, from the connect to the end | `120s` |
//! | `reattach_concurrency` | how many trees a start reads the manifests of at once when it makes the attachments again, before it serves (1 reads them one after the other) | `16` |
//! | `recover_wait` | after a restart on a kernel without resend: how long the connection may show requests no one answers before it is aborted | `30s` |
//! | `tree_idle_secs` | how long, in seconds, a tree a lookup attached stays attached with no request naming it | `900` |
//! | `tree_list_ram_bytes` | the most bytes of version 2 tree lists the mount keeps mapped before it releases those no run attaches and no request reads (`core/lists.rs`); at least a page, 4096 | `536870912` (512 MiB) |
//! | `put` | the put directory, `<mountpoint>/put/`, see `put/config.rs` | not served |
//!
//! The cache directory is `local_cache.path` of the node's file.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::server::config::{ConfigError, invalid, require, within};
use crate::server::config_handoff::MOST_SOCKET_PATH;
use crate::server::config_tuning::{HOUR, MS};
use crate::server::config_units::Dur;

use super::groups::{Database, Found};
use super::nfs::config::{NfsSettings, NfsTable};
use super::put::config::{self as put_config, PutConfig, PutTable};

/// The lookup of a group or user name: [`super::groups::system`], or a test's table.
pub(in crate::server) type Lookup<'a> = &'a dyn Fn(Database, &str) -> Found;

/// The lookup of a system whose one name is `builders`, id 2001, as a group and as a user: the
/// tests' table.
#[cfg(test)]
pub(in crate::server) fn builders(_database: Database, name: &str) -> Found {
    Ok((name == "builders").then_some(2001))
}

/// The place the params of [`from_params`] are at.
#[cfg(test)]
pub(in crate::server) const TEST_PLACE: &str = "interface[1].params";

/// The settings of the params `text` (the keys of the `[interface.params]` of an `fs` interface,
/// its sub-tables after them) of a node whose cache directory is `/var/cache/tierstore`, checked
/// as an interface's are, at [`TEST_PLACE`]: the tests' way to the interface's settings with a
/// lookup of their own.
#[cfg(test)]
pub(in crate::server) fn from_params(
    text: &str,
    lookup: Lookup<'_>,
) -> Result<FsSettings, ConfigError> {
    let table: FsTable = toml::from_str(text).expect("params of the types of the table");
    let cache = PathBuf::from("/var/cache/tierstore");
    FsSettings::of_interface(TEST_PLACE, table, cache, lookup)
}

/// The mount point when the file leaves out `fs.mountpoint`.
pub(in crate::server) const DEFAULT_MOUNTPOINT: &str = "/run/tierstore/fs";

/// The runtime directory when the file leaves out `fs.runtime_dir`: the `RuntimeDirectory=` of the
/// unit.
pub(in crate::server) const DEFAULT_RUNTIME_DIR: &str = "/run/tierstore-fs";

/// The name of the control socket in the runtime directory when the file leaves out
/// `fs.control_socket`.
pub(in crate::server) const DEFAULT_CONTROL_SOCKET: &str = "control.sock";

/// The most time a fill is waited for by default: a fill that fails sooner is not waited for.
pub(super) const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// The threads that serve the kernel's requests when the file leaves out `fs.threads`.
const DEFAULT_THREADS: u32 = 16;

/// The most threads `fs.threads` may name: each reads its own copy of the connection's descriptor,
/// and a `tsfuse` session holds at most `tsfuse::MAX_DEVICES` (the descriptors one `SCM_RIGHTS`
/// message carries, less one).
const MOST_THREADS: u32 = 252;

/// How long a hand-over of the mount may last when the file leaves out `handoff_wait`.
const DEFAULT_HANDOFF_WAIT: Duration = Duration::from_secs(120);

/// How long a restarted instance watches for requests no one answers when the file leaves out
/// `fs.recover_wait`.
const DEFAULT_RECOVER_WAIT: Duration = Duration::from_secs(30);

/// The trees a start reads at once when the file leaves out `fs.reattach_concurrency`.
const DEFAULT_REATTACH_CONCURRENCY: u32 = 16;

/// The most `fs.reattach_concurrency` may be.
const MOST_REATTACH_CONCURRENCY: u32 = 1024;

/// The seconds a tree attached by a lookup stays attached with no use when the file leaves out
/// `fs.tree_idle_secs`.
const DEFAULT_TREE_IDLE_SECS: u64 = 900;

/// The most `fs.tree_idle_secs` may be: a week.
const MOST_TREE_IDLE_SECS: u64 = 7 * 24 * 3600;

/// The bytes of tree lists the mount keeps mapped when the file leaves out
/// `fs.tree_list_ram_bytes`: 512 MiB, about 1,700 lists of 0.3 MiB or 73 of 7 MiB.
pub(in crate::server) const DEFAULT_TREE_LIST_RAM_BYTES: u64 = 512 << 20;

/// The least `fs.tree_list_ram_bytes` may be: one page, which a mapping takes at least. The
/// control socket holds a value it sets to the same bound (`control.rs`).
pub(super) const LEAST_TREE_LIST_RAM_BYTES: u64 = 4096;

/// The params of an `fs` interface (`config_fs.rs` refuses `fill_socket`, `cache_path` and
/// `handoff_socket` by name).
#[derive(Debug, Default, Deserialize)]
pub(in crate::server) struct FsTable {
    pub(in crate::server) mountpoint: Option<PathBuf>,
    pub(in crate::server) runtime_dir: Option<PathBuf>,
    pub(in crate::server) control_socket: Option<PathBuf>,
    pub(in crate::server) client_group: Option<String>,
    pub(in crate::server) owner: Option<String>,
    pub(in crate::server) fetch_timeout: Option<Dur>,
    pub(in crate::server) threads: Option<u32>,
    pub(in crate::server) nfs: Option<NfsTable>,
    pub(in crate::server) handoff_wait: Option<Dur>,
    pub(in crate::server) recover_wait: Option<Dur>,
    pub(in crate::server) reattach_concurrency: Option<u32>,
    pub(in crate::server) tree_idle_secs: Option<u64>,
    pub(in crate::server) tree_list_ram_bytes: Option<u64>,
    pub(in crate::server) put: Option<PutTable>,
}

/// What the file system runs with: the `fs` interface of a build node's file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct FsSettings {
    pub mountpoint: PathBuf,
    pub runtime_dir: PathBuf,
    pub control_socket: PathBuf,
    /// The group id of the control socket.
    pub client_group: u32,
    /// The user and group id every entry reports.
    pub owner: (u32, u32),
    /// The node's cache directory: `local_cache.path` of the `fs` interface's file.
    pub cache_path: PathBuf,
    pub fetch_timeout: Duration,
    pub threads: usize,
    /// How long a hand-over of the mount may last.
    pub handoff_wait: Duration,
    /// How long a restarted instance on a kernel without resend watches for requests no one
    /// answers.
    pub recover_wait: Duration,
    /// How many trees a start reads at once when it makes the attachments again.
    pub reattach_concurrency: usize,
    /// How long a tree a lookup attached stays attached with no request naming it.
    pub tree_idle: Duration,
    /// The most bytes of tree lists the mount keeps mapped (`fs.tree_list_ram_bytes`).
    pub tree_list_ram_bytes: u64,
    /// Whether users other than the one who mounts may use the mount: always, but in the tests,
    /// where the mounting user is not root and `fusermount3` allows it only by `/etc/fuse.conf`.
    pub allow_other: bool,
    /// The settings of the NFS frontend.
    pub nfs: NfsSettings,
    /// The settings of the put directory; none when it is not served.
    pub put: Option<PutConfig>,
}

/// `path`, which is the value of `key`, if it is absolute.
fn absolute(key: &str, path: PathBuf) -> Result<PathBuf, ConfigError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(invalid(key, "must be an absolute path"))
    }
}

/// `wait`, which is the value of `key`, if it is between a second and an hour.
fn bounded(key: &str, wait: Duration) -> Result<Duration, ConfigError> {
    if wait < Duration::from_secs(1) || wait > HOUR {
        let why = format!("{}ms is not between 1s and 1h", wait.as_millis());
        return Err(invalid(key, why));
    }
    Ok(wait)
}

/// The id that `text`, the value of `key`, names in `database`: a number, or a name that `lookup`,
/// the system's lookup of names, finds.
pub(in crate::server) fn id_of(
    key: &str,
    database: Database,
    text: &str,
    lookup: Lookup<'_>,
) -> Result<u32, ConfigError> {
    let noun = database.noun();
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text
            .parse()
            .map_err(|_| invalid(key, format!("the {noun} id {text} is too large")));
    }
    match lookup(database, text) {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(invalid(
            key,
            format!("{text:?} is neither a {noun} id nor the name of a {noun} of this system"),
        )),
        Err(why) => Err(invalid(
            key,
            format!("the {noun} {text} could not be looked up: {why}"),
        )),
    }
}

/// `user:group`, each a number or a name of this system, as two ids; `key` is the key that holds
/// it.
fn owner_of(key: &str, text: &str, lookup: Lookup<'_>) -> Result<(u32, u32), ConfigError> {
    let mut parts = text.split(':');
    let (Some(user), Some(group), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(not_two_parts(key, text));
    };
    if user.is_empty() || group.is_empty() {
        return Err(not_two_parts(key, text));
    }
    Ok((
        id_of(key, Database::User, user, lookup)?,
        id_of(key, Database::Group, group, lookup)?,
    ))
}

/// The refusal of an owner (the value of `key`) that is not a user and a group joined by one
/// colon.
fn not_two_parts(key: &str, text: &str) -> ConfigError {
    invalid(
        key,
        format!(
            "must be user:group, two parts joined by one colon, each a number or a name of this \
             system, not {text:?}"
        ),
    )
}

impl FsSettings {
    /// The settings of `table`, the params at `place` (`interface[2].params`) of an `fs`
    /// interface of a build node whose cache directory is `cache_path`; `lookup` looks a group or
    /// user name up. The table has no `fill_socket`, `cache_path` or `handoff_socket` (the
    /// interface refuses them as params).
    ///
    /// # Errors
    /// A [`ConfigError`] that names the param: one needed and left out, a path that is not
    /// absolute, an owner or a group that is not one, a value out of its range.
    pub(in crate::server) fn of_interface(
        place: &str,
        table: FsTable,
        cache_path: PathBuf,
        lookup: Lookup<'_>,
    ) -> Result<Self, ConfigError> {
        let key = |name: &str| format!("{place}.{name}");
        let mountpoint = table
            .mountpoint
            .unwrap_or_else(|| PathBuf::from(DEFAULT_MOUNTPOINT));
        let runtime_dir = table
            .runtime_dir
            .unwrap_or_else(|| PathBuf::from(DEFAULT_RUNTIME_DIR));
        let control_socket = table
            .control_socket
            .unwrap_or_else(|| runtime_dir.join(DEFAULT_CONTROL_SOCKET));
        let length = control_socket.as_os_str().len();
        if length > MOST_SOCKET_PATH {
            let why = format!(
                "is {length} bytes; the path of a Unix socket is at most {MOST_SOCKET_PATH}"
            );
            return Err(invalid(&key("control_socket"), why));
        }
        let handoff_wait = bounded(
            &key("handoff_wait"),
            table.handoff_wait.map_or(DEFAULT_HANDOFF_WAIT, |dur| dur.0),
        )?;
        let recover_wait = bounded(
            &key("recover_wait"),
            table.recover_wait.map_or(DEFAULT_RECOVER_WAIT, |dur| dur.0),
        )?;
        let timeout = table
            .fetch_timeout
            .map_or(DEFAULT_FETCH_TIMEOUT, |dur| dur.0);
        if timeout < MS || timeout > HOUR {
            let why = format!("{}ms is not between 1ms and 1h", timeout.as_millis());
            return Err(invalid(&key("fetch_timeout"), why));
        }
        let threads = within(
            place,
            "threads",
            table.threads.unwrap_or(DEFAULT_THREADS),
            1,
            MOST_THREADS,
        )?;
        let reattach_concurrency = within(
            place,
            "reattach_concurrency",
            table
                .reattach_concurrency
                .unwrap_or(DEFAULT_REATTACH_CONCURRENCY),
            1,
            MOST_REATTACH_CONCURRENCY,
        )?;
        let tree_idle_secs = within(
            place,
            "tree_idle_secs",
            table.tree_idle_secs.unwrap_or(DEFAULT_TREE_IDLE_SECS),
            1,
            MOST_TREE_IDLE_SECS,
        )?;
        let tree_list_ram_bytes = within(
            place,
            "tree_list_ram_bytes",
            table
                .tree_list_ram_bytes
                .unwrap_or(DEFAULT_TREE_LIST_RAM_BYTES),
            LEAST_TREE_LIST_RAM_BYTES,
            u64::MAX,
        )?;
        let group = require(place, "client_group", table.client_group)?;
        let owner = require(place, "owner", table.owner)?;
        let mountpoint = absolute(&key("mountpoint"), mountpoint)?;
        let runtime_dir = absolute(&key("runtime_dir"), runtime_dir)?;
        let control_socket = absolute(&key("control_socket"), control_socket)?;
        let client_group = id_of(&key("client_group"), Database::Group, &group, lookup)?;
        let owner = owner_of(&key("owner"), &owner, lookup)?;
        Ok(Self {
            mountpoint,
            runtime_dir,
            control_socket,
            client_group,
            owner,
            cache_path,
            fetch_timeout: timeout,
            threads: threads as usize,
            handoff_wait,
            recover_wait,
            reattach_concurrency: reattach_concurrency as usize,
            tree_idle: Duration::from_secs(tree_idle_secs),
            tree_list_ram_bytes,
            allow_other: true,
            nfs: NfsSettings::of(place, table.nfs, timeout)?,
            put: put_config::of(&key("put"), table.put)?,
        })
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
