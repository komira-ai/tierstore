//! The plain puts of a proxy whose format is `active-marks` (`docs/design/put-without-lock.md`):
//! the entry points of the overwrite (5.3), the same-bytes put (5.4) and the emergency landing
//! (5.5). [`Proxy::put_plain`] and [`Proxy::land_in_cold`] are methods of [`Proxy`] for a library
//! caller, and a put of the [`Backend`](crate::Backend) trait is create-only. The S3 front's `PUT` without
//! `If-None-Match` calls [`Backend::put_stream_replacing`](crate::Backend::put_stream_replacing),
//! which the proxy answers with `Inner::put_replacing` below: the plain put, and the emergency landing for a body the hot
//! tier refused with [`Error::Full`] before it took a byte when
//! [`ProxyConfig::emergency_cold_landing`](super::ProxyConfig::emergency_cold_landing) is set.
//! Under every other format that call creates, and a plain put of the methods answers
//! [`Error::Unavailable`] without a request.
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
use crate::{Error, Markers, PartBody};
use bytes::Bytes;

impl Proxy {
    /// A plain put of `key`: the last writer wins (5.3). If the hot tier holds the key the object
    /// is replaced under the lock on its marker and the answer is given once the replace is made;
    /// if it does not, the object and the marker are written together (5.1);
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
    /// stream whose creation of a new key lost its race to another writer; the error of the body,
    /// if it ends with one, and of the stores.
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

    /// The plain `PUT` of the S3 front (see the module): the format `active-marks` routes it to
    /// `put_plain`, and any other format to the create-only put of the trait.
    ///
    /// A hot tier that answers [`Error::Full`] before it took a byte of the body, whether it was
    /// asked to create the key or to replace it, is answered 503 `SlowDown` (the error is passed
    /// on), unless `emergency_cold_landing` is on, and then the body goes on to
    /// `Inner::land_in_cold`. A hot tier that took part of the body before it refused it, or a
    /// body that ended with an error, is passed on: those bytes cannot be sent again.
    pub(super) async fn put_replacing(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        claim: &PutClaim,
    ) -> Result<(), Error> {
        if !matches!(self.route_write(key).await?, WriteRoute::Marks(_)) {
            return self
                .put_object_stream(key, len, body, false)
                .await
                .map(drop);
        }
        let offered = Offered::new(body);
        let stream = || PutBody::Stream {
            len,
            body: offered.view(),
        };
        match self.put_plain(key, stream(), claim).await {
            Err(Error::Full(_))
                if self.config.emergency_cold_landing
                    && offered.taken() == 0
                    && !offered.failed() =>
            {
                self.land_in_cold(key, stream()).await.map(drop)
            }
            done => done.map(drop),
        }
    }

    async fn put_plain(
        &self,
        key: &str,
        body: PutBody<'_>,
        claim: &PutClaim,
    ) -> Result<PlainPut, Error> {
        let markers = self.admit_marked(key).await?;
        match (self.head_hot(key).await?, body) {
            (Some(meta), body) => self.overwrite(markers, key, body, claim, meta).await,
            (None, PutBody::Held(bytes)) => self.put_new_held(markers, key, claim, bytes).await,
            (None, PutBody::Stream { len, body }) => {
                self.put_new_stream(markers, key, len, body).await
            }
        }
    }

    /// The fast path for a body held whole. A creation that lost its race to another writer, which
    /// made the key between the look and the write, goes on as the overwrite of what the winner
    /// made; one more look that finds no key (the winner's key was deleted already) is
    /// [`Error::Unavailable`], for the client to retry.
    async fn put_new_held(
        &self,
        markers: &Arc<Markers>,
        key: &str,
        claim: &PutClaim,
        bytes: Bytes,
    ) -> Result<PlainPut, Error> {
        let landed = self
            .land_marked(
                markers,
                key,
                || async {
                    let written = self.hot.put_tagged(key, bytes.clone()).await;
                    written.map_err(landing)
                },
                false,
            )
            .await;
        match landed {
            Ok(tag) => Ok(PlainPut {
                tag: Some(tag),
                wrote: true,
            }),
            Err(Error::AlreadyExists(_)) => match self.head_hot(key).await? {
                Some(meta) => {
                    let body = PutBody::Held(bytes);
                    self.overwrite(markers, key, body, claim, meta).await
                }
                None => Err(retry(key, "the key was made and removed while the put ran")),
            },
            Err(error) => Err(error),
        }
    }

    /// The fast path for a stream: the bytes of a body that met a creation race cannot be sent
    /// again, so that put is answered [`Error::Unavailable`] and the client's retry overwrites.
    async fn put_new_stream(
        &self,
        markers: &Markers,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<PlainPut, Error> {
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
