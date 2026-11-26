//! An `fs` interface: the settings of the trees of a build node as a file system (one FUSE mount on
//! Linux, NFS on loopback on macOS) in the node's own file, as `docs/design/one-service.md` lays
//! out. They are read and checked here; the start of the build node serves the mount from them (on
//! Linux by FUSE, `treefs/interface.rs`; on macOS by NFS, `treefs/nfs/interface.rs`), and on a
//! system that is neither this release serves nothing from them and says so (`not_served`). `tierstore fs`, with its own `[fs]` table, still serves the trees of a node
//! whose file has no `fs` interface.
//!
//! ```toml
//! [[interface]]
//! type = "fs"
//! [interface.params]
//! mountpoint   = "/run/tierstore/fs"
//! client_group = "tierstore"
//! owner        = "1000:1000"
//! [interface.params.put]
//! enabled   = true
//! prefixes  = ["cas/b/"]
//! stage_dir = "/var/lib/tierstore-cache/cas/put-stage"
//! ```
//!
//! The params are the keys of the `[fs]` table of `tierstore fs` (`treefs/config.rs`), with the same
//! names, defaults and ranges, and its sub-tables `[fs.nfs]` and `[fs.put]` are the params `nfs` and
//! `put`. Three keys of `[fs]` are not params, and each is refused by name: `fill_socket` (the file
//! system is served by the process that holds the cache), `cache_path` (it is `local_cache.path` of
//! the same file) and `handoff_socket` (the process's hand-over is on `server.handoff_socket`); nor
//! are the hand-off keys of `[fs.nfs]` (`handoff_socket`, `handoff_wait`, `handoff_ready_wait`).
//! Unlike `[fs]`, which warns of a key it does not know, the params refuse one, as every
//! interface's do, in `nfs` and `put` too: a new table has no older file to keep loading.
//!
//! What the one file adds to the rules of `[fs]`, each a refusal by name at load:
//!
//! - the interface is a build node's alone (a file with `[local_cache]` and `[cold_store]` and no
//!   `[hot_store]`), as the `trees` and `mount-provider` interfaces are (`no_trees`), and a file has
//!   at most one;
//! - a `mount-provider` interface's `mountpoint`, left out, is the `fs` interface's, and one that is
//!   given must be it ([`provider_mountpoint`]);
//! - `local_cache.path` is absolute, `params.put.stage_dir` is on its file system and
//!   `params.runtime_dir` is not inside it, and `params.control_socket` is none of the node's other
//!   sockets ([`fs_settings`]).
//!
//! `params.owner` also takes the table `{ subids_of = "<job user>", id = 1000 }`, the job user's
//! first subordinate uid and gid plus `id`, read from `/etc/subuid` and `/etc/subgid` at load
//! ([`owner`]), so that one file serves every node of a group.
//!
//! Names of users and groups (`client_group`, `owner`) are looked up through the system at load, as
//! `tierstore fs --check-config` does, the file system of a path is the device of the nearest
//! directory of it that is there ([`volume_of`]), and the subordinate id files are read: all
//! through [`Host`], which a test replaces.

use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use crate::server::config::{ConfigError, invalid, refuse};
use crate::server::config_combined::normalised;
use crate::server::config_interface::Params;
use crate::server::provider::ProviderFlags;
use owner::{Owner, Subids};

use crate::server::treefs::{
    DEFAULT_MOUNTPOINT, Database, Found, FsSettings, FsTable, Lookup, system_names,
};

/// What the load of a file asks of the system: the id of a user or group name, and the file system
/// (its device number) a path is on.
#[derive(Clone, Copy, Debug)]
pub(in crate::server) struct Host {
    /// The id of a name in the users or the groups of the system.
    pub names: fn(Database, &str) -> Found,
    /// The device number of the file system a path is on.
    pub volume: fn(&Path) -> io::Result<u64>,
    /// The text of a file: the subordinate id files an `owner = { subids_of, id }` is read from.
    pub read: fn(&Path) -> io::Result<String>,
}

impl Host {
    /// The system this process runs on.
    pub(in crate::server) const SYSTEM: Host = Host {
        names: system_names,
        volume: volume_of,
        read: owner::read_text,
    };
}

impl Default for Host {
    fn default() -> Self {
        Self::SYSTEM
    }
}

