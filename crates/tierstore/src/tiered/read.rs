//! The read of one object as one call: the object's meta and a body that is given as it arrives,
//! which is what a hot hit costs, one request to the hot tier.

use std::sync::Arc;

use futures::StreamExt;

use super::relay::{Tee, relay};
use super::{Inner, Level};
use crate::metrics::{Read, Skipped, Tier};
use crate::{Error, Opened, Span, Window};

impl Inner {
    /// Opens the read of `span` of the object `key` (all of it without one).
    ///
    /// The hot tier is asked once, with [`Backend::open_read`](crate::Backend::open_read), and a
    /// hit is the answer as it comes: its body is the hot tier's own stream, never collected, and
    /// no HEAD is made. Bytes are counted when the read opens, from the window, and not as the
    /// body is read.
    ///
    /// A miss goes to the cold tier. A read of a span is the cold tier's own stream of that span
    /// and copies nothing back. A read of the whole object is one read of the cold tier, whose
    /// body is given to the caller as it arrives and, unless the hot tier is at its critical mark,
    /// fed at the same time to a streamed create-only write into the hot tier (see
    /// [`relay`](super::relay)): the cold object is read once, nothing is held whole, and the
    /// copy neither delays nor fails the read. Bytes of the cold read are counted when it opens, from the meta, and a
    /// body that fails later counts an error as well.
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
        if span.is_some() {
            // A read of a part asks the cold tier for that part alone and copies nothing back: a
            // whole read per chunk of a long blob would be quadratic, and the memory of a read is
            // its range.
            let Opened { meta, window, body } = self.cold_open(key, span).await?;
            let served = window.bytes(meta.size) as usize;
            self.counters.hit(Tier::Cold, Read::Get);
            self.counters.bytes_read(Tier::Cold, served);
            self.counters.served(Tier::Cold, served);
            // A body that fails is counted, as the relayed body of a whole read is.
            let inner = Arc::clone(self);
            let body = body
                .inspect(move |piece| {
                    if piece.is_err() {
                        inner.counters.error(Tier::Cold, Read::Get);
                    }
                })
                .boxed();
            return Ok(Opened { meta, window, body });
        }
        let Opened { meta, body, .. } = self.cold_open(key, None).await?;
        self.counters.hit(Tier::Cold, Read::Get);
        self.counters.bytes_read(Tier::Cold, meta.size as usize);
        self.counters.served(Tier::Cold, meta.size as usize);
        // The tag is that of the very bytes read, so the copy is journaled under it whatever the
        // key holds by then.
        let tee = (!critical).then(|| Tee::new(self, key, meta.size, meta.tag.clone()));
        let body = relay(self, body, tee);
        Ok(Opened {
            meta,
            window: Window::Whole,
            body,
        })
    }

    /// The cold tier's open read of `span`, with the counting of a miss or an error that an
    /// object that is not there or a failed call is.
    async fn cold_open(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        match self.cold.open_read(key, span).await {
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
