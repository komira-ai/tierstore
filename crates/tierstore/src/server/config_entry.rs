//! The keys of `[hot_store]` that set the waits, delays and limits of the entry protocol: the
//! entry reconciler's retry delays, stray sweep, copy-back limits and usage and listing work, and
//! the proxy's side of it (how long the usage object stands, how often the format object is read,
//! how many looks at an entry, how long a delete waits for the reconciler, and how often and how
//! fast the last step of a put or a delete is tried again), and the keys of the proxy's reads and
//! listings that read the markers of `docs/design/put-without-lock.md` (`read_window`,
//! `read_retries`, `list_marker_pages`, `want_retry`, `want_recent_limit` and
//! `copy_back_max_bytes`).
//!
//! Each has the value that was a constant of the code before it was a key as its default, and a
//! range, and the relations between keys are checked here once, when the file is read, each
//! refused with a message that names the key.

use std::time::Duration;

use crate::{
    DeleteWait, EntryReconcilerConfig, MarkReads, MarkedPutConfig, MarkersConfig, ProxyConfig,
    StepRetry,
};

use super::cli_heartbeat::DEPARTURE;
use super::config::{ConfigError, TierTable, duration, invalid, refuse, within};
use super::config_tuning::{HOUR, MS, span};
use super::config_units::{Dur, Size};

