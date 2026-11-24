//! The settings of `tierstore fs`: the `[fs]` table of the file `--config` names, the file of this
//! process alone (the S3 instance reads a file of its own, which has no `[fs]` table).
//!
//! | key | meaning | default |
//! |---|---|---|
//! | `fs.mountpoint` | where the one FUSE mount is made | `/run/tierstore/fs` |
//! | `fs.runtime_dir` | the state that outlives the process | `/run/tierstore-fs` |
//! | `fs.control_socket` | the socket clients attach on | `<runtime_dir>/control.sock` |
//! | `fs.client_group` | the group (a name of this system, or a number) that may use that socket | needed |
//! | `fs.owner` | `user:group` every entry of every tree reports, each side a name of this system or a number | needed |
//! | `fs.fill_socket` | the fill socket of the node's tierstore | needed |
//! | `fs.cache_path` | the node's `local_cache.path` | needed |
//! | `fs.fetch_timeout` | how long a fill, or a closed fill socket, is waited for | `60s` |
//! | `fs.threads` | threads that serve the kernel's requests, and as many that make the opens that wait on a fill (`fetches.rs`), at most 252 | `16` |
//! | `[fs.nfs]` | the settings of the NFS frontend (macOS), see `nfs/config.rs` | its defaults |
//! | `fs.handoff_socket` | the socket a new instance asks the running one for the mount on (`handoff-fs-v1`) | `<runtime_dir>/handoff.sock` |
//! | `fs.handoff_wait` | how long the hand-over of the mount may last, from the connect to the end | `120s` |
//! | `fs.reattach_concurrency` | how many trees a start reads the manifests of at once when it makes the attachments again, before it serves (1 reads them one after the other) | `16` |
//! | `fs.recover_wait` | after a restart on a kernel without resend: how long the connection may show requests no one answers before it is aborted | `30s` |
//! | `fs.tree_idle_secs` | how long, in seconds, a tree a lookup attached stays attached with no request naming it | `900` |
//! | `[fs.put]` | the put directory, `<mountpoint>/put/`, see `put/config.rs` | not served |
//!
//! `fs.passthrough` and `fs.max_backing_ids` were removed with passthrough, after tierstore 0.3.121:
//! a file that has them starts, and each is said in a warning and ignored (`REMOVED`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::server::config::{ConfigError, invalid, require, within};
use crate::server::config_handoff::MOST_SOCKET_PATH;
use crate::server::config_syntax::syntax;
use crate::server::config_tuning::{HOUR, MS};
use crate::server::config_units::Dur;
use crate::server::config_unknown::fields_of;

use super::groups::{self, Database, Found};
use super::nfs::config::{NfsSettings, NfsTable, ignored as nfs_ignored};
use super::put::config::{self as put_config, PutConfig, PutTable};

/// The lookup of a group or user name: [`groups::system`], or a test's table.
pub(in crate::server) type Lookup<'a> = &'a dyn Fn(Database, &str) -> Found;

/// The lookup of a system whose one name is `builders`, id 2001, as a group and as a user: the
/// tests' table.
#[cfg(test)]
pub(in crate::server) fn builders(_database: Database, name: &str) -> Found {
    Ok((name == "builders").then_some(2001))
}

/// The mount point when the file leaves out `fs.mountpoint`.
pub(in crate::server) const DEFAULT_MOUNTPOINT: &str = "/run/tierstore/fs";

/// The runtime directory when the file leaves out `fs.runtime_dir`: the `RuntimeDirectory=` of the
/// unit.
const DEFAULT_RUNTIME_DIR: &str = "/run/tierstore-fs";

/// The name of the control socket in the runtime directory when the file leaves out
/// `fs.control_socket`.
const DEFAULT_CONTROL_SOCKET: &str = "control.sock";

/// The most time a fill is waited for by default: a fill that fails sooner is not waited for.
pub(super) const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// The threads that serve the kernel's requests when the file leaves out `fs.threads`.
const DEFAULT_THREADS: u32 = 16;

/// The most threads `fs.threads` may name: each reads its own copy of the connection's descriptor,
/// and a `tsfuse` session holds at most `tsfuse::MAX_DEVICES` (the descriptors one `SCM_RIGHTS`
/// message carries, less one).
const MOST_THREADS: u32 = 252;

/// The name of the hand-over socket in the runtime directory when the file leaves out
/// `fs.handoff_socket`.
const DEFAULT_HANDOFF_SOCKET: &str = "handoff.sock";

/// How long a hand-over of the mount may last when the file leaves out `fs.handoff_wait`.
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

/// The keys of `[fs]` that an earlier release read and this one does not, each with why: a file
/// written for that release starts with them, and each is said.
const REMOVED: [(&str, &str); 2] = [
    (
        "passthrough",
        "removed after tierstore 0.3.121, with passthrough: every open is a cached open",
    ),
    (
        "max_backing_ids",
        "removed after tierstore 0.3.121, with passthrough: no backing id is registered",
    ),
];

