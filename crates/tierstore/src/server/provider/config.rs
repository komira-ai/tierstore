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
//! can be, and neither the hand-off socket nor the fill socket. `params.mountpoint` is the mount of
//! `tierstore fs` on the node (its `fs.mountpoint`), under which the provider answers a source's
//! directory: an absolute path, `/run/tierstore/fs` if left out. `params.fetch_timeout` bounds one
//! read of the store (a tree list, or the bytes of one file): longer than zero, `60s` if left out.
//!
//! `params.local_trees`, an absolute path of a directory outside `local_cache.path`, makes the
//! provider write each tree whole, as plain files, into that directory and answer `Mount` with the
//! tree's directory there (module [`local`](super::local)) instead of a directory of `tierstore
//! fs`: a Mac's, whose jobs cannot read a network volume. It is not given with `mountpoint`, which
//! it replaces:
//!
//! ```toml
//! [[interface]]
//! type   = "mount-provider"
//! params = { socket = "/usr/local/tierstore/run/provider.sock", local_trees = "/usr/local/tierstore/job-trees" }
//! ```
//!
//! The interface listens on no address and takes no other param; a file has at most one, and only
//! a build node's file has one, since the provider reads its trees through the node's cache and
//! the node's `[trees]` bound its reads.

use std::path::{Path, PathBuf};

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
/// or is too long, a `mountpoint` that is not absolute, a `fetch_timeout` of zero, a `local_trees`
/// that is not an absolute path of a directory, or `local_trees` with `mountpoint`; each names its
/// key.
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
    let local_trees = local_trees_of(&place, params)?;
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
        local_trees,
    })
}

/// The directory `params.local_trees` names, if it names one: an absolute path of a directory, not
/// given with `params.mountpoint`.
///
/// # Errors
/// A [`ConfigError`] on `<place>.local_trees`.
fn local_trees_of(place: &str, params: &Params) -> Result<Option<PathBuf>, ConfigError> {
    let Some(local) = params.local_trees.clone() else {
        return Ok(None);
    };
    let key = format!("{place}.local_trees");
    if !local.is_absolute() {
        return Err(invalid(&key, format!("{local:?} must be an absolute path")));
    }
    if normalised(&local).is_some_and(|path| path.parent().is_none()) {
        let why = format!("{local:?} is the root: the trees need a directory of their own");
        return Err(invalid(&key, why));
    }
    if params.mountpoint.is_some() {
        let why = "not with mountpoint: the provider answers either directories it writes or \
                   directories of tierstore fs";
        return Err(invalid(&key, why));
    }
    Ok(Some(local))
}

/// Refuses a provider's socket that is another socket of the node (the hand-off socket or the fill
/// socket, each of which replaces the file at its path when it is bound) or is inside the cache
/// directory `cache`, which is not a place for it; and a provider's `local_trees` that is the
/// cache directory or inside it, where the cache would take the trees' files for its own.
///
/// # Errors
/// A [`ConfigError`] naming the provider's socket or its `local_trees`.
pub(in crate::server) fn check_provider_paths(
    provider: Option<&ProviderFlags>,
    others: &[(&str, Option<&Path>)],
    cache: &Path,
) -> Result<(), ConfigError> {
    let Some(provider) = provider else {
        return Ok(());
    };
    // The socket and the trees are absolute (`provider_of` refuses another); a relative cache
    // path, which a build node's file may have, is not compared.
    let inside = |path: &Path| {
        normalised(path)
            .zip(normalised(cache))
            .is_some_and(|(path, cache)| path.starts_with(cache))
    };
    if let Some(local) = provider
        .local_trees
        .as_deref()
        .filter(|local| inside(local))
    {
        let why = format!(
            "{local:?} is local_cache.path or inside it: it is not a place for the cache's files"
        );
        return Err(invalid("mount-provider interface params.local_trees", why));
    }
    let key = "mount-provider interface params.socket";
    if inside(&provider.socket) {
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
