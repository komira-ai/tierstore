//! A `mount-provider` interface: the mount provider of a build node (see
//! [`provider`](super)), jobscheduler's `MountProvider` protocol on a Unix socket.
//!
//! ```toml
//! [[interface]]
//! type   = "mount-provider"
//! params = { socket = "/var/lib/tierstore/provider.sock", mountpoint = "/run/tierstore/fs" }
//! ```
//!
//! `params.socket` is needed: an absolute path of a file, at most as long as a Unix socket's path
//! can be, and not the hand-off socket. `params.mountpoint` is the node's
//! file system mount, under which the provider answers a source's directory: an absolute path. In a
//! file with an `fs` interface it is that interface's `mountpoint`, which it takes when left out and
//! must equal when given (`config_fs.rs`); in a file without one it is the mount of `tierstore fs`
//! on the node (its `fs.mountpoint`), `/run/tierstore/fs` if left out. `params.fetch_timeout` bounds one
//! read of the store (a tree list, or the bytes of one file): longer than zero, `60s` if left out.
//! The interface listens on no address and takes no other param; a file has at most one, and only
//! a build node's file has one, since the provider makes its trees in the node's tree store.

use std::path::Path;

use super::super::config::fs::refuse_fs_params;
use super::super::config::{ConfigError, invalid, refuse, require};
use super::super::config_combined::normalised;
use super::super::config_handoff::MOST_SOCKET_PATH;
use super::super::config_interface::Params;
use super::super::lock::check_lock_path;
use super::{DEFAULT_FETCH_TIMEOUT, DEFAULT_MOUNTPOINT, ProviderFlags};

/// The settings of the provider that a `mount-provider` interface at `at` says.
///
/// # Errors
/// A param the type does not take, no `socket`, a `socket` that is not an absolute path of a file
/// or is too long, a `mountpoint` that is not absolute, or a `fetch_timeout` of zero; each names
/// its key.
pub(in crate::server) fn provider_of(
    at: &str,
    params: &Params,
) -> Result<ProviderFlags, ConfigError> {
    let place = format!("{at}.params");
    let others = [
        ("port", params.port.is_some()),
        ("addresses", params.addresses.is_some()),
        ("tls_cert_file", params.tls_cert_file.is_some()),
        ("tls_key_file", params.tls_key_file.is_some()),
        ("prefix", params.prefix.is_some()),
        ("max_connections", params.max_connections.is_some()),
        ("connection_lifetime", params.connection_lifetime.is_some()),
        ("require_auth", params.require_auth.is_some()),
        ("stage_read", params.stage_read.is_some()),
        ("max_manifest_bytes", params.max_manifest_bytes.is_some()),
        ("read_chunk_bytes", params.read_chunk_bytes.is_some()),
        ("echo_bytes", params.echo_bytes.is_some()),
        ("upstream", params.upstream.is_some()),
    ];
    let why = "not for type \"mount-provider\": it listens on its socket alone";
    refuse(&place, why, &others)?;
    refuse_fs_params(&place, "mount-provider", params)?;
    let socket = require(&place, "socket", params.socket.clone())?;
    let key = format!("{place}.socket");
    check_lock_path(&socket).map_err(|why| invalid(&key, format!("{socket:?} {why}")))?;
    let length = socket.as_os_str().len();
    if length > MOST_SOCKET_PATH {
        let why = format!(
            "{socket:?} is {length} bytes; the path of a Unix socket is at most {MOST_SOCKET_PATH}"
        );
        return Err(invalid(&key, why));
    }
    let mountpoint = params
        .mountpoint
        .clone()
        .unwrap_or_else(|| DEFAULT_MOUNTPOINT.into());
    if !mountpoint.is_absolute() {
        let why = format!("{mountpoint:?} must be an absolute path");
        return Err(invalid(&format!("{place}.mountpoint"), why));
    }
    let fetch_timeout = params
        .fetch_timeout
        .as_ref()
        .map_or(DEFAULT_FETCH_TIMEOUT, |dur| dur.0);
    if fetch_timeout.is_zero() {
        let why = "it must be longer than zero";
        return Err(invalid(&format!("{place}.fetch_timeout"), why));
    }
    Ok(ProviderFlags {
        socket,
        mountpoint,
        fetch_timeout,
    })
}

/// Refuses a provider's socket that is another socket of the node (such as the hand-off socket,
/// each of which replaces the file at its path when it is bound) or is inside the cache
/// directory `cache`, which is not a place for it.
///
/// # Errors
/// A [`ConfigError`] naming the provider's socket.
pub(in crate::server) fn check_provider_socket(
    provider: Option<&ProviderFlags>,
    others: &[(&str, Option<&Path>)],
    cache: &Path,
) -> Result<(), ConfigError> {
    let Some(provider) = provider else {
        return Ok(());
    };
    let key = "mount-provider interface params.socket";
    // The socket is absolute (`provider_of` refuses another); a relative cache path, which a build
    // node's file may have, is not compared.
    let inside = normalised(&provider.socket)
        .zip(normalised(cache))
        .is_some_and(|(socket, cache)| socket.starts_with(cache));
    if inside {
        let why = format!(
            "{:?} is inside local_cache.path: it is not a place for the cache's files",
            provider.socket
        );
        return Err(invalid(key, why));
    }
    for (name, other) in others {
        if *other == Some(provider.socket.as_path()) {
            let why = format!(
                "{:?} is {name} too; the two sockets are two files",
                provider.socket
            );
            return Err(invalid(key, why));
        }
    }
    Ok(())
}
