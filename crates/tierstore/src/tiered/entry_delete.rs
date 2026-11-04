//! The deletes of a proxy whose format is active (the format says `active`): a delete is made on the
//! journal entry of its key, and the cold tier's object is deleted once the delete has acquired or
//! amended that entry. When another operation holds or has sealed the entry, the delete first
//! waits for it (see below), and until then neither tier is touched; if the wait runs out it is
//! answered [`Error::Unavailable`] with both tiers untouched.
//!
//! A delete, in order:
//!
//! 1. The proxy must be live (`Proxy::is_live`): else it is refused with [`Error::Unavailable`].
//! 2. The entry is taken, and what the delete does next depends on what it finds:
//!    - no entry: it is created, create-only, as `(DEL, HELD)` owned by this proxy (a fresh delete);
//!    - `(PUT, PENDING)` or `(CB, PENDING)`: the delete amends it to `(DEL, HELD)` of its own,
//!      and does not wait for anyone;
//!    - `(PUT, HELD)` owned by the reconciler, which holds the lease and so may be copying the
//!      object to the cold tier: the delete amends it, and waits for the release (below);
//!    - `(DEL, PENDING)`, a delete that was handed over (this is a retry of one): the delete
//!      claims it, and waits for the release;
//!    - any other entry that is held or sealed (a put or a delete of a client in flight, a put
//!      whose owner is not the reconciler, a sealed put, delete, copy-back or eviction): the delete
//!      changes nothing yet and waits, as [`DeleteWait`] says, until the entry is gone or is no
//!      longer held or sealed (the reconciler removes a sealed entry as soon as its work ends), and
//!      then takes the entry again; a copy that lands after the delete removed the object would
//!      bring the key back;
//!    - an entry in a state this release does not know: the delete is refused with
//!      [`Error::Unavailable`], and changes nothing.
//!
//!    It looks again if the entry was gone in between or the entry was released, at most
//!    [`ProxyConfig::entry_rounds`](super::ProxyConfig::entry_rounds) times. A delete that waits
//!    for an entry to be released and then meets another one is not given the wait over
//!    again: it has what is left of [`DeleteWait::wait`], and at the end is answered
//!    [`Error::Unavailable`] with nothing deleted.
//! 3. The cold tier's object is deleted, then the hot tier's, each once and cut at the margin of
//!    the registration. A call that fails hands the entry over, `(DEL, HELD)` to `(DEL, PENDING)`,
//!    and the delete is refused with the failure; a retry claims the entry and does the calls again.
//! 4. A fresh delete removes its entry. A delete that amended a pending entry hands it over and is
//!    acknowledged. A delete that amended a held put or claimed a handed-over delete hands the entry
//!    over and then waits, as [`DeleteWait`] says, until the entry is gone or is another operation's:
//!    the reconciler ends its copy, finishes the delete (which deletes both tiers again) and removes
//!    the entry, and only then is the delete acknowledged. A reconciler that does not release the
//!    entry in time (it is frozen, or its lease is lost) makes the delete answer
//!    [`Error::Unavailable`] although both tiers were deleted: the entry stays `(DEL, PENDING)`, and
//!    the retry waits again.
//!
//! The wait is what keeps a copy that was in flight when the delete came from being the object
//! that outlives it: a copy the reconciler started may still publish to the cold tier after the
//! delete removed it there, and the delete is not acknowledged until the reconciler has seen the
//! entry change and finished.

use std::future::Future;
use std::time::Duration;

use super::Inner;
use super::entry_put::retry;
use crate::Error;
use crate::entry::{Created, Entry, EntryOp, EntryState, Outcome, Seen, Step, Take};
use crate::journal::LEASE_KEY;

