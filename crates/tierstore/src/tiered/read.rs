//! The read of one object as one call: the object's meta and a body that is given as it arrives,
//! which is what a hot hit costs, one request to the hot tier.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::StreamExt;

use super::{Inner, Level};
use crate::metrics::{Read, Skipped, Tier};
use crate::{ByteRange, Error, Opened, ReadBody, Span, Window};

/// The bytes of `body`, collected.
async fn collect(mut body: ReadBody) -> Result<Bytes, Error> {
    let mut whole = BytesMut::new();
    while let Some(chunk) = body.next().await {
        whole.extend_from_slice(&chunk?);
    }
    Ok(whole.freeze())
}

impl Inner {
    /// Opens the read of `span` of the object `key` (all of it without one).
    ///
    /// The hot tier is asked once, with [`Backend::open_read`](crate::Backend::open_read), and a
    /// hit is the answer as it comes: its body is the hot tier's own stream, never collected, and
    /// no HEAD is made. Bytes are counted when the read opens, from the window, and not as the
    /// body is read.
    ///
    /// A miss goes to the cold tier. A read of a span, with the hot tier at its critical mark, is
    /// the cold tier's own stream of that span and copies nothing back. Any other read of the
    /// cold tier reads the whole object, which is copied back as [`Inner::copy_back`] says (the
    /// copy is held whole, as it is put in one call), and the answer is the window of the held
    /// bytes. A cold object that is a reservation is not an object: a miss.
    pub(super) async fn open_object(
        self: &Arc<Self>,
        key: &str,
        span: Option<Span>,
    ) -> Result<Opened, Error> {
        match self.hot.open_read(key, span).await {
            Ok(opened) => {
                let served = opened.served() as usize;
                self.counters.hit(Tier::Hot, Read::Get);
                self.counters.bytes_read(Tier::Hot, served);
                self.counters.served(Tier::Hot, served);
                return Ok(opened);
            }
            Err(Error::NotFound(_)) => self.counters.miss(Tier::Hot, Read::Get),
            Err(error) => {
                self.counters.error(Tier::Hot, Read::Get);
                return Err(error);
            }
        }
        let critical = self.level().await >= Level::Critical;
        if critical {
            self.counters.skipped(Skipped::Critical);
        }
        if critical && span.is_some() {
            // A read of a part, with the hot tier too full to take a copy, asks the cold tier for
            // that part alone: a whole read per chunk of a long blob would be quadratic.
            let opened = self.cold_open(key, span).await?;
            let served = opened.served() as usize;
            self.counters.hit(Tier::Cold, Read::Get);
            self.counters.bytes_read(Tier::Cold, served);
            self.counters.served(Tier::Cold, served);
            return Ok(opened);
        }
        let whole = self.cold_open(key, None).await?;
        let meta = whole.meta;
        let held = match collect(whole.body).await {
            Ok(held) => held,
            Err(error) => {
                self.counters.error(Tier::Cold, Read::Get);
                return Err(error);
            }
        };
        self.counters.hit(Tier::Cold, Read::Get);
        self.counters.bytes_read(Tier::Cold, held.len());
        if !critical {
            // The tag is that of the very bytes read, so the copy is journaled under it
            // whatever the key holds by then.
            self.copy_back(key, &held, meta.tag.clone()).await;
        }
        let window = Window::of(span, meta.size);
        let served = window.slice(&held);
        self.counters.served(Tier::Cold, served.len());
        let body = futures::stream::once(std::future::ready(Ok(served))).boxed();
        Ok(Opened { meta, window, body })
    }

    /// The bytes of `range` as [`Backend::get`](crate::Backend::get) gives them, as a body of one
    /// chunk: what a range of no bytes, or one that starts at or past the end, is answered with.
    pub(super) async fn held_stream(
        self: &Arc<Self>,
        key: &str,
        range: ByteRange,
    ) -> Result<ReadBody, Error> {
        let held = self.get_object(key, Some(range)).await?;
        Ok(futures::stream::once(std::future::ready(Ok(held))).boxed())
    }

    /// The cold tier's open read of `span`, with the counting of a miss or an error that an
    /// object that is not there, a reservation, or a failed call is.
    async fn cold_open(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        match self.cold.open_read(key, span).await {
            Ok(opened) if opened.meta.reservation => {
                self.counters.miss(Tier::Cold, Read::Get);
                Err(Error::NotFound(key.to_owned()))
            }
            Ok(opened) => Ok(opened),
            Err(error @ Error::NotFound(_)) => {
                self.counters.miss(Tier::Cold, Read::Get);
                Err(error)
            }
            Err(error) => {
                self.counters.error(Tier::Cold, Read::Get);
                Err(error)
            }
        }
    }
}
