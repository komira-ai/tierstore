//! The place of the fill socket of a build node (see [`fill_socket`](super::fill_socket)):
//! `local_cache.fill_socket`, or, where the key is left out, `fill.sock` in the service's state
//! directory, as the hand-off socket is `handoff.sock` there (see
//! [`config_handoff`](super::config_handoff)). With neither the node makes no fill socket.
//!
//! The path is held to what the hand-off socket's is: an absolute path of a file, short enough for
//! a Unix socket's address. It must not be the hand-off socket (the offer replaces the file at its
//! path), and must not be inside the cache directory.

use std::path::{Path, PathBuf};

use super::config::{ConfigError, invalid, refuse};
use super::config_cache::LocalCacheTable;
use super::config_combined::normalised;
use super::config_handoff::MOST_SOCKET_PATH;
use super::handoff::Handoff;
use super::lock::check_lock_path;

/// The file name of the fill socket in the state directory, when `local_cache.fill_socket` is
/// left out.
pub(super) const DEFAULT_FILL_SOCKET_NAME: &str = "fill.sock";

/// The fill socket `table` asks for: its `fill_socket`, else `fill.sock` in `state_dir`; none if
/// there is neither.
///
/// # Errors
/// A `fill_socket` that is not an absolute path of a file, is longer than a Unix socket's path
/// can be, or is the hand-off socket `handoff` names; each names `local_cache.fill_socket`.
pub(super) fn fill_socket_of(
    table: &LocalCacheTable,
    handoff: Option<&Handoff>,
    state_dir: Option<&Path>,
) -> Result<Option<PathBuf>, ConfigError> {
    let default = state_dir.map(|dir| dir.join(DEFAULT_FILL_SOCKET_NAME));
    let Some(socket) = table.fill_socket.as_ref().or(default.as_ref()) else {
        return Ok(None);
    };
    let key = "local_cache.fill_socket";
    check_lock_path(socket).map_err(|why| invalid(key, format!("{socket:?} {why}")))?;
    let length = socket.as_os_str().len();
    if length > MOST_SOCKET_PATH {
        let why = format!(
            "{socket:?} is {length} bytes; the path of a Unix socket is at most {MOST_SOCKET_PATH}"
        );
        return Err(invalid(key, why));
    }
    if handoff.is_some_and(|handoff| &handoff.socket == socket) {
        let why = format!("{socket:?} is server.handoff_socket too; the two sockets are two files");
        return Err(invalid(key, why));
    }
    Ok(Some(socket.clone()))
}

/// Refuses a cache directory that holds `socket`: the cache's files are not a place for it.
///
/// # Errors
/// A cache directory that holds the socket, or a relative one, which cannot be compared with it;
/// each names `local_cache.path`.
pub(super) fn check_fill_outside_cache(socket: &Path, cache: &Path) -> Result<(), ConfigError> {
    let key = "local_cache.path";
    let Some(cache) = normalised(cache) else {
        let why = "must be an absolute path to be compared with local_cache.fill_socket";
        return Err(invalid(key, why));
    };
    // The socket is absolute: `fill_socket_of` refuses another.
    if normalised(socket).is_some_and(|socket| socket.starts_with(&cache)) {
        let why = "must not hold local_cache.fill_socket: it is not a place for the cache's files";
        return Err(invalid(key, why));
    }
    Ok(())
}

/// Refuses the keys of the fill socket, and of the prefetch queue, in a file whose node has none: a
/// storage node with a cache in front of its proxy.
///
/// # Errors
/// A key of the fill socket that is given; it names the key.
pub(super) fn refuse_fill_keys(table: &LocalCacheTable) -> Result<(), ConfigError> {
    let why = "only a build node has a fill socket";
    let keys = [
        ("fill_socket", table.fill_socket.is_some()),
        (
            "fill_socket_concurrency",
            table.fill_socket_concurrency.is_some(),
        ),
        ("fill_socket_wait", table.fill_socket_wait.is_some()),
    ];
    refuse("local_cache", why, &keys)?;
    let why = "only a build node has a prefetch queue";
    let keys = [
        ("prefetch_concurrency", table.prefetch_concurrency.is_some()),
        ("prefetch_queue", table.prefetch_queue.is_some()),
        ("prefetch_queue_ttl", table.prefetch_queue_ttl.is_some()),
    ];
    refuse("local_cache", why, &keys)
}

#[cfg(test)]
#[path = "config_fill_tests.rs"]
mod tests;
