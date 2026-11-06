//! The eviction of the hot tier's objects, which the leader does while the hot tier holds more
//! than the high water of its budget, and goes on doing until it holds no more than the low water.
//!
//! The candidates are the oldest objects of the last listing of the hot tier (see the usage
//! module), taken oldest first, a batch at a time on the pool. For one candidate:
//!
//! 1. No hot object means nothing to evict. No cold object means the hot object is the only copy:
//!    it is adopted by a pending put entry instead, which the next step copies, and it is not
//!    evicted.
//! 2. Both tiers have it. Unless this leader copied or compared the key itself and both content
//!    tags are still the ones it recorded (a tag is a function of the content, so the bytes are
//!    the same), the key is first looked up for an entry, and an entry of any operation and state
//!    means the key is being worked on: it is left alone, and its bytes are not read, since a put
//!    that has landed over a cold object leaves the tiers different for a while. Then the bytes
//!    are compared (ranges of
//!    [`EntryReconcilerConfig::compare_range`](super::EntryReconcilerConfig), outside any entry).
//!    A difference is counted ([`StepReport::mismatched`](super::StepReport)) and the hot object
//!    is kept.
//! 3. An `(EVICT, SEALED)` entry is created, create-only. It fails if any entry exists, and the
//!    key is skipped: for a key whose tags are recorded this is the only look at the entries,
//!    since there is no compare for an entry to spoil. Under the entry the cold object is looked
//!    at again: if its tag is not the one the compare saw, nothing is deleted. Otherwise, with at
//!    least the margin of the lease left, the hot object is deleted on its tag, and the entry is
//!    removed. The delete on a tag is what looks at the hot object again: if its tag is not the
//!    one the compare saw, the object stays. That call is one step only on a store that enforces
//!    `If-Match` on a DELETE (`conditional_delete` on). On the default S3 backend it is a HEAD, a
//!    comparison and a plain DELETE, with a window that eviction has always had, which the entry
//!    closes for the writers that follow the protocol: they find it and wait. A store that cannot
//!    delete on a tag at all gets a head of the hot object and, if the tag is still the one the
//!    compare saw, a plain delete. An entry whose removal failed is sealed, and a later step
//!    removes it.
//!
//! Only the active-format puts take an entry today, so while the eviction's entry stands they find
//! it and wait or fail instead of acting on an object that is going. A delete and a copy-back do
//! not take an entry yet: they act on the objects directly, and making them wait for an entry is
//! work still to be done.

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
        // A cycle opens above the high water and stays open, across steps, until the low water.
        if bytes <= high && !self.books.evicting {
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
        self.books.evicting = bytes > low;
        report.evicting = self.books.evicting;
    }

    /// Evicts the hot object of `key` by the rules of the module.
    async fn evict_one(&self, key: &str) -> Result<Evicted, Error> {
        let hot = match self.hot.head(key).await {
            Ok(hot) => hot,
            Err(Error::NotFound(_)) => return Ok(Evicted::Gone),
            Err(error) => return Err(error),
        };
        let cold = match self.cold.head(key).await {
            Ok(cold) => cold,
            Err(Error::NotFound(_)) => return self.adopt_unreplicated(key, hot.size).await,
            Err(error) => return Err(error),
        };
        if !self.remembered(key, &hot, &cold) {
            // Only a compare needs the key to have no entry (see the module); the create below
            // is what shuts the others out, for a key that is not compared as for one that is.
            if self.entries.read(key).await?.is_some() {
                return Ok(Evicted::Kept);
            }
            if let Replicated::Differs = self.compare(key, &hot, &cold).await? {
                return Ok(Evicted::Mismatched);
            }
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
    /// it, and counts its `size` bytes as not held by the cold tier until then. An entry that
    /// exists is left as it is.
    async fn adopt_unreplicated(&self, key: &str, size: u64) -> Result<Evicted, Error> {
        match self.entries.create(key, Take::Stray).await? {
            Created::Taken(_) => {
                self.tally.lags(key, size);
                Ok(Evicted::Adopted)
            }
            Created::Exists => Ok(Evicted::Kept),
        }
    }

    /// Under the eviction's entry: looks at the cold object again, and deletes the hot one if the
    /// cold object is the one the compare saw, the hot one is too (see [`Self::delete_hot`]), and
    /// the lease has its margin left.
    async fn delete_under_entry(
        &self,
        key: &str,
        hot: &Meta,
        cold: &Meta,
    ) -> Result<Evicted, Error> {
        match self.cold.head(key).await {
            Ok(now) if now.tag == cold.tag => {}
            Ok(_) | Err(Error::NotFound(_)) => return Ok(Evicted::Kept),
            Err(error) => return Err(error),
        }
        match self.delete_hot(key, hot).await {
            Ok(()) => {
                self.tags().forget(key);
                self.tally.evicted(key, hot.size, hot.modified);
                Ok(Evicted::Removed(hot.size))
            }
            Err(Error::NotFound(_)) => Ok(Evicted::Gone),
            Err(Error::PreconditionFailed(_)) => Ok(Evicted::Kept),
            Err(error) => Err(error),
        }
    }

    /// Deletes the hot object of `key` if it has the tag of `hot`, with the margin of the lease
    /// left before the delete. The delete on a tag is the look at the hot object: it answers that
    /// the object is gone or is not the one named, and the S3 backend that has no conditional
    /// delete of its own heads the object in it. Only a backend that cannot delete on a tag is
    /// given a head of its own here, and a plain delete after it.
    async fn delete_hot(&self, key: &str, hot: &Meta) -> Result<(), Error> {
        self.require_lease().await?;
        match self.hot.delete_if_match(key, &hot.tag).await {
            Err(Error::Unsupported(_)) => {}
            answer => return answer,
        }
        if self.hot.head(key).await?.tag != hot.tag {
            return Err(Error::PreconditionFailed(key.to_owned()));
        }
        self.require_lease().await?;
        self.hot.delete(key).await
    }
}
