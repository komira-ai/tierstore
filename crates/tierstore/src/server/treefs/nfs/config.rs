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
//! | `fs.nfs.drain_wait` | how long an instance that handed over waits for the clients to close their connections before it ends | `10s` |
//! | `fs.nfs.jukebox_after` | how long a read waits for a fill before the client is told to ask again | `5s` |
//! | `fs.nfs.dead_timeout` | how long the mount may be unresponsive before the kernel marks it dead and every call on it fails (`deadtimeout` of `mount_nfs`); more than `fs.fetch_timeout`, so that a fill the server still waits for never ends the mount | `fs.fetch_timeout` and `30s` |
//! | `fs.nfs.unmount_wait` | the most time each command of the mount and the unmount (`mount_nfs`, `umount`, `diskutil`, `mount`, `id`) and the process's own `unmount(2)` may take; at most `25s`, so that the four steps of an unmount fit in the `ExitTimeOut` of the package's job | `20s` |
//!
//! The hand-off keys an earlier release read (`handoff_socket`, `handoff_wait`,
//! `handoff_ready_wait`) are not keys of this table: the NFS front hands its socket over with the
//! process (`SIGUSR2`), as `run.rs` says.

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

/// The wait for the clients when the file leaves out `fs.nfs.drain_wait`.
const DEFAULT_DRAIN_WAIT: Duration = Duration::from_secs(10);

/// How much longer than `fs.fetch_timeout` the mount may be unresponsive, when the file leaves out
/// `fs.nfs.dead_timeout`: a fill that takes `fs.fetch_timeout` is answered with JUKEBOX until then,
/// which the client counts as unresponsive, and the margin covers the answer after it.
const DEAD_TIMEOUT_MARGIN: Duration = Duration::from_secs(30);

/// The most `fs.nfs.dead_timeout` may be.
const MOST_DEAD_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// The limit of each command when the file leaves out `fs.nfs.unmount_wait`.
const DEFAULT_UNMOUNT_WAIT: Duration = Duration::from_secs(20);

/// The most `fs.nfs.unmount_wait` may be: four steps of an unmount, each this long and the wait
/// for a killed command, stay below the `ExitTimeOut` of the package's job (150 s).
pub(super) const MOST_UNMOUNT_WAIT: Duration = Duration::from_secs(25);

/// The longest a name of a user may be.
const MOST_USER_NAME: usize = 64;

/// `[fs.nfs]`, and `params.nfs` of an `fs` interface.
#[derive(Clone, Debug, Deserialize)]
pub(in crate::server) struct NfsTable {
    listen: Option<String>,
    mount: Option<bool>,
    mount_user: Option<String>,
    mount_wait: Option<Dur>,
    mount_retry: Option<Dur>,
    fd_cache_shards: Option<u32>,
    fd_cache_per_shard: Option<u32>,
    jukebox_after: Option<Dur>,
    drain_wait: Option<Dur>,
    dead_timeout: Option<Dur>,
    unmount_wait: Option<Dur>,
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
    pub jukebox_after: Duration,
    /// How long an instance that handed over waits for the clients to close their connections.
    pub drain_wait: Duration,
    /// How long the mount may be unresponsive before the kernel marks it dead: the mount option
    /// `deadtimeout`, more than `fs.fetch_timeout`.
    pub dead_timeout: Duration,
    /// The most time each command of the mount and the unmount may take.
    pub unmount_wait: Duration,
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
            jukebox_after: DEFAULT_JUKEBOX_AFTER,
            drain_wait: DEFAULT_DRAIN_WAIT,
            dead_timeout: dead_timeout_after(super::super::config::DEFAULT_FETCH_TIMEOUT),
            unmount_wait: DEFAULT_UNMOUNT_WAIT,
        }
    }
}

/// The dead timeout of a file that leaves it out: `fetch_timeout` in whole seconds, rounded up, and
/// the margin.
fn dead_timeout_after(fetch_timeout: Duration) -> Duration {
    let seconds = fetch_timeout.as_secs() + u64::from(fetch_timeout.subsec_nanos() > 0);
    Duration::from_secs(seconds) + DEAD_TIMEOUT_MARGIN
}

