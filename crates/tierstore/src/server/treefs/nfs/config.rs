//! The `[fs.nfs]` table: the settings of the NFS frontend (macOS), which a file without the table
//! gets the defaults of, so that a file written before this table existed loads unchanged.
//!
//! | key | meaning | default |
//! |---|---|---|
//! | `fs.nfs.listen` | the loopback address and port the NFS and MOUNT protocols are served on | `127.0.0.1:12049` |
//! | `fs.nfs.mount` | make the mount at `fs.mountpoint` (`false`: serve only, for a mount made by hand or by an earlier instance) | `true` |
//! | `fs.nfs.mount_user` | the user the mount is made as, who must own `fs.mountpoint` | the user of the process |
//! | `fs.nfs.mount_wait` | how long a mount that fails is tried again | `30s` |
//! | `fs.nfs.mount_retry` | the pause between two tries | `500ms` |
//! | `fs.nfs.fd_cache_shards` | shards of the cache of open cache files | `64` |
//! | `fs.nfs.fd_cache_per_shard` | open cache files kept in one shard | `64` |
//! | `fs.nfs.allowed_uids` | the user ids whose requests are served (`AUTH_SYS`); a fence against other local users, not authentication | the mount user and the owner (`fs.owner`) |
//! | `fs.nfs.jukebox_after` | how long a read waits for a fill before the client is told to ask again | `5s` |

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde::Deserialize;

use crate::server::config::{ConfigError, invalid, within};
use crate::server::config_tuning::{HOUR, MS};
use crate::server::config_units::Dur;

/// The address the protocols are served on when the file leaves out `fs.nfs.listen`.
const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12_049);

/// The time a failing mount is tried again when the file leaves out `fs.nfs.mount_wait`.
const DEFAULT_MOUNT_WAIT: Duration = Duration::from_secs(30);

/// The pause between two tries when the file leaves out `fs.nfs.mount_retry`.
const DEFAULT_MOUNT_RETRY: Duration = Duration::from_millis(500);

/// The shards of the cache of open files when the file leaves out `fs.nfs.fd_cache_shards`.
const DEFAULT_SHARDS: u32 = 64;

/// The open files of a shard when the file leaves out `fs.nfs.fd_cache_per_shard`.
const DEFAULT_PER_SHARD: u32 = 64;

/// The wait for a fill when the file leaves out `fs.nfs.jukebox_after`.
const DEFAULT_JUKEBOX_AFTER: Duration = Duration::from_secs(5);

/// The longest a name of a user may be.
const MOST_USER_NAME: usize = 64;

/// `[fs.nfs]`.
#[derive(Debug, Deserialize)]
pub(in crate::server) struct NfsTable {
    listen: Option<String>,
    mount: Option<bool>,
    mount_user: Option<String>,
    mount_wait: Option<Dur>,
    mount_retry: Option<Dur>,
    fd_cache_shards: Option<u32>,
    fd_cache_per_shard: Option<u32>,
    allowed_uids: Option<Vec<u32>>,
    jukebox_after: Option<Dur>,
}

/// What the NFS frontend runs with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct NfsSettings {
    /// Where the protocols are served; a loopback address.
    pub listen: SocketAddr,
    /// Whether the process makes the mount.
    pub mount: bool,
    /// The user the mount is made as; none is the user of the process.
    pub mount_user: Option<String>,
    pub mount_wait: Duration,
    pub mount_retry: Duration,
    pub fd_cache_shards: usize,
    pub fd_cache_per_shard: usize,
    /// The user ids that are served; none is the mount user and the owner of the entries.
    pub allowed_uids: Option<Vec<u32>>,
    pub jukebox_after: Duration,
}

impl Default for NfsSettings {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN,
            mount: true,
            mount_user: None,
            mount_wait: DEFAULT_MOUNT_WAIT,
            mount_retry: DEFAULT_MOUNT_RETRY,
            fd_cache_shards: DEFAULT_SHARDS as usize,
            fd_cache_per_shard: DEFAULT_PER_SHARD as usize,
            allowed_uids: None,
            jukebox_after: DEFAULT_JUKEBOX_AFTER,
        }
    }
}

/// The address `text` names, which must be on loopback.
fn listen_of(text: &str) -> Result<SocketAddr, ConfigError> {
    let address: SocketAddr = text.parse().map_err(|_| {
        invalid(
            "fs.nfs.listen",
            "must be an address and a port, as 127.0.0.1:12049",
        )
    })?;
    match address.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => Ok(address),
        _ => Err(invalid(
            "fs.nfs.listen",
            "must be an IPv4 loopback address: the protocol has no authentication a network could rely on",
        )),
    }
}

/// `user`, if it is a name that cannot be taken for an option or a path.
fn user_of(user: String) -> Result<String, ConfigError> {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    if user.is_empty()
        || user.len() > MOST_USER_NAME
        || user.starts_with('-')
        || !user.chars().all(plain)
    {
        return Err(invalid(
            "fs.nfs.mount_user",
            "must be a user name of letters, digits, `_`, `.` and `-`, not starting with `-`",
        ));
    }
    Ok(user)
}

/// `value` if it is between `low` and `high`.
fn span(
    key: &str,
    value: Option<Dur>,
    default: Duration,
    (low, high): (Duration, Duration),
) -> Result<Duration, ConfigError> {
    let value = value.map_or(default, |dur| dur.0);
    if value < low || value > high {
        let why = format!(
            "{}ms is not between {}ms and {}ms",
            value.as_millis(),
            low.as_millis(),
            high.as_millis()
        );
        return Err(invalid(&format!("fs.nfs.{key}"), why));
    }
    Ok(value)
}

impl NfsSettings {
    /// The settings of `table`, or the defaults where the file has none.
    ///
    /// # Errors
    /// A [`ConfigError`] that names the key: an address that is not on loopback, a user name that
    /// is not one, an empty list of users, a value out of its range.
    pub(in crate::server) fn of(table: Option<NfsTable>) -> Result<Self, ConfigError> {
        let Some(table) = table else {
            return Ok(Self::default());
        };
        let shards = within(
            "fs.nfs",
            "fd_cache_shards",
            table.fd_cache_shards.unwrap_or(DEFAULT_SHARDS),
            1,
            4096,
        )?;
        let per_shard = within(
            "fs.nfs",
            "fd_cache_per_shard",
            table.fd_cache_per_shard.unwrap_or(DEFAULT_PER_SHARD),
            1,
            65_536,
        )?;
        if table.allowed_uids.as_ref().is_some_and(Vec::is_empty) {
            return Err(invalid(
                "fs.nfs.allowed_uids",
                "is empty: no request would be served",
            ));
        }
        Ok(Self {
            listen: table
                .listen
                .as_deref()
                .map_or(Ok(DEFAULT_LISTEN), listen_of)?,
            mount: table.mount.unwrap_or(true),
            mount_user: table.mount_user.map(user_of).transpose()?,
            mount_wait: span(
                "mount_wait",
                table.mount_wait,
                DEFAULT_MOUNT_WAIT,
                (Duration::from_secs(1), HOUR),
            )?,
            mount_retry: span(
                "mount_retry",
                table.mount_retry,
                DEFAULT_MOUNT_RETRY,
                (MS, Duration::from_secs(60)),
            )?,
            fd_cache_shards: shards as usize,
            fd_cache_per_shard: per_shard as usize,
            allowed_uids: table.allowed_uids,
            jukebox_after: span(
                "jukebox_after",
                table.jukebox_after,
                DEFAULT_JUKEBOX_AFTER,
                (MS, Duration::from_secs(600)),
            )?,
        })
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
