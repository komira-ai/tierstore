//! The delete of a key under the format phase `active-marks` (`docs/design/put-without-lock.md`,
//! 5.6): the lock on the marker of the key, a HEAD of the hot object under it, the marker's `gone`,
//! a plain hot DELETE, the answer, and the release of the lock after it. The hot store is the only
//! store a delete asks: the cold object is deleted later by the leader, off the acknowledgement
//! path, and the marker is what keeps a read from serving it meanwhile.
//!
//! A delete, in order:
//!
//! 1. The proxy must be live (`Proxy::is_live` is not asked, but the registration must allow a
//!    call to start): else [`Error::Unavailable`], nothing touched.
//! 2. **The lock.** The marker is read (wave 1). No marker: `LOCK(DELETE)` is created, create-only
//!    (M3). `DIRTY` or `WANT`: the marker is changed by tag to `LOCK(DELETE)`, keeping the `gone`
//!    it found (M4). `LOCK` or `CLEARING` (anyone's, this proxy's earlier one included): the delete
//!    waits for it to end (`Markers::wait`: `lock_poll_min` doubling to `lock_poll_max`, up to
//!    `lock_wait`) and takes the key if it is free by then, once. A wait that ends with the lock
//!    still there, or a second lock after the first wait, is [`Error::SlowDown`]. A create or a
//!    change that loses a race reads the marker again, up to `entry_rounds` times in all, and then
//!    is [`Error::Unavailable`].
//! 3. **HEAD of the hot object under the lock.** It cannot change now except by a put that creates
//!    the key where it is absent, and such a put is not acknowledged while the lock is held.
//! 4. **`gone`: a change of the lock by tag to `LOCK(DELETE, gone = true)` (M4')**, whatever the
//!    HEAD found: a delete of a key that exists nowhere writes the marker as well (S3 answers 204
//!    for it too). It is written here and not with the lock, so that it takes effect on a key only
//!    cold holds only after hot was seen absent under the lock. If the change is lost the lock was
//!    reaped (or the write did not land): nothing was destroyed and the answer is
//!    [`Error::SlowDown`].
//! 5. **The hot DELETE**, plain (MinIO ignores `If-Match` on a DELETE), only if the HEAD found the
//!    object, started only while `Markers::may_destroy` says the last write of the lock is younger
//!    than `lock_lease - queued_call_margin`, and cut at the margin of the registration like every
//!    call that destroys (`Inner::bounded`).
//! 6. **204**, if the proxy is still live (`Inner::acknowledge`). No request to the cold tier.
//! 7. **The release**, after the answer: the lock is changed by tag to `DIRTY`, keeping `gone`
//!    (M5), tried again up to `hot_store.step_attempts` times while it fails ambiguously.
//!
//! Every call after the lock runs on a task of its own that the client's connection does not own
//! ([`Inner::delete_on_marker`]): a client that goes away after the lock was taken does not leak
//! it, and a task that ends any other way (a panic, an abort, a runtime shutting down) releases
//! it from its drop (`MarkedDelete`'s `Drop`).
//!
//! **A hot DELETE whose outcome is unknown** (it timed out, or failed in a way that does not say
//! whether the store did it) may still be queued at the store, which does not cancel it. The delete
//! is answered [`Error::SlowDown`] at once but keeps its lock for `queued_call_margin`, and only
//! then releases it, so that no put is acknowledged that the late DELETE would remove (5.2). A
//! delete that fails with an error that says the store refused the call releases at once.
//!
//! **What this does not do.** It does not delete the cold object, clear the marker or take the key
//! out of the usage tally (the copier and the leader, 5.9 and 5.14), and it does not reap a lock
//! whose holder died (5.13): a lock left by a process that died stands until the leader reaps it,
//! and a release that failed in every try stands the same way. The lock is not refreshed
//! (`Markers::refresh_due`): the delete rewrites it at step 4, which is its refresh, and a delete
//! that took longer than `lock_lease - queued_call_margin` does not destroy (step 5).

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::oneshot;

