//! The emergency landing in the cold tier (`docs/design/put-without-lock.md`, 5.5), for a plain
//! put that the hot tier refused with `Full` before it took a byte. Off unless
//! [`ProxyConfig::emergency_cold_landing`](super::ProxyConfig::emergency_cold_landing) is set.
//!
//! In order, when it is on:
//!
//! 1. The cold HEAD (the replace needs the cold tag) and the marker, together.
//! 2. The lock `LOCK(COLDLAND)`. A marker that says `gone` is refused ([`Error::Unavailable`]):
//!    nothing here can end the tombstone before cold holds the new object and no read of the old
//!    one is open.
//! 3. The cold write: create-only where cold is absent, `If-Match` its tag where it is not.
//! 4. The hot HEAD under the lock and, if hot holds the key, the hot DELETE. A DELETE that fails
//!    definitely, or a HEAD that fails, answers 503 and releases the lock as `DIRTY`: hot is the
//!    truth, the put was not acknowledged, and the copier makes cold agree with it. A DELETE whose
//!    outcome is unknown (it returned an ambiguous error, or the landing was dropped while it was
//!    in flight) may still land at the store, so the lock is kept for `queued_call_margin` and
//!    then released the same way.
//! 5. The answer follows the delete. The lock is settled to `CLEARING` and the marker removed in a
//!    task of its own, because hot is absent and cold holds the truth.

use bytes::Bytes;

use super::Inner;
use super::entry_put::retry;
use super::lock_hold::Held;
use super::overwrite::{PlainPut, PutBody};
use super::put::Offered;
use crate::entry::ambiguous;
use crate::{Error, MarkMove, MarkerWhy, Markers};

impl Inner {
    pub(super) async fn land_in_cold(
        &self,
        key: &str,
        body: PutBody<'_>,
    ) -> Result<PlainPut, Error> {
        let (markers, _writing) = self.admit_marked(key).await?;
        if !self.config.emergency_cold_landing {
            return Err(retry(key, "the hot tier is full"));
        }
        let (cold, marker) = futures::join!(self.cold.head(key), markers.read(key));
        let cold_tag = match cold {
            Ok(meta) => Some(meta.tag),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let mut held = self
            .take_lock(markers, key, MarkerWhy::ColdLand, marker?, true)
            .await?;
        self.write_cold(&mut held, key, body, cold_tag).await?;
        self.remove_hot(markers, &mut held, key).await?;
        held.release(MarkMove::Settle);
        Ok(PlainPut {
            tag: None,
            wrote: true,
        })
    }

    /// The write to the cold tier, under the lock.
    async fn write_cold(
        &self,
        held: &mut Held,
        key: &str,
        body: PutBody<'_>,
        cold_tag: Option<String>,
    ) -> Result<(), Error> {
        if !held.may_destroy() {
            return Err(retry(key, "the lock was held too long to be used"));
        }
        let written = match body {
            PutBody::Held(bytes) => held.lease(self.cold_held(key, bytes, cold_tag)).await,
            PutBody::Stream { len, body } => {
                let offered = Offered::new(body);
                held.lease(self.cold_stream(key, len, &offered, cold_tag))
                    .await
            }
        };
        match written {
            Err(Error::AlreadyExists(_) | Error::PreconditionFailed(_) | Error::NotFound(_)) => {
                Err(retry(
                    key,
                    "the cold object changed while the landing began",
                ))
            }
            other => other,
        }
    }

    async fn cold_held(
        &self,
        key: &str,
        bytes: Bytes,
        cold_tag: Option<String>,
    ) -> Result<(), Error> {
        match cold_tag {
            Some(tag) => self.cold.put_if_match(key, bytes, &tag).await.map(drop),
            None => self.cold.put(key, bytes).await,
        }
    }

    async fn cold_stream(
        &self,
        key: &str,
        len: Option<u64>,
        offered: &Offered<'_>,
        cold_tag: Option<String>,
    ) -> Result<(), Error> {
        match cold_tag {
            Some(tag) => self
                .cold
                .put_stream_if_match(key, len, offered.view(), &tag)
                .await
                .map(drop),
            None => self.cold.put_stream(key, len, offered.view()).await,
        }
    }

    /// Under the lock, deletes the hot object if there is one. An error leaves the lock to be
    /// released as `DIRTY` (the guard), or kept for the margin if the DELETE may still land.
    async fn remove_hot(&self, markers: &Markers, held: &mut Held, key: &str) -> Result<(), Error> {
        match self.hot.head(key).await {
            Ok(_) => {}
            Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        }
        if !held.may_destroy() {
            return Err(retry(key, "the lock was held too long to be used"));
        }
        match held.lease_untagged(self.hot.delete(key)).await {
            Err(error) if ambiguous(&error) => {
                held.keep_for(markers.config().queued_call_margin);
                Err(error)
            }
            other => other,
        }
    }
}
