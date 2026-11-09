//! The two keys of `[server]` that turn on the hand-off of the listening sockets to a successor
//! (see [`handoff`](super::handoff)): `handoff_socket`, the control socket, and `handoff_wait`,
//! how long a new instance waits for the running one to hand its sockets over.
//!
//! Without `handoff_socket` there is no hand-off, unless the service manager made a state
//! directory for the service (systemd's `STATE_DIRECTORY`, which the unit of hostkeeper's package
//! has): the socket is then [`DEFAULT_SOCKET_NAME`] in it, so that a node's file need not name it.
//! With neither the start binds its sockets itself and makes no control socket, as it did before
//! the keys existed. Every shape of node reads them: a storage
//! node serves through [`run_handing`](super::cli::run_handing), and a build node and a storage
//! node with `[local_cache]` get their sockets through the same hand-off (`bind_fronts`). A build
//! node has no `hot_store.lock_file`, so only the other checks apply to it. The keys are in
//! `[server]`, a table that the releases before them read and warn of the keys they do not know
//! (see `config_unknown`), so a storage node's file with these keys still starts on a release
//! that predates them, without a hand-off; v0.3.47 refuses them on a build node and on a storage
//! node with `[local_cache]`.

use std::path::Path;
use std::time::Duration;

use super::config::{ConfigError, ServerTable, invalid, refuse};
use super::config_combined::normalised;
use super::config_tuning::span;
use super::handoff::Handoff;
use super::lock::check_lock_path;

/// The file name of the control socket in the state directory, when `server.handoff_socket` is
/// left out. The update hook's `prepare` relies on it (`update_hook::DEFAULT_SOCKET_NAME`).
pub(super) const DEFAULT_SOCKET_NAME: &str = "handoff.sock";

/// The wait of `server.handoff_wait` when the key is left out.
pub(super) const DEFAULT_HANDOFF_WAIT: Duration = Duration::from_secs(10);

/// The shortest `server.handoff_wait`.
const LEAST_WAIT: Duration = Duration::from_millis(1);

/// The longest `server.handoff_wait`.
const MOST_WAIT: Duration = Duration::from_secs(3600);

/// The most bytes of the path of `server.handoff_socket`: a Unix socket's path, with the zero that
/// ends it, must fit the address's `sun_path`, which is 104 bytes on macOS and 108 on Linux. The
/// smaller of the two, so that a file means the same on both.
pub(super) const MOST_SOCKET_PATH: usize = 103;

/// The hand-off `[server]` asks for, if `handoff_socket` is given or the service has a
/// `state_dir` to hold it; `lock_file` is the node's `hot_store.lock_file`, which the control
/// socket must not be, since the offer replaces the file at its path.
///
/// # Errors
/// A `handoff_socket` that is not an absolute path of a file, is longer than a Unix socket's path
/// can be, or is the lock file; a `handoff_wait` outside 1ms to 1h, or given without
/// `handoff_socket`. Each names its key.
pub(super) fn handoff_of(
    server: &ServerTable,
    lock_file: Option<&Path>,
    state_dir: Option<&Path>,
) -> Result<Option<Handoff>, ConfigError> {
    let default = state_dir.map(|dir| dir.join(DEFAULT_SOCKET_NAME));
    let Some(socket) = server.handoff_socket.as_ref().or(default.as_ref()) else {
        let why =
            "it is the wait for the running instance of server.handoff_socket, which is not given";
        refuse(
            "server",
            why,
            &[("handoff_wait", server.handoff_wait.is_some())],
        )?;
        return Ok(None);
    };
    let key = "server.handoff_socket";
    check_lock_path(socket).map_err(|why| invalid(key, format!("{socket:?} {why}")))?;
    let length = socket.as_os_str().len();
    if length > MOST_SOCKET_PATH {
        let why = format!(
            "{socket:?} is {length} bytes; the path of a Unix socket is at most {MOST_SOCKET_PATH}"
        );
        return Err(invalid(key, why));
    }
    if lock_file == Some(socket.as_path()) {
        let why = format!(
            "{socket:?} is hot_store.lock_file too; the control socket replaces the file at its path"
        );
        return Err(invalid(key, why));
    }
    let wait = span(
        ("server", "handoff_wait"),
        server.handoff_wait,
        DEFAULT_HANDOFF_WAIT,
        (LEAST_WAIT, MOST_WAIT),
    )?;
    Ok(Some(Handoff {
        socket: socket.clone(),
        wait,
    }))
}

/// Refuses a build node's cache directory that holds the control socket of `handoff` (a storage
/// node with `[local_cache]` refuses it with its other files, in `config_combined`): the cache's
/// files are not a place for it, and the offer replaces the file at its path, which could be the
/// directory's `.lock`.
///
/// # Errors
/// A cache directory that holds the socket, or a relative one, which cannot be compared with it;
/// each names `local_cache.path`.
pub(super) fn check_outside_cache(
    handoff: Option<&Handoff>,
    cache: &Path,
) -> Result<(), ConfigError> {
    let Some(handoff) = handoff else {
        return Ok(());
    };
    let key = "local_cache.path";
    let Some(cache) = normalised(cache) else {
        let why = "must be an absolute path to be compared with server.handoff_socket";
        return Err(invalid(key, why));
    };
    // The socket is absolute: `handoff_of` refuses another.
    if normalised(&handoff.socket).is_some_and(|socket| socket.starts_with(&cache)) {
        let why = "must not hold server.handoff_socket: it is not a place for the cache's files";
        return Err(invalid(key, why));
    }
    Ok(())
}

#[cfg(test)]
#[path = "config_handoff_tests.rs"]
mod tests;