use super::Inner;
use super::entry_put::retry;
use super::entry_retry::Backoff;
use crate::entry::ambiguous;
use crate::{
    Creation, Error, MarkBirth, MarkMove, MarkSeen, MarkerWhy, Markers, Outcome, Waited, Written,
};

/// A call that gave up on a key that is busy.
fn busy(key: &str, why: &str) -> Error {
    Error::SlowDown(format!("{why} for {key}; retry after a moment"))
}

impl Inner {
    /// Deletes `key` on its marker: the steps of the module, on a task of their own, so that a
    /// client that drops this future after the lock was taken leaves nothing held. The answer is
    /// sent when the delete is acknowledged, before the lock is released.
    pub(super) async fn delete_on_marker(
        self: &Arc<Self>,
        markers: Arc<Markers>,
        key: &str,
    ) -> Result<(), Error> {
        self.require_live(key)?;
        let (answer, asked) = oneshot::channel();
        let delete = MarkedDelete::new(Arc::clone(self), markers, key);
        drop(tokio::spawn(delete.run(answer)));
        asked
            .await
            .unwrap_or_else(|_| Err(retry(key, "the delete did not finish")))
    }
}

/// A delete under way, and the lock it holds.
///
/// The drop of a delete that still holds its lock releases it on a task of its own.
pub(super) struct MarkedDelete {
    inner: Arc<Inner>,
    markers: Arc<Markers>,
    key: String,
    /// The lock as last written by this delete: none before it is taken and once it is let go.
    held: Option<Written>,
    /// Whether the hot DELETE was started and is not known to have ended.
    in_flight: bool,
}

impl MarkedDelete {
    pub(super) fn new(inner: Arc<Inner>, markers: Arc<Markers>, key: &str) -> Self {
        Self {
            inner,
            markers,
            key: key.to_owned(),
            held: None,
            in_flight: false,
        }
    }

    /// The delete, its answer, and the release of the lock. The answer is not waited for: the
    /// client may be gone.
    pub(super) async fn run(mut self, answer: oneshot::Sender<Result<(), Error>>) {
        let result = self.work().await;
        let _ = answer.send(result);
        self.finish().await;
    }

    /// Steps 2 to 6. An error leaves the lock in `held` if it is taken, and `finish` releases it.
    async fn work(&mut self) -> Result<(), Error> {
        let lock = self.take_lock().await?;
        let present = self.head().await?;
        let lock = self.decide_gone(lock).await?;
        if present {
            self.delete_hot(&lock).await?;
        }
        self.inner.acknowledge(&self.key)
    }

    /// Step 2: the lock on the marker of the key.
    pub(super) async fn take_lock(&mut self) -> Result<Written, Error> {
        let key = self.key.as_str();
        let mut waited = false;
        let mut seen = self.markers.read(key).await?;
        for _ in 0..self.inner.config.entry_rounds {
            let free = match seen {
                Some(held) if held.marker.st.is_held() => self.wait_once(&mut waited).await?,
                other => other,
            };
            let made = match free {
                None => match self
                    .markers
                    .create(key, MarkBirth::Lock(MarkerWhy::Delete))
                    .await?
                {
                    Creation::Made(lock) => Some(lock),
                    Creation::Exists => None,
                },
                Some(marker) => match self
                    .markers
                    .advance(&marker, MarkMove::Lock(MarkerWhy::Delete))
                    .await?
                {
                    Outcome::Done(lock) => Some(lock),
                    Outcome::Lost | Outcome::Refused => None,
                },
            };
            if let Some(lock) = made {
                self.held = Some(lock.clone());
                return Ok(lock);
            }
            seen = self.markers.read(key).await?;
        }
        Err(retry(key, "the marker keeps changing"))
    }