/// How a delete that met a copy in flight waits for the reconciler to release the entry: it reads
/// the entry, then sleeps `poll_min`, `2 * poll_min` and so on up to `poll_max` between reads,
/// until the sleeps add up to `wait`, and then gives up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeleteWait {
    /// The most time a delete waits for the release, as the sum of its sleeps. At the end the
    /// delete is answered `Unavailable` and may be retried.
    pub wait: Duration,
    /// The first sleep between two reads of the entry. A sleep shorter than a millisecond is read
    /// as a millisecond.
    pub poll_min: Duration,
    /// The longest sleep between two reads of the entry.
    pub poll_max: Duration,
}

impl DeleteWait {
    /// A wait of 30 seconds, with sleeps from 1 second doubling to 10 seconds.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            wait: Duration::from_secs(30),
            poll_min: Duration::from_secs(1),
            poll_max: Duration::from_secs(10),
        }
    }
}

impl Default for DeleteWait {
    fn default() -> Self {
        Self::new()
    }
}

/// The entry a delete holds, and the step that gave it to the delete (none for a fresh one).
struct Held {
    seen: Seen,
    step: Option<Step>,
}

/// Whether an operation of the entry's owner is in flight: the entry is held or sealed, whatever
/// the operation. A delete that meets one waits for its release.
fn in_flight(entry: &Entry) -> bool {
    matches!(entry.st, EntryState::Held | EntryState::Sealed)
}

/// A missing object is as good as a deleted one.
fn ignore_missing(answer: Result<(), Error>) -> Result<(), Error> {
    match answer {
        Err(Error::NotFound(_)) => Ok(()),
        other => other,
    }
}

