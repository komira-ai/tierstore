//! The shape of the configuration file of a storage node that keeps a disk cache in front of its
//! own proxy: `[hot_store]`, `[cold_store]` and `[local_cache]` in one file, with `[server]`,
//! `[oci]` and `[[policy]]` as every file has them.
//!
//! ```toml
//! [hot_store]
//! kind   = "dir"
//! path   = "/srv/tierstore/hot"
//! budget = "500GiB"
//!
//! [cold_store]
//! kind   = "s3"
//! bucket = "tierstore-cold"
//!
//! [local_cache]
//! path   = "/var/lib/tierstore-cache"   # not inside either tier's directory
//! budget = "40GiB"
//!
//! [[policy]]
//! prefix = "cas/b/"
//! cache  = true
//! ```
//!
//! The file is read into a [`CombinedSettings`]: the `node` is the [`Cli`] the same file without
//! `[local_cache]` makes (so the tiers, the proxy, the reconciler and the front are the storage
//! node's, with its rules), and the `cache` is the cache half of a build node's file (see
//! [`config_cache`](super::config_cache)). The cache is in front of the proxy: the S3 front and
//! the OCI front read from the cache, and the cache reads from, and writes to, the proxy.
//!
//! In this shape a `[[policy]]` with `cache = true` keeps the keys under its prefix in the cache,
//! and at least one is needed. `write_through` is true, left out or given: the node owns the data
//! and takes writes, and they pass through the cache so that its entries stay right; `false` is
//! refused. The cache's versions are compared by size (see `CacheConfig::tiered_upstream`).
//!
//! What it refuses, besides what every file does: the refusals of a storage node's file, those of
//! a build node's `[local_cache]` table, `write_through = false`, and a cache directory that is
//! the directory of a `dir` tier, inside it or around it, or around the lock file or a key file of
//! the `[oci]` table (the tier would list the cache's files as objects), and a relative lock file
//! or `[oci]` key file (a relative path leads from the working directory of the process, which
//! the file does not name, so it cannot be compared with the cache directory). The paths are
//! compared by component, `.` and `..` resolved by text, not through links.

use std::path::{Component, Path, PathBuf};

use super::cli::Cli;
use super::config::{
    ConfigError, OciFlags, OciTable, PerRun, ServerTable, TierTable, invalid, storage,
};
use super::config_cache::{CachePart, LocalCacheTable, check_oci_is_kept};
use super::config_policy::{Policy, check_policies, require_keeping};

/// What a file with all three tables stands for.
#[derive(Debug)]
pub(super) struct CombinedSettings {
    /// The storage node: what the file makes without `[local_cache]`.
    pub node: Cli,
    /// The cache in front of its proxy.
    pub cache: CachePart,
}

/// The settings of the tables of a file with `[hot_store]`, `[cold_store]` and `[local_cache]`.
pub(super) fn combined(
    server: ServerTable,
    tiers: (TierTable, TierTable),
    table: LocalCacheTable,
    oci_table: Option<OciTable>,
    policy: &[Policy],
    per_run: PerRun,
) -> Result<CombinedSettings, ConfigError> {
    check_policies(policy, true)?;
    require_keeping(policy)?;
    if table.write_through == Some(false) {
        let why = "a node that owns the data takes writes, and they pass through the cache so that its entries stay right: leave it out or give true";
        return Err(invalid("local_cache.write_through", why));
    }
    let node = storage(server, tiers, oci_table, per_run)?;
    overlap(&table.path, &node)?;
    let mut cache = CachePart::new(table, policy)?;
    cache.config.write_through = true;
    cache.config.tiered_upstream = true;
    check_oci_is_kept(&oci_flags(&node), policy)?;
    Ok(CombinedSettings { node, cache })
}

/// The `[oci]` table of the node, as the flags that stand for it.
fn oci_flags(node: &Cli) -> OciFlags {
    OciFlags {
        listen: node.oci_listen.clone(),
        tls_cert: node.oci_tls_cert.clone(),
        tls_key: node.oci_tls_key.clone(),
        prefix: node.oci_prefix.clone(),
        max_connections: node.oci_max_connections,
        connection_lifetime: node.oci_connection_lifetime,
        require_auth: node.oci_require_auth,
        upstream: node.oci_upstream.clone(),
    }
}

/// The absolute `path` with its `.` and `..` components resolved by text (no symlink is
/// followed), or none for a relative path.
fn normalised(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                // The parent of the root is the root.
                if out.parent().is_some() {
                    let _ = out.pop();
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// Refuses a cache directory that is, is inside, or holds the directory of a `dir` tier, and one
/// that holds the lock file or a key file of the OCI front. Paths are compared by component
/// after `.` and `..` are resolved by text, so the cache path, the directory of a `dir` tier, the
/// lock file and the key files of the `[oci]` table must be absolute: a relative path cannot be
/// compared with another.
fn overlap(cache: &Path, node: &Cli) -> Result<(), ConfigError> {
    let Some(cache) = normalised(cache) else {
        let why = "must be an absolute path (it is compared with the directories of the tiers)";
        return Err(invalid("local_cache.path", why));
    };
    for (name, spec) in [("hot_store", &node.hot), ("cold_store", &node.cold)] {
        let Some(tier) = spec.strip_prefix("dir:").map(Path::new) else {
            continue;
        };
        let Some(tier) = normalised(tier) else {
            let why = format!("must be an absolute path to be compared with {name}'s directory");
            return Err(invalid("local_cache.path", why));
        };
        if cache.starts_with(&tier) || tier.starts_with(&cache) {
            let why = format!(
                "must not be the directory of {name}, inside it or around it: the tier would list the cache's files as objects"
            );
            return Err(invalid("local_cache.path", why));
        }
    }
    let files = [
        ("hot_store.lock_file", node.lock_file.as_deref()),
        ("oci.tls_cert_file", node.oci_tls_cert.as_deref()),
        ("oci.tls_key_file", node.oci_tls_key.as_deref()),
    ];
    for (name, file) in files {
        let Some(file) = file else { continue };
        // A relative path would be read from the working directory of the process, which the
        // file does not say: it could lead into the cache directory, which the start empties.
        let Some(file) = normalised(file) else {
            return Err(invalid(
                name,
                "must be an absolute path in a file with a local cache",
            ));
        };
        if file.starts_with(&cache) {
            let why = format!("must not hold {name}: it is not a place for the cache's files");
            return Err(invalid("local_cache.path", why));
        }
    }
    Ok(())
}