/// The warning for the key `key` of `[fs]` that this release does not read.
fn ignored(key: &str) -> String {
    match REMOVED.iter().find(|(removed, _)| *removed == key) {
        Some((_, why)) => format!("tierstore fs: fs.{key} is ignored: {why}"),
        None => format!("tierstore fs: fs.{key} is not a key of this release and is ignored"),
    }
}

/// The file: only its `[fs]` table is read.
#[derive(Debug, Deserialize)]
struct File {
    fs: FsTable,
}

/// `[fs]`, and the params of an `fs` interface (which leave out `fill_socket`, `cache_path` and
/// `handoff_socket`: see `config_fs.rs`).
#[derive(Debug, Default, Deserialize)]
pub(in crate::server) struct FsTable {
    pub(in crate::server) mountpoint: Option<PathBuf>,
    pub(in crate::server) runtime_dir: Option<PathBuf>,
    pub(in crate::server) control_socket: Option<PathBuf>,
    pub(in crate::server) client_group: Option<String>,
    pub(in crate::server) owner: Option<String>,
    pub(in crate::server) fill_socket: Option<PathBuf>,
    pub(in crate::server) cache_path: Option<PathBuf>,
    pub(in crate::server) fetch_timeout: Option<Dur>,
    pub(in crate::server) threads: Option<u32>,
    pub(in crate::server) nfs: Option<NfsTable>,
    pub(in crate::server) handoff_socket: Option<PathBuf>,
    pub(in crate::server) handoff_wait: Option<Dur>,
    pub(in crate::server) recover_wait: Option<Dur>,
    pub(in crate::server) reattach_concurrency: Option<u32>,
    pub(in crate::server) tree_idle_secs: Option<u64>,
    pub(in crate::server) put: Option<PutTable>,
}

/// What the file system runs with: `tierstore fs`, from its `[fs]` table, or the `fs` interface
/// of a build node's file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct FsSettings {
    pub mountpoint: PathBuf,
    pub runtime_dir: PathBuf,
    pub control_socket: PathBuf,
    /// The group id of the control socket.
    pub client_group: u32,
    /// The user and group id every entry reports.
    pub owner: (u32, u32),
    /// The fill socket the core asks for fills on: `fs.fill_socket` of `tierstore fs`; for an
    /// `fs` interface, the node's own `local_cache.fill_socket` (or the one in its state
    /// directory), none when the node has none, as a client whose socket is not there.
    pub fill_socket: Option<PathBuf>,
    /// The node's cache directory: `fs.cache_path`, or `local_cache.path` of an `fs` interface's
    /// file.
    pub cache_path: PathBuf,
    pub fetch_timeout: Duration,
    pub threads: usize,
    /// The socket a new instance asks the running one for the mount on.
    pub handoff_socket: PathBuf,
    /// How long a hand-over of the mount may last.
    pub handoff_wait: Duration,
    /// How long a restarted instance on a kernel without resend watches for requests no one
    /// answers.
    pub recover_wait: Duration,
    /// How many trees a start reads at once when it makes the attachments again.
    pub reattach_concurrency: usize,
    /// How long a tree a lookup attached stays attached with no request naming it.
    pub tree_idle: Duration,
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

/// What the node gives the file system beside its table: the fill socket it asks for fills on,
/// if any, and the cache directory it reads.
struct Node {
    fill_socket: Option<PathBuf>,
    cache_path: PathBuf,
}

impl FsSettings {
    /// The settings of `table`, the `[fs]` table of the file of `tierstore fs`; `lookup` looks a
    /// group or user name up.
    ///
    /// # Errors
    /// A [`ConfigError`] that names the key: one needed and left out, a path that is not
    /// absolute, an owner or a group that is not one, a value out of its range.
    pub(super) fn of(mut table: FsTable, lookup: Lookup<'_>) -> Result<Self, ConfigError> {
        let (fill_socket, cache_path) = (table.fill_socket.take(), table.cache_path.take());
        let node = move || {
            let fill_socket = require("fs", "fill_socket", fill_socket)?;
            let fill_socket = Some(absolute("fs.fill_socket", fill_socket)?);
            let cache_path = require("fs", "cache_path", cache_path)?;
            let cache_path = absolute("fs.cache_path", cache_path)?;
            Ok(Node {
                fill_socket,
                cache_path,
            })
        };
        Self::made("fs", table, node, lookup)
    }

    /// The settings of `table`, the params at `place` (`interface[2].params`) of an `fs`
    /// interface of a build node whose cache directory is `cache_path` and whose own fill socket
    /// is `fill_socket`; `lookup` looks a group or user name up. The table has no `fill_socket`,
    /// `cache_path` or `handoff_socket` (the interface refuses them as params), so the hand-over
    /// socket is `handoff.sock` in the runtime directory.
    ///
    /// # Errors
    /// Those of [`FsSettings::of`] but the keys the node gives, each naming the param.
    pub(in crate::server) fn of_interface(
        place: &str,
        table: FsTable,
        (cache_path, fill_socket): (PathBuf, Option<PathBuf>),
        lookup: Lookup<'_>,
    ) -> Result<Self, ConfigError> {
        let node = || {
            Ok(Node {
                fill_socket,
                cache_path,
            })
        };
        Self::made(place, table, node, lookup)
    }