impl Inner {
    /// Whether the lease says `owner` is the reconciler that holds it now: the first line of the
    /// lease object is the holder's name.
    async fn is_reconciler(&self, owner: &str) -> Result<bool, Error> {
        match self.hot.get(LEASE_KEY, None).await {
            Ok(body) => Ok(String::from_utf8_lossy(&body).lines().next() == Some(owner)),
            Err(Error::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The step by which a delete takes the entry `found`, or none if it may not.
    async fn step_for(&self, found: &Entry) -> Result<Option<Step>, Error> {
        match (found.op, found.st) {
            (EntryOp::Put | EntryOp::Cb, EntryState::Pending) => Ok(Some(Step::Amend)),
            (EntryOp::Del, EntryState::Pending) => Ok(Some(Step::Claim)),
            (EntryOp::Put, EntryState::Held) => {
                let owner = found.owner.as_deref().unwrap_or_default();
                let reconciler = self.is_reconciler(owner).await?;
                Ok(reconciler.then_some(Step::AmendCopy))
            }
            _ => Ok(None),
        }
    }

    /// Takes the entry of `key` for a delete, or says why the delete is refused.
    async fn take_delete(&self, key: &str, waited: &mut Duration) -> Result<Held, Error> {
        for _ in 0..self.config.entry_rounds {
            if let Created::Taken(seen) = self.entries.create(key, Take::Del).await? {
                return Ok(Held { seen, step: None });
            }
            let Some(found) = self.entries.read(key).await? else {
                continue;
            };
            let Some(step) = self.step_for(&found.entry).await? else {
                if !in_flight(&found.entry) {
                    return Err(retry(key, "the entry is in a state a delete cannot take"));
                }
                let reason = "an operation on the key is in flight and not released yet";
                self.wait_while(key, waited, in_flight, reason).await?;
                continue;
            };
            if let Outcome::Done(seen) = self.entries.advance(&found, step).await? {
                return Ok(Held {
                    seen,
                    step: Some(step),
                });
            }
        }
        Err(retry(key, "the entry keeps changing"))
    }

    /// Runs a call that destroys or changes something if the registration allows a call to start,
    /// cut when it has not answered within the margin.
    async fn bounded<T>(
        &self,
        key: &str,
        call: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        let deadline = &self.heartbeat.deadline;
        if !deadline.allows_start() {
            return Err(retry(key, "this proxy's registration is not live"));
        }
        let limit = deadline.margin().min(deadline.remaining());
        match tokio::time::timeout(limit, call).await {
            Ok(answer) => answer,
            Err(_) => Err(retry(key, "a call did not answer in time")),
        }
    }

    /// Deletes the cold tier's object and then the hot tier's, each once.
    async fn delete_data(&self, key: &str) -> Result<(), Error> {
        ignore_missing(self.bounded(key, self.cold.delete(key)).await)?;
        ignore_missing(self.bounded(key, self.hot.delete(key)).await)
    }

    /// Hands the entry a delete holds to the reconciler, `(DEL, HELD)` to `(DEL, PENDING)`. A
    /// handover that fails leaves the entry held, and the reconciler reaps it once this proxy is
    /// gone.
    async fn hand_over(&self, key: &str, seen: &Seen) {
        let _ = self
            .bounded(key, self.entries.advance(seen, Step::Handover))
            .await;
    }

    /// The end of a delete: a fresh one removes its entry (and hands it over if it cannot), one
    /// that amended or claimed hands it over, and one that must wait for the release does.
    async fn end_delete(&self, key: &str, held: &Held, waited: &mut Duration) -> Result<(), Error> {
        let Some(step) = held.step else {
            let released = self.bounded(key, self.entries.release(&held.seen)).await;
            if released.is_err() {
                self.hand_over(key, &held.seen).await;
            }
            return self.acknowledge(key);
        };
        self.hand_over(key, &held.seen).await;
        if step.waits() {
            return self.wait_for_release(key, waited).await;
        }
        self.acknowledge(key)
    }

    /// Waits until the entry of `key` is gone or is another operation's, as [`DeleteWait`] says,
    /// and then acknowledges if the proxy is still live.
    async fn wait_for_release(&self, key: &str, waited: &mut Duration) -> Result<(), Error> {
        let ours = |entry: &Entry| entry.op == EntryOp::Del;
        let reason = "both tiers are deleted, but the entry is not released yet";
        self.wait_while(key, waited, ours, reason).await?;
        self.acknowledge(key)
    }

    /// Reads the entry of `key` until it is gone or `waiting` says no more of it, sleeping as
    /// [`DeleteWait`] says. `waited` is the time slept so far by the delete, so that all its waits
    /// together end at [`DeleteWait::wait`], when the answer is [`Error::Unavailable`] with `reason`.
    async fn wait_while(
        &self,
        key: &str,
        waited: &mut Duration,
        waiting: impl Fn(&Entry) -> bool,
        reason: &str,
    ) -> Result<(), Error> {
        let config = self.config.delete_wait;
        let mut pause = config.poll_min;
        loop {
            let still_waiting = matches!(
                self.entries.read(key).await?,
                Some(seen) if waiting(&seen.entry)
            );
            if !still_waiting {
                return Ok(());
            }
            let left = config.wait.saturating_sub(*waited);
            if left.is_zero() {
                return Err(retry(key, reason));
            }
            let sleep = pause.max(Duration::from_millis(1)).min(left);
            tokio::time::sleep(sleep).await;
            *waited += sleep;
            pause = pause.saturating_mul(2).min(config.poll_max);
        }
    }

    /// Whether a delete that ended may be acknowledged: the proxy is still live.
    fn acknowledge(&self, key: &str) -> Result<(), Error> {
        if self.heartbeat.deadline.is_live() {
            Ok(())
        } else {
            Err(retry(key, "this proxy's registration is no longer live"))
        }
    }

    /// A delete of `key` on its entry.
    pub(super) async fn delete_on_entry(&self, key: &str) -> Result<(), Error> {
        self.require_live(key)?;
        let mut waited = Duration::ZERO;
        let held = self.take_delete(key, &mut waited).await?;
        if let Err(error) = self.delete_data(key).await {
            self.hand_over(key, &held.seen).await;
            return Err(error);
        }
        self.end_delete(key, &held, &mut waited).await
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wait_tests;
