//! The leader's copy to the cold tier over the `DIRTY` markers (`docs/design/put-without-lock.md`,
//! 5.9). One call here is the work on one marker `a`, as the GET of the pass read it (its version
//! is `a.tag`), and it ends in one of the verdicts of [`Marked`]:
//!
//! - **Hot has the key** (whatever `gone` says): cold is asked for its tag. If the two tags are
//!   plain MD5s (S3's, 32 hex digits) and equal and the sizes are equal, or this leader copied the
//!   key and both tags are the ones it recorded, there is nothing to copy. Otherwise the marker is
//!   read again, and if it is not `a` the pass gives up on the key (a landing or an overwrite moved
//!   it, and the copier must not copy an older hot object over a cold object that just landed);
//!   then the hot object is streamed to cold, create-only if cold has none and on cold's tag if it
//!   has (I11), with the lease checked before the call and at every chunk. If the marker said
//!   `gone`, cold held a deleted object that has been overwritten: the marker is cleared only
//!   after the hold (below). Then the marker is written `CLEARING` on `a` (M7), and removed (M8):
//!   a write to the marker since `a` makes the first of the two fail and the key is looked at
//!   again at the next pass.
//! - **Hot lacks the key, `gone`**: the cold object is deleted (plain; an absent object is a
//!   success) and the hold starts. A pass that finds the marker unchanged once `tombstone_hold`
//!   has passed looks at hot again (a put whose marker landed before the delete may have landed
//!   its object during the hold; found, the key is copied as above), and at cold: absent, the
//!   marker is cleared; present (a stalled leader published late), deleted again and held again.
//! - **Hot lacks the key, not `gone`**: a put in flight or crashed. The marker is cleared (M13)
//!   once it has stood unchanged for `orphan_after` and hot, looked at again after that wait, still
//!   lacks the key. A put still streaming its body re-asserts its marker when its object lands.
//!
//! The copier never writes hot except the marker, and takes no lock: a write to the marker during
//! the copy is what fails its clear.

use std::sync::atomic::Ordering;

use super::EntryReconciler;
use super::marks::{Hold, Look, MarkSide, Marked};
use crate::deadline::millis;
use crate::entry::found;
use crate::{Error, MarkMove, MarkSeen, Meta, Outcome};