/// The device number of the file system `path` is on: that of `path`, or of the nearest directory
/// above it that is there, since a directory a start makes (a stage directory, a cache directory
/// on a fresh node) need not be there when the file is checked.
///
/// # Errors
/// What the system says of the first path that is there but cannot be looked at, or
/// [`io::ErrorKind::NotFound`] for a relative path none of whose parts is there.
pub(in crate::server) fn volume_of(path: &Path) -> io::Result<u64> {
    for above in path.ancestors() {
        match std::fs::metadata(above) {
            Ok(found) => return Ok(found.dev()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no part of the path is there",
    ))
}

/// An `fs` interface of a file, as its params say it, before the file's `[local_cache]` is read.
#[derive(Debug)]
pub(in crate::server) struct FsInterface {
    /// The entry's place in the list (`interface[2]`).
    at: String,
    /// The params, as the `[fs]` table of `tierstore fs` would hold them, but for an owner given as
    /// the job user's subordinate ids, which is `subids` until it is resolved.
    table: FsTable,
    /// `params.owner` given as `{ subids_of, id }`.
    subids: Option<Subids>,
}

impl FsInterface {
    /// Where the mount is made: `params.mountpoint`, or its default.
    pub(in crate::server) fn mountpoint(&self) -> PathBuf {
        self.table
            .mountpoint
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_MOUNTPOINT))
    }
}

