//! The leader's reap of the locks and clearings of owners that are gone (`docs/design/put-without-lock.md`,
//! 5.13), and its finish of a `CLEARING` whose owner stopped before it deleted the marker.
//!
//! An owner has departed when its registration is absent or has stood unchanged for `departure`
//! on the leader's monotonic clock; a marker unchanged for `lock_lease` has a holder presumed
//! stalled whether or not it beats. Then, by the table of 5.13 and the move [`MarkMove::Reap`]:
//!
//! - a `LOCK` goes to `DIRTY` if it owes cold a copy (an overwrite, a delete, an emergency
//!   landing, a reason this release does not know) and to `CLEARING`, which is removed at once, if
//!   it does not (an eviction, a copy-back). `gone` is kept as the lock found it, always.
//! - A `LOCK(DELETE)` written before its holder decided `gone` (`gone = false`) therefore goes to
//!   `DIRTY(gone = false)`: that delete never reached its decision, nothing had been deleted from
//!   hot, and it was never acknowledged, so the key is as it was. (The model reaps it to
//!   `gone = true` and so rolls the delete forward on cold; both are legal for a delete nobody was
//!   told succeeded, and this release takes the one that deletes nothing.)
//! - A marker whose `st` this release does not know goes to `DIRTY`, keeping `gone`.
//! - A `CLEARING` is removed: no one writes it but its owner, and its owner has either stopped or
//!   is this leader's earlier pass.
//!
//! A marker whose owner lives and whose lease has not run out is left, and counted as pinned. So is
//! a `LOCK` of the leader's own holder (the lock of one of its eviction and copy-back passes, or of
//! a pass of a process of the same name that died) until its lease has run out: the holder of the
//! leader is no proxy, has no registration, and would otherwise count as departed at once.

use super::EntryReconciler;
use super::marks::{Look, MarkSide, Marked};
use crate::{Error, MarkMove, MarkSeen, MarkerState, Outcome};

impl EntryReconciler {
    /// The step for a `LOCK`, a `CLEARING` or a marker of a state this release does not know.
    /// `registered` is the time the registrations were listed at: a registration is judged by the
    /// listing, not by a later time.
    pub(super) async fn mark_held(
        &self,
        side: &MarkSide,
        a: &MarkSeen,
        look: Look,
        registered: u64,
    ) -> Result<Marked, Error> {
        let marker = &a.marker;
        let owner = marker.owner.as_deref();
        let departed = owner.is_some_and(|owner| {
            self.registrations
                .departed(owner, registered, self.config.departure)
        });
        let own = owner == Some(self.config.base.holder.as_str());
        if marker.st == MarkerState::Clearing {
            if !(own || departed || look.expired) {
                return Ok(Marked::Pinned);
            }
            self.require_lease().await?;
            side.markers.remove_departed(a).await?;
            return Ok(Marked::Reaped);
        }
        // The lock of this leader's own holder is one of its passes' (an eviction or a copy-back
        // whose hot write was left open): the leader has no registration, so it never counts as
        // departed, and only the lease of the lock ends it.
        if !(look.expired || (departed && !own)) {
            return Ok(Marked::Pinned);
        }
        self.require_lease().await?;
        match side.markers.advance(a, MarkMove::Reap).await? {
            Outcome::Done(reaped) => {
                if reaped.seen.marker.st == MarkerState::Clearing {
                    self.require_lease().await?;
                    side.markers.remove(&reaped.seen).await?;
                }
                Ok(Marked::Reaped)
            }
            Outcome::Lost | Outcome::Refused => Ok(Marked::Moved),
        }
    }
}
