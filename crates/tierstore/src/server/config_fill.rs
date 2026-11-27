//! The keys of `[local_cache]` that the fill socket of a build node had: `local_cache.fill_socket`,
//! read and ignored with a line on stderr (no fill socket is served: the file system of an `fs`
//! interface calls the cache in this process, `docs/design/one-service.md` section 5), so that a
//! file written for a release that served one still starts; and `fill_socket_concurrency` and
//! `fill_socket_wait`, which keep their names and bound the fills the file system asks for
//! (`config_cache_tuning.rs`).

use super::config::{ConfigError, refuse};
use super::config_cache::LocalCacheTable;

/// The line a file with `local_cache.fill_socket` is told on stderr.
pub(super) const IGNORED_FILL_SOCKET: &str = "tierstore: local_cache.fill_socket is ignored: no \
     fill socket is served, the file system of an fs interface calls the cache in this process";

/// Says on the operator's stderr that `local_cache.fill_socket` of `table` is ignored, if it has
/// one.
pub(super) fn warn_ignored_fill_socket(table: &LocalCacheTable) {
    if table.fill_socket.is_some() {
        tracing::warn!("{IGNORED_FILL_SOCKET}");
    }
}

/// Refuses the keys of the prefetch queue in a file whose node has none: a storage node with a
/// cache in front of its proxy. The keys of the fill socket's pool are not refused there.
///
/// # Errors
/// A key of the prefetch queue that is given; it names the key.
pub(super) fn refuse_prefetch_keys(table: &LocalCacheTable) -> Result<(), ConfigError> {
    let why = "only a build node has a prefetch queue";
    let keys = [
        ("prefetch_concurrency", table.prefetch_concurrency.is_some()),
        ("prefetch_queue", table.prefetch_queue.is_some()),
        ("prefetch_queue_ttl", table.prefetch_queue_ttl.is_some()),
        ("hydrate_max_trees", table.hydrate_max_trees.is_some()),
        ("hydrate_max_blobs", table.hydrate_max_blobs.is_some()),
        ("hydrate_max_bytes", table.hydrate_max_bytes.is_some()),
        ("hold_max", table.hold_max.is_some()),
        ("hold_max_count", table.hold_max_count.is_some()),
    ];
    refuse("local_cache", why, &keys)
}

#[cfg(test)]
#[path = "config_fill_tests.rs"]
mod tests;
