//! The plain puts of a proxy whose format is `active-marks` (`docs/design/put-without-lock.md`):
//! the entry points of the overwrite (5.3), the same-bytes put (5.4) and the emergency landing
//! (5.5). They are methods of [`Proxy`] and no part of [`Backend`](crate::Backend): a put of the
//! trait is create-only, and it is answered as before under every format. Nothing in this
//! release's server calls them yet, and a proxy that reads any format but `active-marks` answers
//! them [`Error::Unavailable`] without a request.
//!
//! A plain put of a key that the hot tier does not hold is the fast path (5.1, `marked_put.rs`):
//! the object and the marker are written together, create-only, with no cold request. A body held
//! whole that meets a creation race goes on as an overwrite; a stream, whose bytes are gone, is
//! answered [`Error::Unavailable`] for the client to retry.

use std::sync::Arc;

use super::Inner;
use super::Proxy;
use super::entry_put::{landing, retry};
use super::overwrite::{PlainPut, PutBody, PutClaim};
use super::pause::WriteRoute;
use super::put::Offered;
use crate::{Error, Markers};

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
    /// The calls on the markers, if the format says `active-marks` with the marks bucket this
    /// proxy has and the proxy may write now.
    pub(super) async fn admit_marked(&self, key: &str) -> Result<&Arc<Markers>, Error> {
        let route = self.route_write(key).await?;
        let WriteRoute::Marks(markers) = route else {
            let why = "writes on markers wait until the format object says active-marks";
            return Err(Error::Unavailable(why.to_owned()));
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
        for _ in 0..self.config.entry_rounds.max(1) {
            let (held, marker) = self.look(markers, key).await?;
            if let Some(meta) = held {
                return self
                    .overwrite(markers, key, body, claim, meta, marker)
                    .await;
            }
            let PutBody::Held(bytes) = &body else {
                return self.put_new_stream(markers, key, body).await;
            };
            let landed = self
                .land_marked(
                    markers,
                    key,
                    || async { self.hot.put_tagged(key, bytes.clone()).await.map_err(landing) },
                    false,
                )
                .await;
            match landed {
                Ok(tag) => {
                    return Ok(PlainPut {
                        tag: Some(tag),
                        wrote: true,
                    });
                }
                // Another writer created the key between the look and the write: this put is the
                // overwrite of what it made.
                Err(Error::AlreadyExists(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Err(retry(key, "the key keeps appearing and disappearing"))
    }

    /// The fast path for a stream: the bytes of a body that met a creation race cannot be sent
    /// again, so that put is answered [`Error::Unavailable`] and the client's retry overwrites.
    async fn put_new_stream(
        &self,
        markers: &Markers,
        key: &str,
        body: PutBody<'_>,
    ) -> Result<PlainPut, Error> {
        let PutBody::Stream { len, body } = body else {
            return Err(retry(key, "a held body is not streamed"));
        };
        let offered = Offered::new(body);
        let landed = self
            .land_marked(
                markers,
                key,
                || async {
                    let written = self.hot.put_stream_tagged(key, len, offered.view()).await;
                    written.map_err(|error| self.sort(&offered, error))
                },
                false,
            )
            .await;
        match landed {
            Ok(tag) => Ok(PlainPut {
                tag: Some(tag),
                wrote: true,
            }),
            Err(Error::AlreadyExists(_)) => Err(retry(
                key,
                "the key was created while the body streamed; retry the put",
            )),
            Err(error) => Err(error),
        }
    }
}
