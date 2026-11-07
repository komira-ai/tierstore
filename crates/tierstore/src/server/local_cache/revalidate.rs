//! Checking a hit that is old against the upstream (`local_cache.revalidate_after`).
//!
//! A hit is served from the disk with no call to the upstream, so an object that another node
//! deleted or wrote over is served on until the entry is evicted. With `revalidate_after` set the
//! entry also records when it was filled or last found to be the upstream's version, and a hit
//! older than that is checked before it is served, with **one HEAD** to the upstream whose
//! answer is compared with the entry by [`Meta::same_content`]: the ETag the entry was filled with
//! **and** the size, whatever the upstream is, so that an object written over with bytes of the
//! same length is seen. (The fill's check of an object that is being read is another question,
//! and the upstream answers it with [`Backend::same_version`]: over a proxy of two tiers that is
//! the size alone.) The body is never read for a check:
//!
//! - **Same version:** the entry's time is now, and the hit is served from the disk.
//! - **Another version, or no object (404):** the entry is dropped (`forget` of a write does the
//!   same) and the request goes on as a miss: the new object is read and kept, or the 404 is the
//!   answer.
//! - **The HEAD fails** (an error, a timeout, an upstream that is down): the cached copy is served
//!   and the failure is logged and counted. The entry's time is not changed, so the next hit asks
//!   again. A node caches to go on when the upstream is slow or down; the bound on how long it
//!   serves a changed object is therefore "while the upstream answers" and not a second setting.
//!
//! A check is claimed by the hit that finds it due: the entry's time is set to now before the HEAD
//! is sent, so that the hits that come while it is out are served and send none, which makes the
//! cost of a cached object one HEAD an interval. A claim that does not end in a verdict (the HEAD
//! failed, or the request that made it was dropped) is given back, so the next hit asks again.
//!
//! With `revalidate_after = "0s"` every hit is due, also one in the millisecond of the last check,
//! and the hits that come while a check is out each send their own.
//!
//! Over a proxy of two tiers the tag is the tag of the tier that answers, so an object that the
//! other tier answers for since the fill (the hot copy was evicted, say) may have another tag and
//! is read again once, and the entry then holds the new tag. That is the cost of telling a
//! same-size overwrite apart; it is never a stale answer.
//!
//! An entry found on the disk at start has no time: the first hit of each is checked once, by the
//! tag its sidecar holds (every sidecar has one, as a fill refuses an upstream that gives none; an
//! empty one matches no answer and is read again).
//! A clock that was set back puts an entry's time in the future, which is a check that is due.
//! Without the setting nothing here runs, and a hit is what it was before.

use crate::{Error, Meta};

use super::layout::Name;
use super::metrics::Revalidated;
use super::{Inner, locked};

/// What the check of a hit decided.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Serve the entry: no check was due, the upstream holds the version the entry stands for,
    /// or the upstream could not be asked.
    Serve,
    /// The entry was dropped: the upstream holds this other version of the object.
    Changed(Meta),
    /// The entry was dropped: the upstream has no such object.
    Gone,
}

/// The right to check one entry, given back when it is dropped without a verdict.
struct Claim<'a> {
    inner: &'a Inner,
    hex: &'a str,
    /// The time the claim set on the entry.
    at: u64,
    /// The time the entry had before.
    before: Option<u64>,
    /// Whether the check ended with the entry found to be the upstream's version, or dropped.
    settled: bool,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if !self.settled {
            locked(&self.inner.index).unclaim(self.hex, self.at, self.before);
        }
    }
}

impl Inner {
    /// Checks the hit of `key` against the upstream if it is due (see the module). The entry is
    /// dropped for [`Verdict::Changed`] and [`Verdict::Gone`], so that a read goes on as a miss.
    pub(super) async fn revalidate(&self, key: &str, name: &Name) -> Verdict {
        let Some(after) = self.config.revalidate_after else {
            return Verdict::Serve;
        };
        let now = self.clock.now();
        let after = u64::try_from(after.as_millis()).unwrap_or(u64::MAX);
        let claimed = locked(&self.index).claim(&name.hex, now, after);
        let Some((entry, before)) = claimed else {
            return Verdict::Serve;
        };
        let mut claim = Claim {
            inner: self,
            hex: &name.hex,
            at: now,
            before,
            settled: false,
        };
        match self.upstream.head(key).await {
            Ok(found) if entry.same_content(&found) => {
                claim.settled = true;
                self.counters.revalidated(Revalidated::Same);
                Verdict::Serve
            }
            Ok(found) => {
                self.counters.revalidated(Revalidated::Changed);
                self.drop_entry(&name.hex);
                Verdict::Changed(found)
            }
            Err(Error::NotFound(_)) => {
                self.counters.revalidated(Revalidated::Gone);
                self.drop_entry(&name.hex);
                Verdict::Gone
            }
            Err(failure) => {
                self.counters.revalidated(Revalidated::Failed);
                tracing::warn!(
                    "tierstore: serving {key} from the cache without checking it: {failure}"
                );
                Verdict::Serve
            }
        }
    }
}
