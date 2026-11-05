//! The keys of `[hot_store]` that set the waits, delays and limits of the entry protocol: the
//! entry reconciler's retry delays, stray sweep, copy-back limits and usage and listing work, and
//! the proxy's side of it (how long the usage object stands, how often the format object is read,
//! how many looks at an entry, and how long a delete waits for the reconciler).
//!
//! Each has the value that was a constant of the code before it was a key as its default, and a
//! range, and the relations between keys are checked here once, when the file is read, each
//! refused with a message that names the key.

use std::time::Duration;

use crate::{DeleteWait, EntryReconcilerConfig, ProxyConfig};

use super::config::{ConfigError, TierTable, invalid, within};
use super::config_tuning::{HOUR, MS, span};
use super::config_units::Dur;

/// A bound on the longest interval of a key: a year.
const YEAR: Duration = Duration::from_secs(365 * 24 * 3600);

/// The settings of the entry reconciler and of the entry puts and deletes that `[hot_store]` gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryTuning {
    /// `hot_store.retry_min`: the delay of an entry whose item failed once.
    pub retry_min: Duration,
    /// `hot_store.retry_max`: the longest delay of a failing item.
    pub retry_max: Duration,
    /// `hot_store.stray_grace`: how old a stray hot object must be to be adopted.
    pub stray_grace: Duration,
    /// `hot_store.stray_interval`: how often the stray sweep runs after the one at takeover.
    pub stray_interval: Duration,
    /// `hot_store.stray_rate`: the most cold lookups a second of the stray sweep.
    pub stray_rate: u32,
    /// `hot_store.copy_back_ttl`: how long a requested copy-back may wait.
    pub copy_back_ttl: Duration,
    /// `hot_store.copy_back_max`: the most copy-backs copied.
    pub copy_back_max: u32,
    /// `hot_store.usage_interval`: how often the usage is published.
    pub usage_interval: Duration,
    /// `hot_store.usage_scan_interval`: how often the hot tier is listed to measure it.
    pub usage_scan_interval: Duration,
    /// `hot_store.scan_pages`: the most pages of that listing read in one step.
    pub scan_pages: u32,
    /// `hot_store.candidate_limit`: the most eviction candidates kept from a listing.
    pub candidate_limit: u32,
    /// `hot_store.tag_record_limit`: the most keys whose copy tags are remembered.
    pub tag_record_limit: u32,
    /// `hot_store.proxy_usage_stale_after`: how long the proxy lets the usage object stand
    /// unchanged before it ignores it.
    pub proxy_usage_stale_after: Duration,
    /// `hot_store.format_refresh`: how long the proxy uses the format object it read.
    pub format_refresh: Duration,
    /// `hot_store.entry_rounds`: the looks of a put or delete at an entry that vanished.
    pub entry_rounds: u32,
    /// `hot_store.delete_wait`: the most time a delete waits for the release of an entry.
    pub delete_wait: Duration,
    /// `hot_store.delete_poll_min`: the first sleep of that wait.
    pub delete_poll_min: Duration,
    /// `hot_store.delete_poll_max`: the longest sleep of that wait.
    pub delete_poll_max: Duration,
}

impl Default for EntryTuning {
    fn default() -> Self {
        Self {
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            stray_grace: Duration::from_secs(600),
            stray_interval: Duration::from_secs(24 * 3600),
            stray_rate: 100,
            copy_back_ttl: Duration::from_secs(600),
            copy_back_max: 1000,
            usage_interval: Duration::from_secs(10),
            usage_scan_interval: Duration::from_secs(24 * 3600),
            scan_pages: 100,
            candidate_limit: 10_000,
            tag_record_limit: 100_000,
            proxy_usage_stale_after: Duration::from_secs(30),
            format_refresh: Duration::from_secs(5),
            entry_rounds: 3,
            delete_wait: Duration::from_secs(30),
            delete_poll_min: Duration::from_secs(1),
            delete_poll_max: Duration::from_secs(10),
        }
    }
}

impl EntryTuning {
    /// Gives the values to the settings of the entry reconciler.
    pub(super) fn apply_reconciler(&self, config: &mut EntryReconcilerConfig) {
        config.retry_min = self.retry_min;
        config.retry_max = self.retry_max;
        config.stray_grace = self.stray_grace;
        config.stray_interval = self.stray_interval;
        config.stray_rate = self.stray_rate;
        config.copy_back_ttl = self.copy_back_ttl;
        config.copy_back_max = self.copy_back_max as usize;
        config.usage_interval = self.usage_interval;
        config.usage_scan_interval = self.usage_scan_interval;
        config.scan_pages = self.scan_pages as usize;
        config.candidate_limit = self.candidate_limit as usize;
        config.tag_record_limit = self.tag_record_limit as usize;
    }

