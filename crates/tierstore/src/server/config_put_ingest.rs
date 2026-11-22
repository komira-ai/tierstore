//! `local_cache.put_ingest`: the root of the put ingest of a build node (see
//! [`put_ingest`](super::put_ingest)), the directory under which the jobs of a Mac commit their
//! objects into plain directories. Left out, the node ingests nothing.
//!
//! The root is an absolute path; it must not be the cache directory, inside it or around it, and
//! the node must take writes (`local_cache.write_through = true`), since each object it ingests is
//! a create-only put to the upstream through the cache. A storage node with a cache refuses the key:
//! its jobs, if any, commit through the put directory of `tierstore fs`.

use std::path::{Path, PathBuf};

use super::config::{ConfigError, invalid, refuse};
use super::config_cache::LocalCacheTable;
use super::config_combined::normalised;

/// The name of the key.
const KEY: &str = "local_cache.put_ingest";

/// The root of the put ingest `table` asks for, checked; none when it asks for none.
///
/// # Errors
/// A root that is not an absolute path, that is the cache directory, inside it or around it, or
/// that is given while `write_through` is not `true`; each names the key.
pub(super) fn put_ingest_of(table: &LocalCacheTable) -> Result<Option<PathBuf>, ConfigError> {
    let Some(root) = &table.put_ingest else {
        return Ok(None);
    };
    let Some(root) = normalised(root) else {
        return Err(invalid(KEY, format!("{root:?} is not an absolute path")));
    };
    if table.write_through != Some(true) {
        let why = "each object the ingest stores is a put to the upstream: it needs local_cache.write_through = true, and a key of the upstream that may write";
        return Err(invalid(KEY, why));
    }
    check_apart(&root, &table.path)?;
    Ok(Some(root))
}

/// Refuses a root that is the cache directory `cache`, inside it or around it: the jobs' files and
/// the cache's are kept apart.
fn check_apart(root: &Path, cache: &Path) -> Result<(), ConfigError> {
    let Some(cache) = normalised(cache) else {
        let why = "must be an absolute path to be compared with local_cache.put_ingest";
        return Err(invalid("local_cache.path", why));
    };
    if root.starts_with(&cache) || cache.starts_with(root) {
        let why = format!(
            "{root:?} is the cache directory, inside it or around it: the jobs' files are not the cache's"
        );
        return Err(invalid(KEY, why));
    }
    Ok(())
}

/// Refuses `put_ingest` in the file of a storage node with a cache.
///
/// # Errors
/// The key, when it is given.
pub(super) fn refuse_put_ingest(table: &LocalCacheTable) -> Result<(), ConfigError> {
    let why = "only a build node ingests put directories";
    refuse(
        "local_cache",
        why,
        &[("put_ingest", table.put_ingest.is_some())],
    )
}

#[cfg(test)]
#[path = "config_put_ingest_tests.rs"]
mod tests;