/// A bound on the largest size of a key: a tebibyte.
const TIB: u64 = 1 << 40;

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
    /// `hot_store.emergency_cold_landing`: whether a plain put that the hot tier refuses with
    /// `Full` before it took a byte lands in the cold tier under a lock, and not 503 (only under
    /// the format `active-marks`; residual R6' of `docs/design/put-without-lock.md`).
    pub emergency_cold_landing: bool,
    /// `hot_store.step_attempts`: the most tries of the last step of a put or a delete (the
    /// landing of a put, the hand-over of a delete, the removal of an entry), the first included.
    pub step_attempts: u32,
    /// `hot_store.step_retry_min`: the sleep before the second try of that step.
    pub step_retry_min: Duration,
    /// `hot_store.step_retry_max`: the longest sleep between two tries; each is twice the one
    /// before.
    pub step_retry_max: Duration,
    /// `hot_store.held_ttl`: how long a held put or delete of a live owner may stand unchanged
    /// before the reconciler reaps it.
    ///
    /// A held entry is only ever written by its owner (create, then one replace or delete to end
    /// it), so an entry that has not changed for this long was abandoned, or its operation is
    /// still running. It is reaped by a replace on the tag it was read at, so it can never undo
    /// an operation that finished: an owner that ends its operation first wins the replace, and an
    /// owner that comes back after the reap finds the entry changed and writes nothing. What a
    /// reap can cut short is an operation that is still running, so the value must be longer than
    /// any operation holds its entry: a delete holds it for at most four calls, each cut at
    /// `deadline_margin` (which is at most a quarter of `departure`), so `departure` bounds it
    /// when no step is tried again; with `step_attempts` tries of each of two of its calls (the
    /// removal of its entry and the hand-over; the two `delete_data` calls are not tried again) it
    /// holds it for up to `2 + 2 * step_attempts` calls and the sleeps between tries, which the
    /// default of an hour is far above (a value in the file is checked against `departure` and
    /// not against that sum);
    /// a put holds it for as long as its body takes, and a put that outlives `held_ttl` is not
    /// acknowledged (its landing finds the entry changed and answers 503). A value given in the
    /// file must also be longer than `lease_ttl`, so that an entry the previous holder of the
    /// lease left held while it copied is not reaped before that lease would have lapsed.
    pub held_ttl: Duration,
    /// `hot_store.read_window`: the longest a hot-miss pair, or the reads of a listing after its
    /// cold page, may take before they are read again.
    pub read_window: Duration,
    /// `hot_store.read_retries`: how many times such a read is read again before it is 503.
    pub read_retries: u32,
    /// `hot_store.list_marker_pages`: the pages of marker names one round of a listing reads
    /// beyond its first.
    pub list_marker_pages: u32,
    /// `hot_store.want_retry`: how long a proxy waits to ask again for the copy-back of a key.
    pub want_retry: Duration,
    /// `hot_store.want_recent_limit`: the most keys a proxy remembers it asked a copy-back of.
    pub want_recent_limit: u32,
    /// `hot_store.copy_back_max_bytes`: the largest object a read asks to have copied back.
    pub copy_back_max_bytes: u64,
    /// `hot_store.copy_concurrency`: the keys the copier works at once under the markers.
    pub copy_concurrency: u32,
    /// `hot_store.marks_page`: the markers in one listing page of the marks bucket.
    pub marks_page: u32,
    /// `hot_store.orphan_after`: how long a `DIRTY` marker with no hot object behind it stands
    /// unchanged before the leader clears it.
    pub orphan_after: Duration,
    /// `hot_store.tombstone_hold`: how long a `gone` marker is kept after its cold object was
    /// deleted or overwritten; at least `lease_ttl` plus `lease_margin`.
    pub tombstone_hold: Duration,
    /// `hot_store.lock_lease`: how long a lock stands unchanged before the leader reaps it
    /// though its holder lives; at least twice the default `lock_refresh` plus the default
    /// `queued_call_margin`.
    pub lock_lease: Duration,
    /// `hot_store.evict_min_age`: how old the cold copy of a hot object must be for the leader to
    /// evict the hot object, counted from the cold write on the leader's clock.
    pub evict_min_age: Duration,
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
            emergency_cold_landing: false,
            step_attempts: StepRetry::new().attempts,
            step_retry_min: StepRetry::new().delay,
            step_retry_max: StepRetry::new().delay_max,
            held_ttl: Duration::from_secs(3600),
            read_window: MarkReads::default().read_window,
            read_retries: MarkReads::default().read_retries,
            list_marker_pages: MarkReads::default().list_marker_pages,
            want_retry: MarkReads::default().want_retry,
            want_recent_limit: 100_000,
            copy_back_max_bytes: MarkReads::default().copy_back_max_bytes,
            copy_concurrency: 128,
            marks_page: 1000,
            orphan_after: Duration::from_secs(120),
            tombstone_hold: Duration::from_secs(120),
            lock_lease: MarkersConfig::default().lock_lease,
            evict_min_age: Duration::from_secs(600),
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
        config.held_ttl = self.held_ttl;
        config.copy_concurrency = self.copy_concurrency as usize;
        config.marks_page = self.marks_page as usize;
        config.orphan_after = self.orphan_after;
        config.tombstone_hold = self.tombstone_hold;
        config.markers.lock_lease = self.lock_lease;
        config.evict_min_age = self.evict_min_age;
        config.copy_back_max_bytes = self.copy_back_max_bytes;
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
        proxy.emergency_cold_landing = self.emergency_cold_landing;
        proxy.step_retry = StepRetry {
            attempts: self.step_attempts,
            delay: self.step_retry_min,
            delay_max: self.step_retry_max,
        };
        proxy.mark_reads = MarkReads {
            read_window: self.read_window,
            read_retries: self.read_retries,
            list_marker_pages: self.list_marker_pages,
            want_retry: self.want_retry,
            want_recent_limit: self.want_recent_limit as usize,
            copy_back_max_bytes: self.copy_back_max_bytes,
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

/// Refuses a `held_ttl` that the file sets at or below `longer` (`lease_ttl` or `departure`, and
/// its value): a live owner's operation could be reaped while it runs.
fn longer_than(held_ttl: Duration, key: &str, longer: Duration) -> Result<(), ConfigError> {
    if held_ttl <= longer {
        let why = format!(
            "{}ms must be longer than {key}, which is {}ms, or an entry of a live owner is reaped \
             while its operation runs",
            held_ttl.as_millis(),
            longer.as_millis()
        );
        return Err(invalid("hot_store.held_ttl", why));
    }
    Ok(())
}

/// Refuses the settings of the markers that do not hold together: a `tombstone_hold` below
/// `lease_ttl` plus `lease_margin` (a stalled leader's late cold write could land after the hold),
/// a `read_window` that is not below the `tombstone_hold` (a read pair could then pair a cold answer
/// from before a delete with a marker read after the tombstone was let go, I9), an `orphan_after` that is not more than the `reassert_after` of the puts (the leader would clear
/// the marker of a put that is still streaming its body, 5.1 step 5), and a `lock_lease` below two refreshes plus the margin of a queued call (checked by the
/// markers' own settings, with the defaults of the keys not yet in the file).
fn marks_checks(
    tuning: &EntryTuning,
    lease_ttl: Duration,
    lease_margin: Option<Duration>,
) -> Result<(), ConfigError> {
    let margin = lease_margin.unwrap_or(lease_ttl / 3);
    let least = lease_ttl.saturating_add(margin);
    if tuning.tombstone_hold < least {
        let why = format!(
            "{}ms must be at least lease_ttl plus lease_margin, which is {}ms, or a late cold \
             write of a stalled leader can land after the hold",
            tuning.tombstone_hold.as_millis(),
            least.as_millis()
        );
        return Err(invalid("hot_store.tombstone_hold", why));
    }
    if tuning.read_window >= tuning.tombstone_hold {
        let why = format!(
            "{}ms must be below tombstone_hold, which is {}ms, or a read pair could pair a cold \
             answer from before a delete with a marker read after the tombstone was let go",
            tuning.read_window.as_millis(),
            tuning.tombstone_hold.as_millis()
        );
        return Err(invalid("hot_store.read_window", why));
    }
    let reassert = MarkedPutConfig::new().reassert_after;
    if tuning.orphan_after <= reassert {
        let why = format!(
            "{}ms must be more than reassert_after, which is {}ms, or the leader can clear the \
             marker of a put that is still streaming its body",
            tuning.orphan_after.as_millis(),
            reassert.as_millis()
        );
        return Err(invalid("hot_store.orphan_after", why));
    }
    let markers = MarkersConfig {
        lock_lease: tuning.lock_lease,
        ..MarkersConfig::default()
    };
    markers
        .check()
        .map_err(|error| invalid("hot_store.lock_lease", error.to_string()))
}

/// Refuses the keys of the leader's pass over the markers that `cold` sets: only `[hot_store]`
/// has them.
pub(super) fn refuse_marks_keys(cold: &TierTable) -> Result<(), ConfigError> {
    let set = [
        ("copy_concurrency", cold.copy_concurrency.is_some()),
        ("marks_page", cold.marks_page.is_some()),
        ("orphan_after", cold.orphan_after.is_some()),
        ("tombstone_hold", cold.tombstone_hold.is_some()),
        ("lock_lease", cold.lock_lease.is_some()),
        ("evict_min_age", cold.evict_min_age.is_some()),
    ];
    refuse("cold_store", "only [hot_store] has it", &set)
}

/// The entry settings `[hot_store]` gives, or the defaults of those it leaves out, checked against
/// `lease_ttl`.
pub(super) fn entry_tuning(
    hot: &TierTable,
    lease_ttl: Duration,
    lease_margin: Option<Duration>,
) -> Result<EntryTuning, ConfigError> {
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
        emergency_cold_landing: hot
            .emergency_cold_landing
            .unwrap_or(old.emergency_cold_landing),
        step_attempts: count("step_attempts", hot.step_attempts, old.step_attempts, 100)?,
        step_retry_min: time(
            "step_retry_min",
            hot.step_retry_min,
            old.step_retry_min,
            HOUR,
        )?,
        step_retry_max: time(
            "step_retry_max",
            hot.step_retry_max,
            old.step_retry_max,
            HOUR,
        )?,
        held_ttl: time("held_ttl", hot.held_ttl, old.held_ttl, YEAR)?,
        read_window: time("read_window", hot.read_window, old.read_window, HOUR)?,
        read_retries: count("read_retries", hot.read_retries, old.read_retries, 100)?,
        list_marker_pages: count(
            "list_marker_pages",
            hot.list_marker_pages,
            old.list_marker_pages,
            100,
        )?,
        want_retry: time("want_retry", hot.want_retry, old.want_retry, YEAR)?,
        want_recent_limit: count(
            "want_recent_limit",
            hot.want_recent_limit,
            old.want_recent_limit,
            100_000_000,
        )?,
        copy_back_max_bytes: within(
            "hot_store",
            "copy_back_max_bytes",
            hot.copy_back_max_bytes
                .map_or(old.copy_back_max_bytes, |size: Size| size.0),
            1,
            TIB,
        )?,
        copy_concurrency: count(
            "copy_concurrency",
            hot.copy_concurrency,
            old.copy_concurrency,
            100_000,
        )?,
        marks_page: count("marks_page", hot.marks_page, old.marks_page, 100_000)?,
        orphan_after: time("orphan_after", hot.orphan_after, old.orphan_after, YEAR)?,
        tombstone_hold: time(
            "tombstone_hold",
            hot.tombstone_hold,
            old.tombstone_hold,
            YEAR,
        )?,
        lock_lease: time("lock_lease", hot.lock_lease, old.lock_lease, YEAR)?,
        evict_min_age: time("evict_min_age", hot.evict_min_age, old.evict_min_age, YEAR)?,
    };

    let low = ("retry_min", tuning.retry_min);
    ordered("retry_max", low, tuning.retry_max)?;
    let low = ("delete_poll_min", tuning.delete_poll_min);
    ordered("delete_poll_max", low, tuning.delete_poll_max)?;
    ordered("delete_wait", low, tuning.delete_wait)?;
    let low = ("step_retry_min", tuning.step_retry_min);
    ordered("step_retry_max", low, tuning.step_retry_max)?;
    if tuning.proxy_usage_stale_after <= tuning.usage_interval {
        let why = format!(
            "{}ms must be longer than usage_interval, which is {}ms, or the proxy ignores a usage \
             that is current",
            tuning.proxy_usage_stale_after.as_millis(),
            tuning.usage_interval.as_millis()
        );
        return Err(invalid("hot_store.proxy_usage_stale_after", why));
    }
    if hot.held_ttl.is_some() {
        let departure = duration(hot.departure, DEPARTURE);
        longer_than(tuning.held_ttl, "lease_ttl", lease_ttl)?;
        longer_than(tuning.held_ttl, "departure", departure)?;
    }
    marks_checks(&tuning, lease_ttl, lease_margin)?;
    Ok(tuning)
}

#[cfg(test)]
#[path = "config_entry_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "config_marks_tests.rs"]
mod marks_tests;
