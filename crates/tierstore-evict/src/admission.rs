//! [`Admission`]: `reserve` at job assignment (section 7).

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tierstore_fetch_api::entry::EntryStore;
use tierstore_metrics::Cache;
use tierstore_metrics::labels::AdmissionReason;

use crate::config::{CELL_BYTES, EvictConfig};
use crate::space::CacheDevice;

/// Why [`Admission::reserve`] did not admit a job.
#[derive(Debug, thiserror::Error)]
pub enum ReserveError {
    /// The estimate exceeds the whole budget: the job's needed cells never fit this cache.
    #[error("{estimate} bytes needed exceed the cache budget of {budget} bytes")]
    Oversize {
        /// The bytes asked for.
        estimate: u64,
        /// The cache's budget.
        budget: u64,
    },
    /// The bytes already reserved plus the estimate exceed `0.9 x budget`.
    #[error(
        "{held} bytes held and {estimate} bytes needed exceed {limit} bytes, 0.9 of the budget"
    )]
    Budget {
        /// The bytes asked for.
        estimate: u64,
        /// The bytes already reserved.
        held: u64,
        /// The bytes jobs may hold ([`EvictConfig::admit_limit`]).
        limit: u64,
    },
    /// The cache device cannot hold the reserved bytes and the estimate, even with every cached
    /// byte not reserved evicted.
    #[error(
        "{held} bytes held and {estimate} bytes needed exceed the {available} bytes the cache device can give"
    )]
    Enospc {
        /// The bytes asked for.
        estimate: u64,
        /// The bytes already reserved.
        held: u64,
        /// The device's free bytes plus the cache's use.
        available: u64,
    },
    /// The free space of the cache device could not be read.
    #[error("reading the free space of the cache device: {source}")]
    Device {
        /// The error.
        #[source]
        source: io::Error,
    },
}

impl ReserveError {
    /// The refusal reason the metrics count, or `None` for an error that is not a refusal.
    #[must_use]
    pub fn reason(&self) -> Option<AdmissionReason> {
        match self {
            Self::Oversize { .. } => Some(AdmissionReason::Oversize),
            Self::Budget { .. } => Some(AdmissionReason::Budget),
            Self::Enospc { .. } => Some(AdmissionReason::Enospc),
            Self::Device { .. } => None,
        }
    }
}

struct Inner {
    config: EvictConfig,
    store: Arc<dyn EntryStore>,
    device: Arc<dyn CacheDevice>,
    metrics: Arc<Cache>,
    /// The bytes of every live [`Reservation`].
    held: Mutex<u64>,
}

impl Inner {
    fn held(&self) -> MutexGuard<'_, u64> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The admission side of the node cache (section 7): what admitted jobs hold, and `reserve`.
/// Clones share one account.
#[derive(Clone)]
pub struct Admission {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("config", &self.inner.config)
            .field("held", &self.held_bytes())
            .finish_non_exhaustive()
    }
}

/// The bytes one admitted job holds, charged until the value is dropped (when the job ends).
#[derive(Debug)]
#[must_use = "a reservation is released when dropped"]
pub struct Reservation {
    admission: Admission,
    bytes: u64,
}

impl Reservation {
    /// The bytes reserved.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let inner = &self.admission.inner;
        let mut held = inner.held();
        *held = held.saturating_sub(self.bytes);
        inner.metrics.held_bytes.set((), *held as f64);
    }
}

impl Admission {
    /// The admission of the cache `store` on `device`, with `config`'s budget.
    #[must_use]
    pub fn new(
        store: Arc<dyn EntryStore>,
        device: Arc<dyn CacheDevice>,
        config: EvictConfig,
        metrics: Arc<Cache>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                store,
                device,
                metrics,
                held: Mutex::new(0),
            }),
        }
    }

    /// The bytes every live reservation holds.
    #[must_use]
    pub fn held_bytes(&self) -> u64 {
        *self.inner.held()
    }

    /// Admits a job whose needed cells, minus those already `present`, are `estimate` bytes
    /// (the jobscheduler's estimate of what the job will hold). Refused, in this order, when the
    /// estimate exceeds the budget ([`ReserveError::Oversize`]), when the bytes already reserved
    /// plus the estimate exceed `0.9 x budget` ([`ReserveError::Budget`]), or when the cache
    /// device's free bytes plus the cache's use are less than that sum
    /// ([`ReserveError::Enospc`]). A refusal is counted by reason. Admission does not evict: the
    /// evictor makes the room as the job's cells are fetched.
    pub fn reserve(&self, estimate: u64) -> Result<Reservation, ReserveError> {
        let r = self.check(estimate);
        if let Err(e) = &r
            && let Some(reason) = e.reason()
        {
            self.inner.metrics.admission_refusals_total.inc(reason);
        }
        r
    }

    fn check(&self, estimate: u64) -> Result<Reservation, ReserveError> {
        let inner = &self.inner;
        let budget = inner.config.budget_bytes;
        if estimate > budget {
            return Err(ReserveError::Oversize { estimate, budget });
        }
        // The device and the store are read before the account is locked: neither is
        // serialised with other admissions, only the account is.
        let free = inner
            .device
            .free_bytes()
            .map_err(|source| ReserveError::Device { source })?;
        let used: u64 = inner
            .store
            .entries()
            .iter()
            .map(|i| i.counts.used() * CELL_BYTES)
            .sum();
        let available = free.saturating_add(used);

        let mut held = inner.held();
        let wanted = held.saturating_add(estimate);
        let limit = inner.config.admit_limit();
        if wanted > limit {
            return Err(ReserveError::Budget {
                estimate,
                held: *held,
                limit,
            });
        }
        if wanted > available {
            return Err(ReserveError::Enospc {
                estimate,
                held: *held,
                available,
            });
        }
        *held = wanted;
        inner.metrics.held_bytes.set((), wanted as f64);
        drop(held);
        Ok(Reservation {
            admission: self.clone(),
            bytes: estimate,
        })
    }
}
