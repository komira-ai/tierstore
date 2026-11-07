//! The overwrite put (`docs/design/put-without-lock.md`, 5.3) and the same-bytes put (5.4a).
//!
//! A plain put of a key the hot tier holds, in order:
//!
//! 1. **Wave 1**, the hot HEAD and the read of the marker together ([`Inner::look`]).
//! 2. **Same bytes.** If the caller's [`PutClaim`] names the tag the hot object has and the sizes
//!    are equal, the answer is given with no write. This tier keeps no header of an object (a GET
//!    and a HEAD give the same content type for every object), so a put never changes anything
//!    but the bytes, and the design's condition that the request changes no header is always
//!    met.
//! 3. **The lock** on the marker ([`Inner::take_lock`]): create-only `LOCK(OVERWRITE)` where no
//!    marker exists, a compare-and-swap from the version read where it is `DIRTY` or `WANT` (the
//!    `gone` it found is kept), a wait (5.2) where it is a `LOCK` or a `CLEARING`. A create or a
//!    swap that loses reads the marker and decides again, at most `entry_rounds` times.
//! 4. **The replace**, `If-Match` the tag of the object. A body held whole uses the tag of wave 1
//!    and reads the object again only when the replace is refused for it (412), then tries again;
//!    a stream reads it under the lock first, since its bytes cannot be sent twice, and a replace
//!    that is refused for the tag is a 503 for the client to retry. A hot tier that no longer
//!    holds the key (a delete between wave 1 and the lock) gets a create-only put. Every
//!    destructive call starts only if [`Held::may_destroy`] says the lock is young enough, and
//!    runs beside the refresh of the lock.
//! 5. **The answer** follows the replace. The release (`DIRTY`, keeping `gone`) is made in a task
//!    of its own, and a put that ends any other way, or is dropped, releases by the guard of
//!    `lock_hold`.
//!
//! A replace whose answer is lost does not keep the lock: it names a tag, so a call that lands
//! late is refused if anything replaced the object since, and an unacknowledged put landing is a
//! legal result of it. Only a call without a tag (a DELETE) is kept (5.2, and `cold_land`).

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;

use super::Inner;
use super::entry_put::retry;
use super::lock_hold::Held;
use super::put::Offered;
use crate::{
    Creation, Error, MarkBirth, MarkMove, MarkSeen, MarkerWhy, Markers, Meta, Outcome, PartBody,
    Waited,
};

/// The body of a plain put.
pub enum PutBody<'a> {
    /// A body that is held whole and can be sent again.
    Held(Bytes),
    /// A body that arrives as chunks and can be sent once; `len` is the number of bytes it is to
    /// give, if the caller knows.
    Stream {
        /// The number of bytes the body is to give.
        len: Option<u64>,
        /// The chunks.
        body: PartBody<'a>,
    },
}

impl PutBody<'_> {
    fn len(&self) -> Option<u64> {
        match self {
            Self::Held(bytes) => Some(bytes.len() as u64),
            Self::Stream { len, .. } => *len,
        }
    }
}

/// What a put says of its body before the body is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PutClaim {
    /// The content tags the body would have in the hot tier, in the form of [`Meta::tag`] (the hex
    /// of the MD5 for an S3 hot tier, of the SHA-256 for a memory one), from the digests the
    /// request carried. Empty: the request carried none the hot tier's tag can be compared with.
    pub tags: Vec<String>,
}

impl PutClaim {
    /// Whether `meta` is the object the body would make: a tag the claim names, and the size.
    pub(super) fn is_met_by(&self, meta: &Meta, len: Option<u64>) -> bool {
        len == Some(meta.size) && self.tags.contains(&meta.tag)
    }
}

/// What a plain put did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlainPut {
    /// The content tag of the object the put made, in the tier it was made in, or of the object
    /// that was there for a put that wrote nothing. `None` where the tag a later HEAD gives is not
    /// known (a landing in the cold tier: the tag moves when the object is copied the other way).
    pub tag: Option<String>,
    /// Whether anything was written: false for a put of the bytes the key holds.
    pub wrote: bool,
}

/// A lock that is too old to destroy under, or a client that should retry.
fn stale(key: &str) -> Error {
    retry(key, "the lock was held too long to be used")
}