    /// Gives the values to the settings of the proxy.
    pub(super) fn apply_proxy(&self, proxy: &mut ProxyConfig) {
        proxy.usage_stale_after = self.proxy_usage_stale_after;
        proxy.format_refresh = self.format_refresh;
        proxy.entry_rounds = self.entry_rounds;
        proxy.delete_wait = DeleteWait {
            wait: self.delete_wait,
            poll_min: self.delete_poll_min,
            poll_max: self.delete_poll_max,
        };
    }
}

/// The duration of `hot_store.key`, or `default`, from 1 ms to `high`.
fn time(
    key: &str,
    value: Option<Dur>,
    default: Duration,
    high: Duration,
) -> Result<Duration, ConfigError> {
    span(("hot_store", key), value, default, (MS, high))
}

/// The count of `hot_store.key`, or `default`, from 1 to `high`.
fn count(key: &str, value: Option<u32>, default: u32, high: u32) -> Result<u32, ConfigError> {
    within("hot_store", key, value.unwrap_or(default), 1, high)
}

/// Refuses `high` (the value of `hot_store.high_key`) below `low` (a key and its value).
fn ordered(high_key: &str, low: (&str, Duration), high: Duration) -> Result<(), ConfigError> {
    if high < low.1 {
        let why = format!(
            "{}ms is below {}, which is {}ms",
            high.as_millis(),
            low.0,
            low.1.as_millis()
        );
        return Err(invalid(&format!("hot_store.{high_key}"), why));
    }
    Ok(())
}

/// The entry settings `[hot_store]` gives, or the defaults of those it leaves out.
pub(super) fn entry_tuning(hot: &TierTable) -> Result<EntryTuning, ConfigError> {
    let old = EntryTuning::default();
    let tuning = EntryTuning {
        retry_min: time("retry_min", hot.retry_min, old.retry_min, HOUR)?,
        retry_max: time("retry_max", hot.retry_max, old.retry_max, HOUR)?,
        stray_grace: time("stray_grace", hot.stray_grace, old.stray_grace, YEAR)?,
        stray_interval: time(
            "stray_interval",
            hot.stray_interval,
            old.stray_interval,
            YEAR,
        )?,
        stray_rate: count("stray_rate", hot.stray_rate, old.stray_rate, 1_000_000)?,
        copy_back_ttl: time("copy_back_ttl", hot.copy_back_ttl, old.copy_back_ttl, YEAR)?,
        copy_back_max: count(
            "copy_back_max",
            hot.copy_back_max,
            old.copy_back_max,
            1_000_000,
        )?,
        usage_interval: time(
            "usage_interval",
            hot.usage_interval,
            old.usage_interval,
            HOUR,
        )?,
        usage_scan_interval: time(
            "usage_scan_interval",
            hot.usage_scan_interval,
            old.usage_scan_interval,
            YEAR,
        )?,
        scan_pages: count("scan_pages", hot.scan_pages, old.scan_pages, 100_000)?,
        candidate_limit: count(
            "candidate_limit",
            hot.candidate_limit,
            old.candidate_limit,
            10_000_000,
        )?,
        tag_record_limit: count(
            "tag_record_limit",
            hot.tag_record_limit,
            old.tag_record_limit,
            100_000_000,
        )?,
        proxy_usage_stale_after: time(
            "proxy_usage_stale_after",
            hot.proxy_usage_stale_after,
            old.proxy_usage_stale_after,
            HOUR,
        )?,
        format_refresh: time(
            "format_refresh",
            hot.format_refresh,
            old.format_refresh,
            HOUR,
        )?,
        entry_rounds: count("entry_rounds", hot.entry_rounds, old.entry_rounds, 100)?,
        delete_wait: time("delete_wait", hot.delete_wait, old.delete_wait, HOUR)?,
        delete_poll_min: time(
            "delete_poll_min",
            hot.delete_poll_min,
            old.delete_poll_min,
            HOUR,
        )?,
        delete_poll_max: time(
            "delete_poll_max",
            hot.delete_poll_max,
            old.delete_poll_max,
            HOUR,
        )?,
    };
    let low = ("retry_min", tuning.retry_min);
    ordered("retry_max", low, tuning.retry_max)?;
    let low = ("delete_poll_min", tuning.delete_poll_min);
    ordered("delete_poll_max", low, tuning.delete_poll_max)?;
    ordered("delete_wait", low, tuning.delete_wait)?;
    if tuning.proxy_usage_stale_after <= tuning.usage_interval {
        let why = format!(
            "{}ms must be longer than usage_interval, which is {}ms, or the proxy ignores a usage \
             that is current",
            tuning.proxy_usage_stale_after.as_millis(),
            tuning.usage_interval.as_millis()
        );
        return Err(invalid("hot_store.proxy_usage_stale_after", why));
    }
    Ok(tuning)
}

#[cfg(test)]
#[path = "config_entry_tests.rs"]
mod tests;