/// `fs.nfs.dead_timeout`: the value given, or the default after `fetch_timeout`; one that is not
/// more than `fetch_timeout` is refused, since the client would give up on the mount while the
/// server still waits for a fill it may answer.
/// `place` is where the table's keys are (`fs.nfs`), and `fetch_key` the key of the fetch timeout.
fn dead_timeout_of(
    (place, fetch_key): (&str, &str),
    given: Option<Dur>,
    fetch_timeout: Duration,
) -> Result<Duration, ConfigError> {
    let key = format!("{place}.dead_timeout");
    let value = given.map_or_else(|| dead_timeout_after(fetch_timeout), |dur| dur.0);
    if value <= fetch_timeout {
        let why = format!(
            "{}ms is not more than {fetch_key} ({}ms): a fill that the server still waits \
             for would end the mount",
            value.as_millis(),
            fetch_timeout.as_millis()
        );
        return Err(invalid(&key, why));
    }
    if value > MOST_DEAD_TIMEOUT {
        let why = format!(
            "{}ms is more than {}ms",
            value.as_millis(),
            MOST_DEAD_TIMEOUT.as_millis()
        );
        return Err(invalid(&key, why));
    }
    Ok(value)
}

/// The address `text`, the value of `key`, names, which must be on loopback.
fn listen_of(key: &str, text: &str) -> Result<SocketAddr, ConfigError> {
    let address: SocketAddr = text
        .parse()
        .map_err(|_| invalid(key, "must be an address and a port, as 127.0.0.1:12049"))?;
    match address.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => Ok(address),
        _ => Err(invalid(
            key,
            "must be an IPv4 loopback address: the protocol has no authentication a network could rely on",
        )),
    }
}

/// `user`, the value of `key`, if it is a name that cannot be taken for an option or a path.
fn user_of(key: &str, user: String) -> Result<String, ConfigError> {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    if user.is_empty()
        || user.len() > MOST_USER_NAME
        || user.starts_with('-')
        || !user.chars().all(plain)
    {
        return Err(invalid(
            key,
            "must be a user name of letters, digits, `_`, `.` and `-`, not starting with `-`",
        ));
    }
    Ok(user)
}

/// `value`, the value of `key` of the table at `place`, if it is between `low` and `high`.
fn span(
    (place, key): (&str, &str),
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
        return Err(invalid(&format!("{place}.{key}"), why));
    }
    Ok(value)
}

impl NfsSettings {
    /// The settings of `table`, or the defaults where the file has none; `fs` is where the keys
    /// of the file system are (`fs`, or the params of an `fs` interface), the table's being its
    /// `nfs`, and `fetch_timeout` is its `fetch_timeout`, which the dead timeout must be more
    /// than.
    ///
    /// # Errors
    /// A [`ConfigError`] that names the key: an address that is not on loopback, a user name that
    /// is not one, a value out of its range.
    pub(in crate::server) fn of(
        fs: &str,
        table: Option<NfsTable>,
        fetch_timeout: Duration,
    ) -> Result<Self, ConfigError> {
        let Some(table) = table else {
            return Ok(Self {
                dead_timeout: dead_timeout_after(fetch_timeout),
                ..Self::default()
            });
        };
        let place = format!("{fs}.nfs");
        let place = place.as_str();
        let key = |name: &str| format!("{place}.{name}");
        let shards = within(
            place,
            "fd_cache_shards",
            table.fd_cache_shards.unwrap_or(DEFAULT_SHARDS),
            1,
            4096,
        )?;
        let per_shard = within(
            place,
            "fd_cache_per_shard",
            table.fd_cache_per_shard.unwrap_or(DEFAULT_PER_SHARD),
            1,
            65_536,
        )?;
        Ok(Self {
            listen: table
                .listen
                .as_deref()
                .map_or(Ok(DEFAULT_LISTEN), |text| listen_of(&key("listen"), text))?,
            mount: table.mount.unwrap_or(true),
            mount_user: table
                .mount_user
                .map(|user| user_of(&key("mount_user"), user))
                .transpose()?,
            mount_wait: span(
                (place, "mount_wait"),
                table.mount_wait,
                DEFAULT_MOUNT_WAIT,
                (Duration::from_secs(1), HOUR),
            )?,
            mount_retry: span(
                (place, "mount_retry"),
                table.mount_retry,
                DEFAULT_MOUNT_RETRY,
                (MS, Duration::from_secs(60)),
            )?,
            fd_cache_shards: shards as usize,
            fd_cache_per_shard: per_shard as usize,
            jukebox_after: span(
                (place, "jukebox_after"),
                table.jukebox_after,
                DEFAULT_JUKEBOX_AFTER,
                (MS, Duration::from_secs(600)),
            )?,
            drain_wait: span(
                (place, "drain_wait"),
                table.drain_wait,
                DEFAULT_DRAIN_WAIT,
                (MS, HOUR),
            )?,
            dead_timeout: dead_timeout_of(
                (place, &format!("{fs}.fetch_timeout")),
                table.dead_timeout,
                fetch_timeout,
            )?,
            unmount_wait: span(
                (place, "unmount_wait"),
                table.unmount_wait,
                DEFAULT_UNMOUNT_WAIT,
                (Duration::from_secs(1), MOST_UNMOUNT_WAIT),
            )?,
        })
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
