//! What the leader's work on markers has in common: the lease of the leader, and the two things a
//! holder of a lock does besides its work, keep the lock young enough to destroy under
//! ([`renewed`]) and end it ([`release`]) (`docs/design/put-without-lock.md`, 5.2 and the
//! transitions M5, M6, M8 and M14 of 4.3).
//!
//! The eviction ([`MarkEvictor`](crate::MarkEvictor)), the copy-back
//! ([`CopyBacker`](crate::CopyBacker)) and the stray sweep ([`StraySweep`](crate::StraySweep))
//! are the leader's. None of them is called by the server yet: the leader's step that holds the
//! lease and calls them, and the format phase `active-marks` that the step waits for, come with
//! the pull requests 7 and 10 of the design. Each of their passes reads the format object first
//! and does nothing under another phase.

use std::sync::Arc;

use futures::StreamExt;

use crate::backend::ReadBody;
use crate::deadline::millis;
use crate::lease::Lease;
use crate::marker::MarkerState;
use crate::{Backend, Error, MarkMove, Markers, Outcome, ReconcilerConfig, Written};

/// The lease of the leader, `_tiering/lease`, as the work on markers asks for it: whether it is
/// held, and whether it has its margin left before a call that destroys or publishes.
///
/// It is made over the same settings as the lease of the [`EntryReconciler`](crate::EntryReconciler)
/// (`holder`, `lease_ttl`, `lease_renew`, `lease_margin`, `mono`), and is the same object in the
/// hot tier: a process that runs both holds it once.
#[derive(Clone)]
pub struct LeaderLease {
    lease: Lease,
}

impl LeaderLease {
    /// The lease of `base.holder` over `hot`, with the times `base` gives (a renewal and a margin
    /// of a third of the ttl where it gives none). It holds nothing until [`take`](Self::take).
    #[must_use]
    pub fn new(hot: Arc<dyn Backend>, base: &ReconcilerConfig) -> Self {
        let ttl = millis(base.lease_ttl);
        let third = ttl / 3;
        let renew = base.lease_renew.map_or(third, millis);
        let margin = base.lease_margin.map_or(third, millis);
        let lease = Lease::new(
            hot,
            Arc::clone(&base.mono),
            &base.holder,
            ttl,
            renew,
            margin,
        );
        Self { lease }
    }

    /// Holds the lease after this call if it can: renews it, or takes it if it is free or has
    /// stood unchanged for the ttl. The leader's step calls it before it works.
    pub async fn take(&self) -> bool {
        self.lease.ensure().await
    }

    /// Refuses a step that destroys or publishes when less than the margin of the lease is left
    /// and a renewal now does not restore it.
    ///
    /// # Errors
    /// [`Error::Unavailable`] if the lease is not held with its margin.
    pub(crate) async fn require(&self) -> Result<(), Error> {
        if self.lease.ensure_margin().await {
            Ok(())
        } else {
            Err(lapsed())
        }
    }

    /// `body`, with each chunk that is asked for after the margin of the lease is gone turned into
    /// [`Error::Unavailable`], so that a write fed from it stops instead of running on after this
    /// process has lost the lease.
    pub(crate) fn while_held(&self, body: ReadBody) -> ReadBody {
        let lease = self.lease.clone();
        body.map(move |chunk| {
            if lease.margin_ok() {
                chunk
            } else {
                Err(lapsed())
            }
        })
        .boxed()
    }
}

/// The failure of a step that finds less than the margin of the lease left.
pub(crate) fn lapsed() -> Error {
    Error::Unavailable("less than the margin of the lease is left".to_owned())
}

/// The lock `held`, young enough to destroy under: itself if it may be (5.2), else written again
/// with a fresh nonce (M14). `None` when the marker has moved since (the leader reaped the lock,
/// or something else wrote it): the holder holds nothing and must not destroy.
///
/// # Errors
/// The store's error for the refresh, which leaves the lock as it was.
pub(crate) async fn renewed(markers: &Markers, held: Written) -> Result<Option<Written>, Error> {
    if markers.may_destroy(&held) {
        return Ok(Some(held));
    }
    match markers.advance(&held.seen, MarkMove::Refresh).await? {
        Outcome::Done(written) => Ok(Some(written)),
        Outcome::Lost | Outcome::Refused => Ok(None),
    }
}

/// Ends the lock `held` (M5, M6, M8): to `DIRTY` if the key owes cold a copy or `changed` says the
/// holder changed hot, else to `CLEARING` and then removed. A marker that has moved since (reaped,
/// or written by another) is left as it is: nothing is owed by this holder any more.
///
/// # Errors
/// The store's error for the write or the remove; the lock stays, and the leader reaps it.
pub(crate) async fn release(markers: &Markers, held: &Written, changed: bool) -> Result<(), Error> {
    match markers
        .advance(&held.seen, MarkMove::Release { changed })
        .await?
    {
        Outcome::Done(written) if written.seen.marker.st == MarkerState::Clearing => {
            markers.remove(&written.seen).await.map(|_| ())
        }
        Outcome::Done(_) | Outcome::Lost | Outcome::Refused => Ok(()),
    }
}