impl Inner {
    /// Wave 1 of an overwrite: the hot HEAD and the marker, together. The meta is `None` where the
    /// hot tier does not hold the key.
    pub(super) async fn look(
        &self,
        markers: &Markers,
        key: &str,
    ) -> Result<(Option<Meta>, Option<MarkSeen>), Error> {
        let (head, marker) = futures::join!(self.hot.head(key), markers.read(key));
        let meta = match head {
            Ok(meta) => Some(meta),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        Ok((meta, marker?))
    }

    /// The overwrite of the object `meta` (see the module).
    pub(super) async fn overwrite(
        &self,
        markers: &Arc<Markers>,
        key: &str,
        mut body: PutBody<'_>,
        claim: &PutClaim,
        meta: Meta,
        marker: Option<MarkSeen>,
    ) -> Result<PlainPut, Error> {
        if claim.is_met_by(&meta, body.len()) {
            drain(&mut body).await?;
            return Ok(PlainPut {
                tag: Some(meta.tag),
                wrote: false,
            });
        }
        let mut held = self
            .take_lock(markers, key, MarkerWhy::Overwrite, marker, false)
            .await?;
        let tag = match body {
            PutBody::Held(bytes) => self.replace_held(&mut held, key, bytes, meta.tag).await?,
            PutBody::Stream { len, body } => self.replace_stream(&mut held, key, len, body).await?,
        };
        held.release(MarkMove::Release { changed: false });
        Ok(PlainPut {
            tag: Some(tag),
            wrote: true,
        })
    }

    /// Takes the lock for `why` on the marker of `key`, which was read as `first` (none: absent).
    /// With `refuse_gone` a key the marker says is deleted is refused, whatever the lock would be.
    pub(super) async fn take_lock(
        &self,
        markers: &Arc<Markers>,
        key: &str,
        why: MarkerWhy,
        first: Option<MarkSeen>,
        refuse_gone: bool,
    ) -> Result<Held, Error> {
        let rounds = self.config.entry_rounds.max(1);
        let mut lost = 0;
        let mut seen = first;
        while lost < rounds {
            let taken = match seen {
                None => match markers.create(key, MarkBirth::Lock(why.clone())).await? {
                    Creation::Made(written) => Some(written),
                    Creation::Exists => None,
                },
                Some(found) if found.marker.st.is_held() => match markers.wait(key).await? {
                    Waited::Free(next) => {
                        seen = next;
                        continue;
                    }
                    Waited::TimedOut => {
                        return Err(retry(key, "another writer holds the lock for too long"));
                    }
                },
                Some(found) if refuse_gone && found.marker.gone => {
                    return Err(retry(
                        key,
                        "the key was deleted and its cold copy still stands",
                    ));
                }
                Some(found) => match markers.advance(&found, MarkMove::Lock(why.clone())).await? {
                    Outcome::Done(written) => Some(written),
                    Outcome::Lost | Outcome::Refused => None,
                },
            };
            if let Some(written) = taken {
                let retrying = self.config.step_retry;
                return Ok(Held::new(markers, &self.config.sleeper, retrying, written));
            }
            lost += 1;
            seen = markers.read(key).await?;
        }
        Err(retry(key, "the marker keeps changing"))
    }

    /// The tag of the hot object, or none if the hot tier does not hold the key.
    async fn head_tag(&self, key: &str) -> Result<Option<String>, Error> {
        match self.hot.head(key).await {
            Ok(meta) => Ok(Some(meta.tag)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The replace of a body held whole, starting from the tag `first`.
    async fn replace_held(
        &self,
        held: &mut Held,
        key: &str,
        bytes: Bytes,
        first: String,
    ) -> Result<String, Error> {
        let mut tag = Some(first);
        for _ in 0..self.config.entry_rounds.max(1) {
            if !held.may_destroy() {
                return Err(stale(key));
            }
            let write = async {
                match &tag {
                    Some(tag) => self.hot.put_if_match(key, bytes.clone(), tag).await,
                    None => self.hot.put_tagged(key, bytes.clone()).await,
                }
            };
            match held.lease(write).await {
                Ok(made) => return Ok(made),
                Err(
                    Error::PreconditionFailed(_) | Error::NotFound(_) | Error::AlreadyExists(_),
                ) => tag = self.head_tag(key).await?,
                Err(error) => return Err(error),
            }
        }
        Err(retry(key, "the object keeps changing"))
    }

    /// The replace of a stream, from the tag read under the lock.
    async fn replace_stream(
        &self,
        held: &mut Held,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let tag = self.head_tag(key).await?;
        if !held.may_destroy() {
            return Err(stale(key));
        }
        let offered = Offered::new(body);
        let write = async {
            match &tag {
                Some(tag) => {
                    let view = offered.view();
                    self.hot.put_stream_if_match(key, len, view, tag).await
                }
                None => self.hot.put_stream_tagged(key, len, offered.view()).await,
            }
        };
        match held.lease(write).await {
            Err(Error::PreconditionFailed(_) | Error::NotFound(_) | Error::AlreadyExists(_)) => {
                Err(retry(key, "the object changed while the body streamed"))
            }
            other => other,
        }
    }
}

/// Reads a body to its end, so that a check made on it as it passes is made.
async fn drain(body: &mut PutBody<'_>) -> Result<(), Error> {
    if let PutBody::Stream { body, .. } = body {
        while let Some(chunk) = body.next().await {
            chunk?;
        }
    }
    Ok(())
}