    /// The settings of `table`, whose keys are named under `place` (`fs`, or the params of an
    /// interface), with what `node` gives, which is asked where the `[fs]` table's own keys of
    /// the node are checked, after the owner.
    fn made(
        place: &str,
        table: FsTable,
        node: impl FnOnce() -> Result<Node, ConfigError>,
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
        let handoff_socket = table
            .handoff_socket
            .unwrap_or_else(|| runtime_dir.join(DEFAULT_HANDOFF_SOCKET));
        for (name, socket) in [
            ("control_socket", &control_socket),
            ("handoff_socket", &handoff_socket),
        ] {
            let length = socket.as_os_str().len();
            if length > MOST_SOCKET_PATH {
                let why = format!(
                    "is {length} bytes; the path of a Unix socket is at most {MOST_SOCKET_PATH}"
                );
                return Err(invalid(&key(name), why));
            }
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
        let group = require(place, "client_group", table.client_group)?;
        let owner = require(place, "owner", table.owner)?;
        let mountpoint = absolute(&key("mountpoint"), mountpoint)?;
        let runtime_dir = absolute(&key("runtime_dir"), runtime_dir)?;
        let control_socket = absolute(&key("control_socket"), control_socket)?;
        let client_group = id_of(&key("client_group"), Database::Group, &group, lookup)?;
        let owner = owner_of(&key("owner"), &owner, lookup)?;
        let node = node()?;
        Ok(Self {
            mountpoint,
            runtime_dir,
            control_socket,
            client_group,
            owner,
            fill_socket: node.fill_socket,
            cache_path: node.cache_path,
            fetch_timeout: timeout,
            threads: threads as usize,
            handoff_socket: absolute(&key("handoff_socket"), handoff_socket)?,
            handoff_wait,
            recover_wait,
            reattach_concurrency: reattach_concurrency as usize,
            tree_idle: Duration::from_secs(tree_idle_secs),
            allow_other: true,
            nfs: NfsSettings::of(place, table.nfs, timeout)?,
            put: put_config::of(&key("put"), table.put)?,
        })
    }
}

/// The settings in the text of a file, and a warning for each key of the table that this release
/// does not know (a key a later release added is not a fault: the node starts without it).
///
/// # Errors
/// [`ConfigError::Syntax`] for a file that is not TOML or has no `[fs]` table or a value of the
/// wrong type, and those of [`FsSettings::of`].
pub(in crate::server) fn from_text(
    text: &str,
    lookup: Lookup<'_>,
) -> Result<FsSettings, ConfigError> {
    let file: File = toml::from_str(text).map_err(|error| syntax(text, &error))?;
    let whole = text.parse::<toml::Table>().unwrap_or_default();
    let known = fields_of::<FsTable>();
    let table = whole.get("fs").and_then(toml::Value::as_table);
    for key in table.into_iter().flat_map(|table| table.keys()) {
        if !known.contains(&key.as_str()) {
            tracing::warn!("{}", ignored(key));
        }
    }
    let nfs_known = fields_of::<NfsTable>();
    let nfs = table
        .and_then(|table| table.get("nfs"))
        .and_then(toml::Value::as_table);
    for key in nfs.into_iter().flat_map(|table| table.keys()) {
        if !nfs_known.contains(&key.as_str()) {
            tracing::warn!("{}", nfs_ignored(key));
        }
    }
    let put_known = fields_of::<PutTable>();
    let put = table
        .and_then(|table| table.get("put"))
        .and_then(toml::Value::as_table);
    for key in put.into_iter().flat_map(|table| table.keys()) {
        if !put_known.contains(&key.as_str()) {
            tracing::warn!("tierstore fs: fs.put.{key} is not a key of this release; ignored");
        }
    }
    FsSettings::of(file.fs, lookup)
}

/// The settings of the file at `path`.
///
/// # Errors
/// [`ConfigError::Read`] for a file that cannot be read, and those of [`from_text`]. A group given
/// by name (`fs.client_group`, and each side of `fs.owner`) is looked up through the system
/// ([`groups::system`]); one given by number needs no lookup.
pub(in crate::server) fn load(path: &Path) -> Result<FsSettings, ConfigError> {
    load_with(path, &groups::system)
}

/// [`load`] with `lookup` as the lookup of group and user names.
///
/// # Errors
/// Those of [`load`].
pub(in crate::server) fn load_with(
    path: &Path,
    lookup: Lookup<'_>,
) -> Result<FsSettings, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    from_text(&text, lookup)
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
