//! A put's hold on its key: the reservation it wrote to the cold tier before it wrote anything
//! else, and what becomes of it if the put does not get to its end.

use std::sync::{Arc, Mutex};

use super::{Inner, locked};
use crate::{Error, reservation};

/// How far a put has come, which says what is left to clean up if it stops.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The reservation is written. The hot tier's create has not answered, so the put cannot tell
    /// whether the hot tier has an object of its own.
    Reserved,
    /// The hot tier has the object, which the put itself created.
    HotLanded,
    /// The put is acknowledged, or its cleanup is done: nothing is left to do.
    Settled,
}

/// The reservation of one put. The put owns the key while it holds it: the cold tier's
/// create-only write of the reservation decided that, and no other put of the key gets past
/// its own reservation until this one is released or replaced by the object.
///
/// A hold that is dropped before the put settled it (the caller gave up) cleans up in the
/// background, as far as it can tell what is there: before the hot tier answered it releases the
/// reservation; after the hot tier took the object it removes that object, by its tag, and then
/// releases. A create that was still on its way when the put was dropped may land afterwards,
/// and then the hot tier holds an object that no journal entry names: it is never copied, and
/// a put of the key is refused as existing (the first of the limits of [`Proxy`](crate::Proxy)).
/// A reservation that a hold did not get to release (the proxy was killed) stays until the
/// proxy that wrote it is retired; its body names its owner (see [`reservation::body`]).
pub(super) struct Hold {
    inner: Arc<Inner>,
    key: String,
    tag: String,
    phase: Mutex<Phase>,
}

impl Hold {
    /// The content tag of the reservation, which the journal entry of the put names.
    pub(super) fn tag(&self) -> &str {
        &self.tag
    }

    /// The hot tier took the object.
    pub(super) fn hot_landed(&self) {
        *locked(&self.phase) = Phase::HotLanded;
    }

    /// The put is acknowledged: the reservation stays, for the reconciler to replace by the
    /// object.
    pub(super) fn settle(&self) {
        *locked(&self.phase) = Phase::Settled;
    }

    /// Cleans up now, for a put that fails: removes the object the put created in the hot tier,
    /// if there is one, and releases the reservation. Both are best effort.
    pub(super) async fn abandon(&self) {
        let landed = *locked(&self.phase) == Phase::HotLanded;
        self.inner.clean_up(&self.key, &self.tag, landed).await;
        self.settle();
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        let phase = *locked(&self.phase);
        let landed = match phase {
            Phase::Reserved => false,
            Phase::HotLanded => true,
            Phase::Settled => return,
        };
        let inner = Arc::clone(&self.inner);
        let (key, tag) = (self.key.clone(), self.tag.clone());
        drop(tokio::spawn(async move {
            inner.clean_up(&key, &tag, landed).await;
        }));
    }
}

impl Inner {
    /// Reserves `key` in the cold tier: one create-only write of a marked object, which decides
    /// that this put owns the key.
    ///
    /// # Errors
    /// [`Error::AlreadyExists`] if the cold tier holds the key as an object or as another put's
    /// reservation, which includes the reservation of a put that crashed. Any other error of the
    /// cold tier fails the put: a key that cannot be reserved is not written.
    pub(super) async fn reserve(self: &Arc<Self>, key: &str) -> Result<Hold, Error> {
        let body = reservation::body(&self.config.proxy_id);
        let tag = self.cold.put_reservation(key, body).await?;
        Ok(Hold {
            inner: Arc::clone(self),
            key: key.to_owned(),
            tag,
            phase: Mutex::new(Phase::Reserved),
        })
    }

    /// Removes the object a put created in the hot tier, if `landed`, and releases its
    /// reservation by its content tag, so that a reservation that another put made since is not
    /// touched. The hot object is removed only while the cold tier still holds the put's own
    /// reservation: that reservation keeps every other put of the key out of the hot tier, so
    /// while it is there the object under the key is the put's, and once it is gone (a delete of
    /// the key took it) the object under the key may be another put's, and is left. The hot
    /// tier's tag is read with a HEAD: the put does not know it. A delete and a put of the key
    /// that both run between the check and the hot delete are the narrow case left. Errors are
    /// ignored: this runs after the put has failed, and a leftover is what the limits of the
    /// type say.
    async fn clean_up(&self, key: &str, tag: &str, landed: bool) {
        if landed
            && self.still_holds(key, tag).await
            && let Ok(meta) = self.hot.head(key).await
        {
            let _ = self.hot.delete_if_match(key, &meta.tag).await;
        }
        let _ = self.cold.delete_if_match(key, tag).await;
    }

    /// Whether the cold tier still holds the reservation with content tag `tag` under `key`. An
    /// answer that is not an answer (the HEAD failed) counts as holding: the cleanup is best
    /// effort, and a leftover object is the worse result.
    async fn still_holds(&self, key: &str, tag: &str) -> bool {
        match self.cold.head(key).await {
            Ok(meta) => meta.reservation && meta.tag == tag,
            Err(Error::NotFound(_)) => false,
            Err(_) => true,
        }
    }
}
