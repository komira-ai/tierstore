//! The settings of the reconciler of a node: the lease, the pool of copies and the water marks. The
//! reconciler itself is the [`EntryReconciler`](crate::EntryReconciler).

use std::sync::Arc;
use std::time::Duration;

use crate::{Clock, MonotonicClock, Sleeper, TokioSleeper};

/// The settings that an [`EntryReconciler`](crate::EntryReconciler) is built over: the lease it
/// holds, the pool of its copies and the water marks of the hot tier. Its own settings are
/// [`EntryReconcilerConfig`](crate::EntryReconcilerConfig), which has these as `base`.
#[derive(Clone)]
pub struct ReconcilerConfig {
    /// Names this reconciler in the lease. No two reconcilers that run at the same time have
    /// the same name.
    pub holder: String,
    /// The bytes the hot tier is meant to hold at most.
    pub hot_budget: u64,
    /// The share of the budget (0.8, say) above which the eviction starts.
    pub high_water: f64,
    /// The share of the budget (0.7, say) down to which the eviction goes on.
    pub low_water: f64,
    /// The most objects copied to the cold tier at once. Size it to what the cold tier
    /// takes in parallel.
    pub parallel: usize,
    /// How long a lease object must stand unchanged, on this process's own monotonic clock
    /// (`mono`), before another reconciler may take it over.
    pub lease_ttl: Duration,
    /// How often the holder renews the lease, on a timer of its own that runs while a step
    /// works. `None` is a third of `lease_ttl`.
    pub lease_renew: Option<Duration>,
    /// How much of `lease_ttl` the holder keeps in hand: it counts the lease as held until
    /// `lease_ttl` less this after it sent its last renewal, and a step that starts with less
    /// than this left renews first. `None` is a third of `lease_ttl`.
    pub lease_margin: Option<Duration>,
    /// The most objects asked for in one listing of the entries or of the hot tier.
    pub page_size: usize,
    /// The time, for the usage object and the age of the objects of the hot tier. It must agree
    /// with the times the hot tier gives its objects. The lease does not use it.
    pub clock: Arc<dyn Clock>,
    /// The monotonic clock of this process. It times the lease (when a lease object was first
    /// seen unchanged, and until when the holder is valid) and the time since a registration was
    /// first seen as it is. It is read only on this process, and nothing that another machine
    /// wrote is compared with it, so it must not step back or follow the clock of the machine
    /// ([`MonotonicClock`]).
    pub mono: Arc<dyn Clock>,
    /// What the renewal timer waits on between two renewals.
    pub sleeper: Arc<dyn Sleeper>,
}

impl ReconcilerConfig {
    /// The settings for a reconciler `holder` and a hot tier of `hot_budget` bytes, with
    /// high and low water of 0.8 and 0.7, 16 parallel copies, a lease ttl of 30 seconds, listings
    /// of 100 objects, a renewal and a margin of a third of the lease ttl, a [`MonotonicClock`]
    /// for the lease and the timer of the runtime.
    #[must_use]
    pub fn new(holder: &str, hot_budget: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            holder: holder.to_owned(),
            hot_budget,
            high_water: 0.8,
            low_water: 0.7,
            parallel: 16,
            lease_ttl: Duration::from_secs(30),
            lease_renew: None,
            lease_margin: None,
            page_size: 100,
            clock,
            mono: Arc::new(MonotonicClock::new()),
            sleeper: Arc::new(TokioSleeper),
        }
    }
}