    /// The marker once the lock on it has ended. A delete waits once: a lock that stands after
    /// the wait, or comes back after it, is a key that is busy.
    async fn wait_once(&self, waited: &mut bool) -> Result<Option<MarkSeen>, Error> {
        if std::mem::replace(waited, true) {
            return Err(busy(&self.key, "the key was locked again after the wait"));
        }
        match self.markers.wait(&self.key).await? {
            Waited::Free(seen) => Ok(seen),
            Waited::TimedOut => Err(busy(&self.key, "a lock outlasted lock_wait")),
        }
    }

    /// Step 3: whether the hot tier holds the object.
    async fn head(&self) -> Result<bool, Error> {
        let key = self.key.as_str();
        match self.inner.bounded(key, self.inner.hot.head(key)).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Step 4: `gone` into the lock.
    async fn decide_gone(&mut self, lock: Written) -> Result<Written, Error> {
        match self.markers.advance(&lock.seen, MarkMove::Gone).await? {
            Outcome::Done(next) => {
                self.held = Some(next.clone());
                Ok(next)
            }
            Outcome::Lost | Outcome::Refused => Err(busy(&self.key, "the lock was lost")),
        }
    }

    /// Step 5: the hot DELETE.
    async fn delete_hot(&mut self, lock: &Written) -> Result<(), Error> {
        let key = self.key.as_str();
        if !self.markers.may_destroy(lock) {
            return Err(busy(key, "the lock is too old to delete under"));
        }
        self.in_flight = true;
        match self.inner.bounded(key, self.inner.hot.delete(key)).await {
            Ok(()) | Err(Error::NotFound(_)) => {
                self.in_flight = false;
                Ok(())
            }
            Err(error) if ambiguous(&error) => Err(busy(
                key,
                "the hot DELETE may still land and the lock is kept",
            )),
            Err(error) => {
                self.in_flight = false;
                Err(error)
            }
        }
    }

    /// Step 7: lets the lock go, here.
    async fn finish(&mut self) {
        if let Some(release) = self.release() {
            release.run().await;
        }
    }

    /// The release of the lock, if it is held; the delete holds none after this.
    pub(super) fn release(&mut self) -> Option<Release> {
        let lock = self.held.take()?;
        Some(Release {
            inner: Arc::clone(&self.inner),
            markers: Arc::clone(&self.markers),
            lock,
            queued: self.in_flight,
        })
    }
}

impl Drop for MarkedDelete {
    /// A delete that ends with its lock held, other than through `finish`, is a task that was
    /// aborted, panicked or dropped: the lock is released on a task of its own if a runtime is
    /// there to run it, and is left to the leader's reap if not.
    fn drop(&mut self) {
        let Some(release) = self.release() else {
            return;
        };
        if let Ok(runtime) = Handle::try_current() {
            drop(runtime.spawn(release.run()));
        }
    }
}

/// The release of a lock: `LOCK(DELETE)` to `DIRTY`, keeping `gone`.
pub(super) struct Release {
    pub(super) inner: Arc<Inner>,
    pub(super) markers: Arc<Markers>,
    pub(super) lock: Written,
    /// Whether a hot DELETE may still land: then the lock is kept for `queued_call_margin` first.
    pub(super) queued: bool,
}

impl Release {
    pub(super) async fn run(self) {
        let config = &self.inner.config;
        if self.queued {
            config
                .sleeper
                .sleep(config.marks_put.markers.queued_call_margin)
                .await;
        }
        let mut backoff = Backoff::new(config.step_retry);
        loop {
            let again = match self
                .markers
                .advance(&self.lock.seen, MarkMove::Release { changed: false })
                .await
            {
                Ok(Outcome::Lost) => self.unchanged().await,
                Ok(_) => false,
                Err(_) => true,
            };
            if !(again && self.inner.again(&mut backoff).await) {
                return;
            }
        }
    }

    /// Whether the marker is still the one this lock wrote: a release that was lost did not land
    /// and may be sent again, and one that finds another marker has nothing to release.
    async fn unchanged(&self) -> bool {
        let read = self.markers.read(&self.lock.seen.marker.key).await;
        matches!(read, Ok(Some(now)) if now.marker.nonce == self.lock.seen.marker.nonce)
    }
}
