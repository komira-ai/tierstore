//! The eviction of the hot tier's objects, which the leader does while the hot tier holds more
//! than the high water of its budget, and goes on doing until it holds no more than the low water.
//!
//! The candidates are the oldest objects of the last listing of the hot tier (see the usage
//! module), taken oldest first, a batch at a time on the pool. For one candidate:
//!
//! 1. An entry for the key, of any operation and state, means the key is being worked on: it is
//!    never evicted while it has one.
//! 2. No hot object means nothing to evict. No cold object means the hot object is the only copy:
//!    it is adopted by a pending put entry instead, which the next step copies, and it is not
//!    evicted.
//! 3. Both tiers have it. The bytes are compared (ranges of
//!    [`EntryReconcilerConfig::compare_range`](super::EntryReconcilerConfig), outside any entry),
//!    unless this leader copied or compared the key itself and both content tags are still the
//!    ones it recorded: a tag is a function of the content, so the bytes are the same. A
//!    difference is counted ([`StepReport::mismatched`](super::StepReport)) and the hot object is
//!    kept.
//! 4. An `(EVICT, SEALED)` entry is created, create-only. It fails if any entry exists, and the
//!    key is skipped. Under it both objects are looked at again: if either tag is not the one the
//!    compare saw, nothing is deleted. Otherwise, with at least the margin of the lease left, the
//!    hot object is deleted on its tag where the store honours that, and the entry is removed. An
//!    entry whose removal failed is sealed, and a later step removes it.
//!
//! A put, a delete or a copy-back of the key needs an entry too, so while the eviction's entry
//! stands they find it and wait or fail instead of acting on an object that is going.

use std::collections::BTreeMap;
use std::sync::{MutexGuard, PoisonError};

use futures::stream::{self, StreamExt};

use super::usage::Candidate;
use super::work::Replicated;
use super::{EntryReconciler, StepReport};
use crate::entry::{Created, Take};
use crate::{Error, Meta};

/// What became of one candidate.
enum Evicted {
    /// The hot object was deleted: its size.
    Removed(u64),
    /// The key was left: it has an entry, or an object changed since the compare, or a delete
    /// answered that the object is not the one named.
    Kept,
    /// The hot object is gone already.
    Gone,
    /// The hot object has no cold copy: it got an entry to be copied.
    Adopted,
    /// The cold object has other bytes: the hot object is kept.
    Mismatched,
}

/// The content tags of the copies this leader made or compared, newest kept, at most a limit of
/// keys: the memory of the fast path of the eviction.
#[derive(Default)]
pub(super) struct Tags {
    next: u64,
    by_key: BTreeMap<String, (u64, String, String)>,
    by_age: BTreeMap<u64, String>,
}

impl Tags {
    /// Records the tags of the hot and cold objects of `key`, and forgets the oldest records
    /// beyond `limit` keys (at least one).
    pub(super) fn record(&mut self, key: &str, hot: &str, cold: &str, limit: usize) {
        self.forget(key);
        self.next += 1;
        self.by_age.insert(self.next, key.to_owned());
        let record = (self.next, hot.to_owned(), cold.to_owned());
        self.by_key.insert(key.to_owned(), record);
        let excess = self.by_key.len().saturating_sub(limit.max(1));
        let oldest: Vec<String> = self.by_age.values().take(excess).cloned().collect();
        for key in &oldest {
            self.forget(key);
        }
    }

    /// Whether the records say the hot and cold objects of `key` have these tags.
    pub(super) fn matches(&self, key: &str, hot: &str, cold: &str) -> bool {
        self.by_key
            .get(key)
            .is_some_and(|(_, h, c)| h == hot && c == cold)
    }

    /// Drops every record.
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Drops the record of `key`.
    pub(super) fn forget(&mut self, key: &str) {
        if let Some((age, _, _)) = self.by_key.remove(key) {
            self.by_age.remove(&age);
        }
    }
}