/// The params of the other interface types, which an `fs` interface does not take: it listens on
/// no address and has no socket of its own beside its control socket.
fn others(params: &Params) -> [(&'static str, bool); 14] {
    [
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
        ("socket", params.socket.is_some()),
    ]
}

/// The `fs` interface at `at` as its params say it.
///
/// # Errors
/// A param of another type, a key of `[fs]` that is not a param (`fill_socket`, `cache_path`,
/// `handoff_socket`) and a hand-off key of `params.nfs`, each naming the param and why.
pub(in crate::server) fn fs_of(at: &str, params: &Params) -> Result<FsInterface, ConfigError> {
    let place = format!("{at}.params");
    let why = "not for type \"fs\": the file system is served on its mount point alone";
    refuse(&place, why, &others(params))?;
    let gone = [
        (
            "fill_socket",
            params.fill_socket.is_some(),
            "not a param of an fs interface: the file system is served by the process that holds the cache, which it asks for fills itself",
        ),
        (
            "cache_path",
            params.cache_path.is_some(),
            "not a param of an fs interface: the cache directory is local_cache.path of this file",
        ),
        (
            "handoff_socket",
            params.handoff_socket.is_some(),
            "not a param of an fs interface: the process hands over on server.handoff_socket",
        ),
    ];
    for (key, given, why) in gone {
        refuse(&place, why, &[(key, given)])?;
    }
    let nfs = params.nfs.clone().map(|strict| strict.0);
    if let Some(table) = &nfs {
        let why = "not a param of an fs interface: the NFS front of the process has no hand-off of its own";
        refuse(&format!("{place}.nfs"), why, &table.handoff_keys())?;
    }
    let (owner, subids) = match params.owner.clone() {
        Some(Owner::Pair(text)) => (Some(text), None),
        Some(Owner::Subids(subids)) => (None, Some(subids)),
        None => (None, None),
    };
    let table = FsTable {
        mountpoint: params.mountpoint.clone(),
        runtime_dir: params.runtime_dir.clone(),
        control_socket: params.control_socket.clone(),
        client_group: params.client_group.clone(),
        owner,
        fetch_timeout: params.fetch_timeout,
        threads: params.threads,
        nfs,
        handoff_wait: params.handoff_wait,
        recover_wait: params.recover_wait,
        reattach_concurrency: params.reattach_concurrency,
        tree_idle_secs: params.tree_idle_secs,
        tree_list_ram_bytes: params.tree_list_ram_bytes,
        put: params.put.clone().map(|strict| strict.0),
        ..FsTable::default()
    };
    Ok(FsInterface {
        at: at.to_owned(),
        table,
        subids,
    })
}

/// Refuses the params only an `fs` interface takes in an interface of type `kind` at `place`.
///
/// # Errors
/// The first of them that is given, by name.
pub(in crate::server) fn refuse_fs_params(
    place: &str,
    kind: &str,
    params: &Params,
) -> Result<(), ConfigError> {
    let theirs = [
        ("owner", params.owner.is_some()),
        ("client_group", params.client_group.is_some()),
        ("runtime_dir", params.runtime_dir.is_some()),
        ("control_socket", params.control_socket.is_some()),
        ("threads", params.threads.is_some()),
        ("handoff_wait", params.handoff_wait.is_some()),
        ("recover_wait", params.recover_wait.is_some()),
        (
            "reattach_concurrency",
            params.reattach_concurrency.is_some(),
        ),
        ("tree_idle_secs", params.tree_idle_secs.is_some()),
        ("tree_list_ram_bytes", params.tree_list_ram_bytes.is_some()),
        ("nfs", params.nfs.is_some()),
        ("put", params.put.is_some()),
        ("fill_socket", params.fill_socket.is_some()),
        ("cache_path", params.cache_path.is_some()),
        ("handoff_socket", params.handoff_socket.is_some()),
    ];
    refuse(place, &format!("not for type \"{kind}\""), &theirs)
}

/// Gives the mount provider the `fs` interface's mount point when its own `mountpoint`, at
/// `given` (the provider's place, `interface[1]`), was left out; one that was given must be it.
/// Without an `fs` interface the provider keeps its own, or its default, as before.
///
/// # Errors
/// A provider's `mountpoint` that is not the `fs` interface's, naming the provider's param.
pub(in crate::server) fn provider_mountpoint(
    provider: Option<&mut ProviderFlags>,
    given: Option<&str>,
    fs: Option<&FsInterface>,
) -> Result<(), ConfigError> {
    let (Some(provider), Some(fs)) = (provider, fs) else {
        return Ok(());
    };
    let mountpoint = fs.mountpoint();
    let Some(at) = given else {
        provider.mountpoint = mountpoint;
        return Ok(());
    };
    if provider.mountpoint != mountpoint {
        let why = format!(
            "{:?} is not the mountpoint of the fs interface, {}.params.mountpoint ({mountpoint:?}): the provider answers a source's directory under that mount; leave it out to take it",
            provider.mountpoint, fs.at
        );
        return Err(invalid(&format!("{at}.params.mountpoint"), why));
    }
    Ok(())
}

/// The other sockets of a build node, by key, which the file system's control socket must not be:
/// each replaces the file at its path when it is bound.
pub(in crate::server) type Sockets<'a> = [(&'a str, Option<&'a Path>); 3];

/// The settings of the `fs` interface `fs`, if the file has one, on a build node whose cache
/// directory is `cache` and whose fill socket is `fill_socket`; `sockets` are the node's other
/// sockets, and `host` looks names and file systems up.
///
/// # Errors
/// Those of [`FsSettings::of_interface`]; those of an owner given as the job user's subordinate
/// ids ([`owner::resolved`]); a relative `local_cache.path`; a runtime directory inside
/// it; a stage directory of the put directory on another file system, or one of the two that
/// cannot be looked at; a control socket that is another socket of the node.
pub(in crate::server) fn fs_settings(
    fs: Option<FsInterface>,
    (cache, fill_socket): (&Path, Option<&Path>),
    sockets: &Sockets<'_>,
    host: Host,
) -> Result<Option<FsSettings>, ConfigError> {
    let Some(FsInterface {
        at,
        mut table,
        subids,
    }) = fs
    else {
        return Ok(None);
    };
    let place = format!("{at}.params");
    if let Some(subids) = &subids {
        table.owner = Some(owner::resolved(&format!("{place}.owner"), subids, host)?);
    }
    let Some(normal_cache) = normalised(cache) else {
        let why = "must be an absolute path in a file with an fs interface: the file system reads the cache's files by it";
        return Err(invalid("local_cache.path", why));
    };
    let node = (cache.to_owned(), fill_socket.map(Path::to_owned));
    let lookup: Lookup<'_> = &host.names;
    let settings = FsSettings::of_interface(&place, table, node, lookup)?;
    if normalised(&settings.runtime_dir).is_some_and(|dir| dir.starts_with(&normal_cache)) {
        let why = format!(
            "{:?} is inside local_cache.path: it is not a place for the cache's files",
            settings.runtime_dir
        );
        return Err(invalid(&format!("{place}.runtime_dir"), why));
    }
    for (name, other) in sockets {
        if *other == Some(settings.control_socket.as_path()) {
            let why = format!(
                "{:?} is {name} too; the two sockets are two files",
                settings.control_socket
            );
            return Err(invalid(&format!("{place}.control_socket"), why));
        }
    }
    if let Some(put) = &settings.put {
        let key = format!("{place}.put.stage_dir");
        let looked = |path: &Path, key: &str| {
            (host.volume)(path)
                .map_err(|error| invalid(key, format!("{path:?} could not be looked at: {error}")))
        };
        let stage = looked(&put.stage_dir, &key)?;
        if stage != looked(cache, "local_cache.path")? {
            let why = format!(
                "{:?} is not on the file system of local_cache.path: a staged file becomes a file of the cache by a rename",
                put.stage_dir
            );
            return Err(invalid(&key, why));
        }
    }
    Ok(Some(settings))
}

/// The line a start on a system that is neither Linux nor macOS says when its file has an `fs`
/// interface, which this release reads and checks there and does not serve; none without one.
#[cfg(any(not(any(target_os = "linux", target_os = "macos")), test))]
pub(in crate::server) fn not_served(fs: Option<&FsSettings>) -> Option<String> {
    fs.map(|fs| {
        format!(
            "tierstore: the fs interface (mountpoint {}) is checked and not served by this release on this system; `tierstore fs`, with its own [fs] table, serves the trees",
            fs.mountpoint.display()
        )
    })
}

#[path = "config_fs_owner.rs"]
pub(in crate::server) mod owner;

#[cfg(test)]
#[path = "config_fs_tests.rs"]
mod tests;