/// Whether `tag` is that of an S3 object put in one piece: the 32 hex digits of an MD5. A multipart
/// object's tag has a dash, and the other tiers' tags (SHA-256) are longer, and neither says the
/// bytes are the same when it equals another.
pub(super) fn is_plain_md5(tag: &str) -> bool {
    tag.len() == 32 && tag.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl EntryReconciler {
    /// The step for a `DIRTY` marker.
    pub(super) async fn mark_dirty(
        &self,
        side: &MarkSide,
        a: &MarkSeen,
        look: Look,
        now: u64,
    ) -> Result<Marked, Error> {
        let key = a.marker.key.as_str();
        match found(self.hot.head(key).await)? {
            Some(hot) => self.mark_copy(side, a, &hot, look, now).await,
            None if a.marker.gone => {
                self.tally.caught_up(key);
                self.mark_tombstone(side, a, look, now).await
            }
            None => {
                self.tally.caught_up(key);
                self.mark_orphan(side, a, look).await
            }
        }
    }

    /// Hot has the key: makes cold hold the hot object, holds if the marker said `gone`, and
    /// clears the marker.
    async fn mark_copy(
        &self,
        side: &MarkSide,
        a: &MarkSeen,
        hot: &Meta,
        look: Look,
        now: u64,
    ) -> Result<Marked, Error> {
        let key = a.marker.key.as_str();
        self.tally.copied(key, hot.size, hot.modified);
        self.tally.lags(key, hot.size);
        let cold = found(self.cold.head(key).await)?;
        let mut written = None;
        let same = |cold: &&Meta| Self::same_copy(side, key, hot, cold, &look);
        let cold_tag = match cold.as_ref().filter(same) {
            Some(cold) => cold.tag.clone(),
            None => {
                let current = side.markers.read(key).await?;
                if current.is_none_or(|current| current.tag != a.tag) {
                    return Ok(Marked::Moved);
                }
                let Some(tag) = self.mark_publish(key, hot, cold.as_ref()).await? else {
                    return Ok(Marked::Moved);
                };
                written = Some(self.config.base.mono.now());
                tag
            }
        };
        if a.marker.gone {
            let tags = Some((hot.tag.clone(), cold_tag.clone()));
            match (written, &look.held) {
                (Some(at), _) => return Ok(Marked::Held(Hold { at, tags })),
                (None, None) => {
                    let at = self.config.base.mono.now();
                    return Ok(Marked::Held(Hold { at, tags }));
                }
                (None, Some(held)) if self.holding(held.at, now) => return Ok(Marked::Pinned),
                (None, Some(_)) => {}
            }
        }
        let cleared = self.mark_clear(side, a, MarkMove::Clear).await?;
        if cleared {
            self.mark_record(side, key, (hot, &cold_tag), written, &look);
            self.tally.caught_up(key);
            return Ok(Marked::Cleared);
        }
        Ok(Marked::Moved)
    }

    /// Records the pair of tags of `key` for the eviction and the sweep, with the time on the
    /// monotonic clock at which the cold object was written: `written` if this pass wrote it, else
    /// the time of the hold that noted the pair, else the time of the record the pair already has,
    /// else the time this leader took over (it did not see the copy made).
    fn mark_record(
        &self,
        side: &MarkSide,
        key: &str,
        (hot, cold_tag): (&Meta, &str),
        written: Option<u64>,
        look: &Look,
    ) {
        let held = look
            .held
            .as_ref()
            .filter(|held| held.has_pair(&hot.tag, cold_tag))
            .map(|held| held.at);
        let copied_at = written
            .or(held)
            .or_else(|| side.records.copied_at(key, &hot.tag, cold_tag))
            .unwrap_or_else(|| side.taken_over.load(Ordering::SeqCst));
        let limit = self.config.tag_record_limit;
        side.records
            .record(key, &hot.tag, cold_tag, copied_at, limit);
    }

    /// Hot lacks the key and the marker says `gone`: deletes the cold object and starts the hold;
    /// once the hold has passed with the marker unchanged, clears it if cold lacks the key, and
    /// deletes and holds again if cold has one.
    async fn mark_tombstone(
        &self,
        side: &MarkSide,
        a: &MarkSeen,
        look: Look,
        now: u64,
    ) -> Result<Marked, Error> {
        let key = a.marker.key.as_str();
        match &look.held {
            Some(held) if self.holding(held.at, now) => Ok(Marked::Pinned),
            Some(_) if found(self.cold.head(key).await)?.is_none() => {
                if self.mark_clear(side, a, MarkMove::Clear).await? {
                    self.tally.deleted(key);
                    return Ok(Marked::Cleared);
                }
                Ok(Marked::Moved)
            }
            Some(_) | None => {
                self.require_lease().await?;
                match self.cold.delete(key).await {
                    Ok(()) | Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
                let at = self.config.base.mono.now();
                Ok(Marked::Held(Hold { at, tags: None }))
            }
        }
    }

    /// Hot lacks the key and the marker does not say `gone`: clears it once it has stood unchanged
    /// for `orphan_after`. The hot HEAD of the pass came after that wait.
    async fn mark_orphan(
        &self,
        side: &MarkSide,
        a: &MarkSeen,
        look: Look,
    ) -> Result<Marked, Error> {
        if !look.stale {
            return Ok(Marked::Pinned);
        }
        if self.mark_clear(side, a, MarkMove::ClearOrphan).await? {
            return Ok(Marked::Cleared);
        }
        Ok(Marked::Moved)
    }

    /// Whether the hold that began at `since` has not yet lasted `tombstone_hold`.
    fn holding(&self, since: u64, now: u64) -> bool {
        now.saturating_sub(since) < millis(self.config.tombstone_hold)
    }

    /// Whether the cold object has the bytes of the hot one without a read: the sizes are equal
    /// and either both tags are one plain MD5, or this leader recorded the pair, or it wrote the
    /// pair under this very version of the marker (a hold, which see).
    fn same_copy(side: &MarkSide, key: &str, hot: &Meta, cold: &Meta, look: &Look) -> bool {
        let same_md5 = is_plain_md5(&hot.tag) && hot.tag == cold.tag;
        let held = look
            .held
            .as_ref()
            .is_some_and(|held| held.has_pair(&hot.tag, &cold.tag));
        let recorded = side.records.copied_at(key, &hot.tag, &cold.tag).is_some();
        hot.size == cold.size && (same_md5 || held || recorded)
    }

    /// Writes the hot object of `key` to the cold tier: create-only if there is no cold object
    /// (`cold`), on its tag if there is. Gives the tag of the cold object written, or `None` if
    /// the write lost to another (cold changed since the HEAD) or the hot object is gone.
    async fn mark_publish(
        &self,
        key: &str,
        hot: &Meta,
        cold: Option<&Meta>,
    ) -> Result<Option<String>, Error> {
        self.require_lease().await?;
        let Some(body) = found(self.hot.get_stream(key, None).await)? else {
            return Ok(None);
        };
        self.require_lease().await?;
        let body = self.while_leased(body);
        let written = match cold {
            None => self.cold.put_stream_tagged(key, Some(hot.size), body).await,
            Some(cold) => {
                let replaced = self
                    .cold
                    .put_stream_if_match(key, Some(hot.size), body, &cold.tag);
                replaced.await
            }
        };
        match written {
            Ok(tag) => Ok(Some(tag)),
            Err(Error::AlreadyExists(_) | Error::PreconditionFailed(_) | Error::NotFound(_)) => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Writes the marker `a` to `CLEARING` by `mv` on its version and removes it, with the lease
    /// checked before each. Gives whether the clear won: a write to the marker since `a` makes it
    /// lose.
    pub(super) async fn mark_clear(
        &self,
        side: &MarkSide,
        a: &MarkSeen,
        mv: MarkMove,
    ) -> Result<bool, Error> {
        self.require_lease().await?;
        match side.markers.advance(a, mv).await? {
            Outcome::Done(clearing) => {
                self.require_lease().await?;
                side.markers.remove(&clearing.seen).await?;
                Ok(true)
            }
            Outcome::Lost | Outcome::Refused => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_plain_md5;

    /// Asserts which tags are the tag of an S3 object put in one piece: 32 hex digits, of either
    /// case, and nothing longer (a SHA-256), shorter, dashed (a multipart ETag) or quoted.
    #[test]
    fn only_32_hex_digits_are_a_plain_md5_tag() {
        assert!(is_plain_md5("0123456789abcdef0123456789ABCDEF"));
        assert!(!is_plain_md5(&"a".repeat(64)));
        assert!(!is_plain_md5(&"a".repeat(31)));
        assert!(!is_plain_md5("0123456789abcdef0123456789abcdef-3"));
        assert!(!is_plain_md5("\"0123456789abcdef0123456789abcde\""));
        assert!(!is_plain_md5("0123456789abcdef0123456789abcdeg"));
        assert!(!is_plain_md5(""));
    }
}