impl EntryReconciler {
    /// Evicts, if the figure says the hot tier holds more than the high water of its budget: a
    /// batch of candidates after another on the pool, until it holds no more than the low water,
    /// no candidate is left, or a call fails (that candidate is kept for the next step).
    pub(super) async fn evict(&mut self, report: &mut StepReport) {
        let Some(mut bytes) = self.books.bytes else {
            return;
        };
        let budget = self.config.base.hot_budget as f64;
        let high = (budget * self.config.base.high_water) as u64;
        let low = (budget * self.config.base.low_water) as u64;
        if bytes <= high {
            return;
        }
        let parallel = self.config.base.parallel.max(1);
        while bytes > low {
            let count = parallel.min(self.books.candidates.len());
            let batch: Vec<Candidate> = self.books.candidates.drain(..count).collect();
            if batch.is_empty() {
                break;
            }
            let this = &*self;
            let tries = stream::iter(batch).map(|candidate| async move {
                let result = this.evict_one(&candidate.1).await;
                (candidate, result)
            });
            let results: Vec<_> = tries.buffer_unordered(parallel).collect().await;
            let mut failed = false;
            for (candidate, result) in results {
                match result {
                    Ok(Evicted::Removed(freed)) => {
                        bytes = bytes.saturating_sub(freed);
                        report.evicted += 1;
                        report.changed += 1;
                    }
                    Ok(Evicted::Adopted) => {
                        self.strays_adopted += 1;
                        report.strays_adopted += 1;
                    }
                    Ok(Evicted::Mismatched) => report.mismatched.push(candidate.1),
                    Ok(Evicted::Kept | Evicted::Gone) => {}
                    Err(error) => {
                        report.failed.push(error);
                        self.books.candidates.push_front(candidate);
                        failed = true;
                    }
                }
            }
            if failed {
                break;
            }
        }
        self.books.bytes = Some(bytes);
        report.evicting = bytes > low;
    }

    /// Evicts the hot object of `key` by the rules of the module.
    async fn evict_one(&self, key: &str) -> Result<Evicted, Error> {
        if self.entries.read(key).await?.is_some() {
            return Ok(Evicted::Kept);
        }
        let hot = match self.hot.head(key).await {
            Ok(hot) => hot,
            Err(Error::NotFound(_)) => return Ok(Evicted::Gone),
            Err(error) => return Err(error),
        };
        let cold = match self.cold.head(key).await {
            Ok(cold) => cold,
            Err(Error::NotFound(_)) => return self.adopt_unreplicated(key).await,
            Err(error) => return Err(error),
        };
        if !self.remembered(key, &hot, &cold)
            && let Replicated::Differs = self.compare(key, &hot, &cold).await?
        {
            return Ok(Evicted::Mismatched);
        }
        let Created::Taken(taken) = self.entries.create(key, Take::Evict).await? else {
            return Ok(Evicted::Kept);
        };
        let result = self.delete_under_entry(key, &hot, &cold).await;
        let released = self.entries.release(&taken).await;
        let evicted = result?;
        released?;
        Ok(evicted)
    }

    /// Whether this leader copied or compared both objects as they are now.
    fn remembered(&self, key: &str, hot: &Meta, cold: &Meta) -> bool {
        self.tags().matches(key, &hot.tag, &cold.tag)
    }

    /// The records of the tags of the copies this leader made, for the fast path.
    pub(super) fn tags(&self) -> MutexGuard<'_, Tags> {
        // No method panics while holding the lock, so a poisoned state is still whole.
        self.tags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Gives a hot object that has no cold copy a pending put entry, so that the next step copies
    /// it. An entry that exists is left as it is.
    async fn adopt_unreplicated(&self, key: &str) -> Result<Evicted, Error> {
        match self.entries.create(key, Take::Stray).await? {
            Created::Taken(_) => Ok(Evicted::Adopted),
            Created::Exists => Ok(Evicted::Kept),
        }
    }

    /// Under the eviction's entry: looks at both objects again, and deletes the hot one if they
    /// are the ones the compare saw and the lease has its margin left.
    async fn delete_under_entry(
        &self,
        key: &str,
        hot: &Meta,
        cold: &Meta,
    ) -> Result<Evicted, Error> {
        match self.hot.head(key).await {
            Ok(now) if now.tag == hot.tag => {}
            Ok(_) => return Ok(Evicted::Kept),
            Err(Error::NotFound(_)) => return Ok(Evicted::Gone),
            Err(error) => return Err(error),
        }
        match self.cold.head(key).await {
            Ok(now) if now.tag == cold.tag => {}
            Ok(_) | Err(Error::NotFound(_)) => return Ok(Evicted::Kept),
            Err(error) => return Err(error),
        }
        self.require_lease()?;
        let deleted = match self.hot.delete_if_match(key, &hot.tag).await {
            Err(Error::Unsupported(_)) => self.hot.delete(key).await,
            answer => answer,
        };
        match deleted {
            Ok(()) => {
                self.tags().forget(key);
                Ok(Evicted::Removed(hot.size))
            }
            Err(Error::NotFound(_)) => Ok(Evicted::Gone),
            Err(Error::PreconditionFailed(_)) => Ok(Evicted::Kept),
            Err(error) => Err(error),
        }
    }
}
