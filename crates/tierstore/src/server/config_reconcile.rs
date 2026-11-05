//! The keys of `[hot_store]` that set the pace and the waits of the reconciler's loop: the idle
//! wait after a round that did nothing, the timing of the lease (when it is renewed and how much
//! of its time the holder keeps in hand), the size of a listing page, how long a stop waits for
//! the lease, how often a process that does not hold the lock file tries it, and how often the
//! start looks for the proxy's first beat.
//!
//! Each has the value that was a constant of the code before it was a key as its default, and a
//! range; the relations between keys are checked here once, when the file is read, each refused
//! with a message that names the key.

use std::time::Duration;

use crate::ReconcilerConfig;

use super::config::{ConfigError, TierTable, invalid, within};
use super::config_tuning::{HOUR, MS, span};
use super::config_units::Dur;

/// The settings of the reconciler's loop and of the start that `[hot_store]` gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconcileTuning {
    /// `hot_store.idle_min`: the wait after the first round that did nothing.
    pub idle_min: Duration,
    /// `hot_store.idle_max`: the longest wait; the wait doubles from `idle_min` to it.
    pub idle_max: Duration,
    /// `hot_store.lease_renew`: how often the holder renews the lease; a third of `lease_ttl`
    /// (`None`) if the file leaves it out.
    pub lease_renew: Option<Duration>,
    /// `hot_store.lease_margin`: how much of `lease_ttl` the holder keeps in hand; a third of
    /// `lease_ttl` (`None`) if the file leaves it out.
    pub lease_margin: Option<Duration>,
    /// `hot_store.listing_page_size`: the objects asked for in one listing page.
    pub listing_page_size: u32,
    /// `hot_store.stop_wait`: the longest a halt waits for the lease to be given up.
    pub stop_wait: Duration,
    /// `hot_store.lock_retry`: the wait between two tries of a lock file another process holds.
    pub lock_retry: Duration,
    /// `hot_store.first_beat_poll`: the wait between two looks at the proxy at the start.
    pub first_beat_poll: Duration,
}

impl Default for ReconcileTuning {
    fn default() -> Self {
        Self {
            idle_min: Duration::from_secs(1),
            idle_max: Duration::from_secs(10),
            lease_renew: None,
            lease_margin: None,
            listing_page_size: 100,
            stop_wait: Duration::from_secs(5),
            lock_retry: Duration::from_secs(1),
            first_beat_poll: Duration::from_millis(1),
        }
    }
}

impl ReconcileTuning {
    /// Gives the values to the settings of the reconciler.
    pub(super) fn apply(&self, config: &mut ReconcilerConfig) {
        config.idle_min = self.idle_min;
        config.idle_max = self.idle_max;
        config.lease_renew = self.lease_renew;
        config.lease_margin = self.lease_margin;
        config.page_size = self.listing_page_size as usize;
        config.stop_wait = self.stop_wait;
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

/// The given duration of `hot_store.key`, from `low` to an hour; none if it is left out.
fn given(key: &str, value: Option<Dur>, low: Duration) -> Result<Option<Duration>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    span(("hot_store", key), Some(value), low, (low, HOUR)).map(Some)
}

/// Refuses a lease whose holder could not keep it: the renewal and the margin it keeps in hand
/// (a third of `ttl` for each that the file leaves out, as the reconciler reads it) must each be
/// shorter than the ttl, and together not longer than it, or the holder counts the lease as lost
/// before it renews it. A lease the file does not set is not looked at.
fn check_lease(
    ttl: Duration,
    renew: Option<Duration>,
    margin: Option<Duration>,
) -> Result<(), ConfigError> {
    if renew.is_none() && margin.is_none() {
        return Ok(());
    }
    let third = ttl / 3;
    let renew_in = renew.unwrap_or(third);
    let margin_in = margin.unwrap_or(third);
    let ttl_ms = ttl.as_millis();
    for (key, set, value) in [
        ("lease_renew", renew, renew_in),
        ("lease_margin", margin, margin_in),
    ] {
        if set.is_some() && value >= ttl {
            let why = format!(
                "{}ms must be below lease_ttl, which is {ttl_ms}ms",
                value.as_millis()
            );
            return Err(invalid(&format!("hot_store.{key}"), why));
        }
    }
    if renew_in + margin_in > ttl {
        let key = if renew.is_some() {
            "lease_renew"
        } else {
            "lease_margin"
        };
        let why = format!(
            "lease_renew ({}ms) and lease_margin ({}ms) together must not be longer than \
             lease_ttl, which is {ttl_ms}ms, or the holder counts its lease as lost before it \
             renews it",
            renew_in.as_millis(),
            margin_in.as_millis()
        );
        return Err(invalid(&format!("hot_store.{key}"), why));
    }
    Ok(())
}

/// The reconciler settings `[hot_store]` gives, or the defaults of those it leaves out, checked
/// against `lease_ttl`.
pub(super) fn reconcile_tuning(
    hot: &TierTable,
    lease_ttl: Duration,
) -> Result<ReconcileTuning, ConfigError> {
    let old = ReconcileTuning::default();
    let tuning = ReconcileTuning {
        idle_min: time("idle_min", hot.idle_min, old.idle_min, HOUR)?,
        idle_max: time("idle_max", hot.idle_max, old.idle_max, HOUR)?,
        lease_renew: given("lease_renew", hot.lease_renew, MS)?,
        lease_margin: given("lease_margin", hot.lease_margin, Duration::ZERO)?,
        listing_page_size: within(
            "hot_store",
            "listing_page_size",
            hot.listing_page_size.unwrap_or(old.listing_page_size),
            1,
            100_000,
        )?,
        stop_wait: time("stop_wait", hot.stop_wait, old.stop_wait, HOUR)?,
        lock_retry: time("lock_retry", hot.lock_retry, old.lock_retry, HOUR)?,
        first_beat_poll: time(
            "first_beat_poll",
            hot.first_beat_poll,
            old.first_beat_poll,
            HOUR,
        )?,
    };
    if tuning.idle_max < tuning.idle_min {
        let why = format!(
            "{}ms is below idle_min, which is {}ms",
            tuning.idle_max.as_millis(),
            tuning.idle_min.as_millis()
        );
        return Err(invalid("hot_store.idle_max", why));
    }
    check_lease(lease_ttl, tuning.lease_renew, tuning.lease_margin)?;
    Ok(tuning)
}

#[cfg(test)]
#[path = "config_reconcile_tests.rs"]
mod tests;
