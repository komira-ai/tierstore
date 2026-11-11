//! The keys of `[local_cache]` that set the times and limits of the cache's loops: how often the
//! eviction pass runs and the lock of the directory is tried, how often the scrub ticks, how many
//! pages one listing of the upstream may read, and the sizes of a fill's read, of the copy a put
//! keeps and of the memory of the deletes a read-only cache has seen.
//!
//! Each has the value that was a constant of the code before it was a key as its default, and a
//! range; the relation to `scrub_min_round` is checked by the caller, once, when the file is read.

use std::time::Duration;

use super::config::{ConfigError, within};
use super::config_cache::LocalCacheTable;
use super::config_tuning::{HOUR, MS};
use super::config_units::Dur;
use super::local_cache::CacheConfig;

/// The least `prefetch_queue_ttl`.
const SECOND: Duration = Duration::from_secs(1);

/// The least `hydrate_max_bytes`: a mebibyte.
const MIB: u64 = 1 << 20;

/// The most `hydrate_max_bytes`: a tebibyte.
const TIB: u64 = 1 << 40;

/// The least `hold_max`.
const MINUTE: Duration = Duration::from_secs(60);

/// The most `hold_max`.
const DAY: Duration = Duration::from_secs(24 * 3600);

/// A bound on the largest read, or queue, of a key in bytes: a gibibyte.
const GIB: u64 = 1 << 30;

/// The duration of `local_cache.key`, from 1 ms to an hour; `default` if the file leaves it out.
fn time(key: &str, value: Option<Dur>, default: Duration) -> Result<Duration, ConfigError> {
    time_between(key, value, default, (MS, HOUR))
}

/// The duration of `local_cache.key`, from `low` to `high`; `default` if the file leaves it out.
fn time_between(
    key: &str,
    value: Option<Dur>,
    default: Duration,
    (low, high): (Duration, Duration),
) -> Result<Duration, ConfigError> {
    let got = value.map_or(default, |dur| dur.0);
    if got < low || got > high {
        let why = format!(
            "{}ms is not between {}ms and {}ms",
            got.as_millis(),
            low.as_millis(),
            high.as_millis()
        );
        return Err(super::config::invalid(&format!("local_cache.{key}"), why));
    }
    Ok(got)
}

/// Gives `cache` the loop intervals and limits that `table` sets, and leaves the others at the
/// defaults of [`CacheConfig::new`].
pub(super) fn apply(table: &LocalCacheTable, cache: &mut CacheConfig) -> Result<(), ConfigError> {
    cache.pass_interval = time("pass_interval", table.pass_interval, cache.pass_interval)?;
    cache.lock_interval = time("lock_interval", table.lock_interval, cache.lock_interval)?;
    cache.scrub_tick = time("scrub_tick", table.scrub_tick, cache.scrub_tick)?;
    let calls = table.listing_calls.unwrap_or(cache.listing_calls);
    cache.listing_calls = within("local_cache", "listing_calls", calls, 1, 1_000_000)?;
    let forget = table
        .forget_limit
        .unwrap_or_else(|| u32::try_from(cache.forget_limit).unwrap_or(u32::MAX));
    let forget = within("local_cache", "forget_limit", forget, 1, 100_000_000)?;
    cache.forget_limit = forget as usize;
    let chunk = table.fill_chunk.map_or(cache.fill_chunk, |size| size.0);
    cache.fill_chunk = within("local_cache", "fill_chunk", chunk, 4096, GIB)?;
    let queue = table
        .keep_queue_bytes
        .map_or(cache.keep_queue_bytes, |size| size.0);
    cache.keep_queue_bytes = within("local_cache", "keep_queue_bytes", queue, 1, GIB)?;
    let fills = table
        .fill_socket_concurrency
        .unwrap_or_else(|| u32::try_from(cache.fill_socket_concurrency).unwrap_or(u32::MAX));
    let fills = within("local_cache", "fill_socket_concurrency", fills, 1, 1024)?;
    cache.fill_socket_concurrency = fills as usize;
    cache.fill_socket_wait = time(
        "fill_socket_wait",
        table.fill_socket_wait,
        cache.fill_socket_wait,
    )?;
    prefetch(table, cache)
}

/// The queue's keys: `prefetch_concurrency`, `prefetch_queue` and `prefetch_queue_ttl` (one second
/// to an hour), the bounds of a call that expands trees into it: `hydrate_max_trees`,
/// `hydrate_max_blobs` and `hydrate_max_bytes`, and the holds': `hold_max` (a minute to a day) and
/// `hold_max_count`.
fn prefetch(table: &LocalCacheTable, cache: &mut CacheConfig) -> Result<(), ConfigError> {
    let wanted = |value: Option<u32>, default: usize| {
        value.unwrap_or_else(|| u32::try_from(default).unwrap_or(u32::MAX))
    };
    let workers = wanted(table.prefetch_concurrency, cache.prefetch.concurrency);
    let workers = within("local_cache", "prefetch_concurrency", workers, 0, 64)?;
    cache.prefetch.concurrency = workers as usize;
    let queue = wanted(table.prefetch_queue, cache.prefetch.queue);
    let queue = within("local_cache", "prefetch_queue", queue, 1, 1_000_000)?;
    cache.prefetch.queue = queue as usize;
    let ttl = table.prefetch_queue_ttl;
    let ttl_range = (SECOND, HOUR);
    cache.prefetch.ttl = time_between("prefetch_queue_ttl", ttl, cache.prefetch.ttl, ttl_range)?;
    let hold_max = table.hold_max;
    let hold_range = (MINUTE, DAY);
    cache.prefetch.hold_max =
        time_between("hold_max", hold_max, cache.prefetch.hold_max, hold_range)?;
    let holds = wanted(table.hold_max_count, cache.prefetch.hold_max_count);
    let holds = within("local_cache", "hold_max_count", holds, 1, 100_000)?;
    cache.prefetch.hold_max_count = holds as usize;
    let trees = wanted(table.hydrate_max_trees, cache.prefetch.max_trees);
    let trees = within("local_cache", "hydrate_max_trees", trees, 1, 4096)?;
    cache.prefetch.max_trees = trees as usize;
    let blobs = wanted(table.hydrate_max_blobs, cache.prefetch.max_blobs);
    let blobs = within("local_cache", "hydrate_max_blobs", blobs, 1, 10_000_000)?;
    cache.prefetch.max_blobs = blobs as usize;
    let bytes = table
        .hydrate_max_bytes
        .map_or(cache.prefetch.max_bytes, |size| size.0);
    cache.prefetch.max_bytes = within("local_cache", "hydrate_max_bytes", bytes, MIB, TIB)?;
    Ok(())
}

#[cfg(test)]
#[path = "config_cache_tuning_tests.rs"]
mod tests;
