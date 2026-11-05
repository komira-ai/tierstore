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

/// A bound on the largest read, or queue, of a key in bytes: a gibibyte.
const GIB: u64 = 1 << 30;

/// The duration of `local_cache.key`, from 1 ms to an hour; `default` if the file leaves it out.
fn time(key: &str, value: Option<Dur>, default: Duration) -> Result<Duration, ConfigError> {
    let got = value.map_or(default, |dur| dur.0);
    if got < MS || got > HOUR {
        let why = format!(
            "{}ms is not between {}ms and {}ms",
            got.as_millis(),
            MS.as_millis(),
            HOUR.as_millis()
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
    Ok(())
}

#[cfg(test)]
#[path = "config_cache_tuning_tests.rs"]
mod tests;
