//! A lock on the marker of a key that this proxy holds while it works (`docs/design/put-without-lock.md`,
//! 5.2): its lease is kept, and it is released however the operation ends.
//!
//! - **Lease.** [`Held::lease`] runs a call that may take long (a replace, a cold write) beside
//!   the refresh of the lock: every `lock_refresh` of the monotonic clock the lock is written again
//!   with a fresh nonce (M14), so the leader, which reaps a lock unchanged for `lock_lease`, does
//!   not take it from a holder that is alive. A refresh that finds the lock changed (it was reaped)
//!   ends the holding: [`Held::may_destroy`] is false from then on, and nothing is released.
//! - **Destroying.** [`Held::may_destroy`] is the check before a call that destroys (a replace, a
//!   hot DELETE, a cold write): the last write of the lock is younger than `lock_lease -
//!   queued_call_margin`.
//! - **Release.** [`Held::release`] ends the lock in a task of its own, after the operation was
//!   acknowledged, with the move it is given (and removes the `CLEARING` marker a settle leaves).
//!   [`Held::keep_for`] does so after `queued_call_margin`, for a hot DELETE whose outcome
//!   is unknown and may still land. Dropping a [`Held`] that was not released (the client went
//!   away and the future was dropped, or the operation returned early) releases the lock the same
//!   way, as `DIRTY` if it owes cold a copy: the guard of 5.2. A release that keeps failing is
//!   left to the leader, which reaps the lock.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{Either, select};

use crate::{MarkMove, Markers, Outcome, Sleeper, StepRetry, Written};

/// A lock this proxy holds on the marker of one key.
pub(super) struct Held {
    markers: Arc<Markers>,
    sleeper: Arc<dyn Sleeper>,
    retry: StepRetry,
    /// The lock as this proxy last wrote it; `None` once it is released, handed to a task, or
    /// found changed by a refresh.
    lock: Option<Written>,
}

impl Held {
    pub(super) fn new(
        markers: &Arc<Markers>,
        sleeper: &Arc<dyn Sleeper>,
        retry: StepRetry,
        lock: Written,
    ) -> Self {
        Self {
            markers: Arc::clone(markers),
            sleeper: Arc::clone(sleeper),
            retry,
            lock: Some(lock),
        }
    }

    /// Whether the lock is still this proxy's and young enough that a call which destroys may
    /// start under it (see the module).
    pub(super) fn may_destroy(&self) -> bool {
        self.lock
            .as_ref()
            .is_some_and(|lock| self.markers.may_destroy(lock))
    }

    /// Runs `call`, and writes the lock again every `lock_refresh` while it has not ended.
    pub(super) async fn lease<T>(&mut self, call: impl Future<Output = T>) -> T {
        let every = self.markers.config().lock_refresh;
        let sleeper = Arc::clone(&self.sleeper);
        let mut call = pin!(call);
        loop {
            let tick = pin!(sleeper.sleep(every));
            match select(call.as_mut(), tick).await {
                Either::Left((done, _)) => return done,
                Either::Right(((), _)) => self.refresh().await,
            }
        }
    }

    /// Writes the lock again if it is due, and keeps the new write; a lock that is found changed
    /// (or a write the store refuses) is not held any longer.
    async fn refresh(&mut self) {
        let Some(lock) = self.lock.take() else {
            return;
        };
        if !self.markers.refresh_due(&lock) {
            self.lock = Some(lock);
            return;
        }
        match self.markers.advance(&lock.seen, MarkMove::Refresh).await {
            Ok(Outcome::Done(fresh)) => self.lock = Some(fresh),
            Ok(Outcome::Lost | Outcome::Refused) => {}
            // The store could not say: the old write stands, and the next tick tries again.
            Err(_) => self.lock = Some(lock),
        }
    }

    /// Ends the lock with `mv` in a task of its own.
    pub(super) fn release(mut self, mv: MarkMove) {
        self.hand_over(mv, Duration::ZERO);
    }

    /// Keeps the lock for `margin`, the time a call whose outcome is unknown may still land in,
    /// and then releases it in a task of its own as a dropped lock is.
    pub(super) fn keep_for(&mut self, margin: Duration) {
        self.hand_over(MarkMove::Release { changed: false }, margin);
    }

    fn hand_over(&mut self, mv: MarkMove, margin: Duration) {
        let Some(lock) = self.lock.take() else {
            return;
        };
        let (markers, sleeper, retry) = (
            Arc::clone(&self.markers),
            Arc::clone(&self.sleeper),
            self.retry,
        );
        tokio::spawn(async move {
            if !margin.is_zero() {
                sleeper.sleep(margin).await;
            }
            end(&markers, sleeper.as_ref(), retry, lock, mv).await;
        });
    }
}

impl Drop for Held {
    /// The guard: a lock that was not released is released as `DIRTY` if it owes a copy.
    fn drop(&mut self) {
        self.hand_over(MarkMove::Release { changed: false }, Duration::ZERO);
    }
}

/// Ends `lock` with `mv`, trying again while the store fails the write, and removes the marker if
/// the move left a `CLEARING`. A lock that has changed is not this proxy's to end. A write the
/// store failed is told from one that lost to another writer by reading the marker: if it still
/// has the tag of the lock, the write did not land and is sent again.
async fn end(
    markers: &Markers,
    sleeper: &dyn Sleeper,
    retry: StepRetry,
    lock: Written,
    mv: MarkMove,
) {
    let mut delay = retry.delay;
    let mut left = retry.attempts.max(1);
    loop {
        left -= 1;
        let again = match markers.advance(&lock.seen, mv.clone()).await {
            Ok(Outcome::Done(ended)) => {
                // Failing to remove it leaves the marker to the leader, which finishes a CLEARING.
                let _ = markers.remove(&ended.seen).await;
                return;
            }
            // A lock of its own is never refused its release; one that is looks like a lost write.
            Ok(Outcome::Lost | Outcome::Refused) => still_there(markers, &lock).await,
            Err(_) => true,
        };
        if !again || left == 0 {
            return;
        }
        sleeper.sleep(delay).await;
        delay = delay.saturating_mul(2).min(retry.delay_max);
    }
}

/// Whether the marker still has the tag of `lock`.
async fn still_there(markers: &Markers, lock: &Written) -> bool {
    let read = markers.read(&lock.seen.marker.key).await;
    matches!(read, Ok(Some(now)) if now.tag == lock.seen.tag)
}
