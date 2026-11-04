//! The two keys of `[server]` that turn on the hand-off of the listening sockets to a successor
//! (see [`handoff`](super::handoff)): `handoff_socket`, the control socket, and `handoff_wait`,
//! how long a new instance waits for the running one to hand its sockets over.
//!
//! Without `handoff_socket` there is no hand-off: the start binds its sockets itself and makes no
//! control socket, as it did before the keys existed. Only a storage node without `[local_cache]`
//! serves through [`run_handing`](super::cli::run_handing), so a build node and a storage node
//! with a cache refuse both keys, naming them, rather than read past them. They are in
//! `[server]`, a table that the release before this one reads and warns of the keys it does not
//! know (see `config_unknown`), so a file with these keys still starts there, without a hand-off.

use std::path::Path;
use std::time::Duration;

use super::config::{ConfigError, ServerTable, invalid, refuse};
use super::config_tuning::span;
use super::handoff::Handoff;
use super::lock::check_lock_path;

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

/// The hand-off `[server]` asks for, if `handoff_socket` is given; `lock_file` is the node's
/// `hot_store.lock_file`, which the control socket must not be, since the offer replaces the file
/// at its path.
///
/// # Errors
/// A `handoff_socket` that is not an absolute path of a file, is longer than a Unix socket's path
/// can be, or is the lock file; a `handoff_wait` outside 1ms to 1h, or given without
/// `handoff_socket`. Each names its key.
pub(super) fn handoff_of(
    server: &ServerTable,
    lock_file: Option<&Path>,
) -> Result<Option<Handoff>, ConfigError> {
    let Some(socket) = &server.handoff_socket else {
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

/// Refuses the two keys in the `[server]` of a node that `shape` names, which does not hand off
/// its sockets.
///
/// # Errors
/// The first of the two keys that is given, by name.
pub(super) fn refuse_handoff(server: &ServerTable, shape: &str) -> Result<(), ConfigError> {
    let why = format!("{shape} does not hand off its sockets; leave it out");
    refuse(
        "server",
        &why,
        &[
            ("handoff_socket", server.handoff_socket.is_some()),
            ("handoff_wait", server.handoff_wait.is_some()),
        ],
    )
}

#[cfg(test)]
#[path = "config_handoff_tests.rs"]
mod tests;
