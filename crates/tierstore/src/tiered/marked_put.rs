//! The plain puts of a proxy whose format is `active-marks` (`docs/design/put-without-lock.md`):
//! the entry points of the overwrite (5.3), the same-bytes put (5.4) and the emergency landing
//! (5.5). They are methods of [`Proxy`] and no part of [`Backend`](crate::Backend): a put of the
//! trait is create-only, and it is answered as before under every format. Nothing in this
//! release's server calls them yet, and a proxy that reads any format but `active-marks` answers
//! them [`Error::Unavailable`] without a request.
//!
//! A plain put of a key that the hot tier does not hold is the fast path (5.1), which this
//! release does not have; it is answered [`Error::Unavailable`] too.

use super::entry_put::retry;
use super::{Inner, Proxy, refuse_reserved};
use crate::{Error, Markers};
use std::sync::Arc;

use super::overwrite::{PlainPut, PutBody, PutClaim};

impl Proxy {
    /// A plain put of `key`: the last writer wins (5.3). If the hot tier holds the key the object
    /// is replaced under the lock on its marker and the answer is given once the replace is made;
    /// `claim` names the content tags the body is known to have, and a body that the key holds
    /// already (same tag, same size) is answered at once with no write (5.4a).
    ///
    /// The tags of `claim` are the caller's word for the digests of the body and the proxy does not
    /// hash it: a caller that passes one must check the body against it as it streams (the S3 front
    /// ends the stream with an error when a digest does not match), and a body that is not
    /// replaced is read to its end so that the check runs.
    ///
    /// # Errors
    /// [`Error::Unavailable`] unless the format says `active-marks` and the proxy is live, while
    /// the lock is held by someone else for `lock_wait`, when the marker keeps changing, and for a
    /// key the hot tier does not hold; the error of the body, if it ends with one, and of the
    /// stores.
    pub async fn put_plain(
        &self,
        key: &str,
        body: PutBody<'_>,
        claim: &PutClaim,
    ) -> Result<PlainPut, Error> {
        self.inner.put_plain(key, body, claim).await
    }

    /// A plain put that the hot tier refused with [`Error::Full`] before it took a byte (5.5).
    /// With `emergency_cold_landing` off the answer is [`Error::Unavailable`], and the client
    /// retries. With it on the object lands in the cold tier under the lock on the marker, the hot
    /// object (if there is one) is deleted, and the answer follows the delete. The residual R6' of
    /// the design applies to it.
    ///
    /// # Errors
    /// As [`Proxy::put_plain`], and [`Error::Unavailable`] while the marker says the key is
    /// deleted (`gone`) and for a landing that is switched off.
    pub async fn land_in_cold(&self, key: &str, body: PutBody<'_>) -> Result<PlainPut, Error> {
        self.inner.land_in_cold(key, body).await
    }
}

impl Inner {
    /// The calls on the markers, if the format says `active-marks` and the proxy may write now.
    pub(super) async fn admit_marked(&self, key: &str) -> Result<&Arc<Markers>, Error> {
        refuse_reserved(key)?;
        if !self.marks_active().await {
            let why = "writes on markers wait until the format object says active-marks";
            return Err(Error::Unavailable(why.to_owned()));
        }
        let Some(markers) = &self.markers else {
            return Err(Error::Unavailable(
                "this proxy has no marks bucket; writes on markers need one".to_owned(),
            ));
        };
        self.require_live(key)?;
        Ok(markers)
    }

    async fn put_plain(
        &self,
        key: &str,
        body: PutBody<'_>,
        claim: &PutClaim,
    ) -> Result<PlainPut, Error> {
        let markers = self.admit_marked(key).await?;
        let (held, marker) = self.look(markers, key).await?;
        match held {
            Some(meta) => {
                self.overwrite(markers, key, body, claim, meta, marker)
                    .await
            }
            None => Err(retry(
                key,
                "the fast path of a put of a key the hot tier does not hold is not in this release",
            )),
        }
    }
}
