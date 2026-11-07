//! The read of one object as one call: the object's meta and a body that is given as it arrives,
//! which is what a hot hit costs, one request to the hot tier.

use std::sync::Arc;

use super::mark_reads::Missed;
use super::{Inner, Level};
use crate::metrics::{Read, Skipped, Tier};
use crate::{Error, Opened, ReadBody, Span, Window};
use futures::StreamExt;

impl Inner {
    /// Opens the read of `span` of the object `key` (all of it without one).
    ///
    /// The hot tier is asked once, with [`Backend::open_read`](crate::Backend::open_read), and a
    /// hit is the answer as it comes: its body is the hot tier's own stream, never collected, and
    /// no HEAD is made. Bytes are counted when the read opens, from the window, and not as the
    /// body is read.
    ///
    /// A miss goes to the cold tier. A read of a span is the cold tier's own stream of that span
    /// and asks for no copy-back. A read of the whole object is one read of the cold tier, whose
    /// body is given to the caller as it arrives and, unless the hot tier is at its critical mark
    /// (or the format is not active), followed by an ask for the copy-back on the entry of the key
    /// (see `copy_back.rs`): the cold object is read once, nothing is held whole, and the ask
    /// neither delays nor fails the read. Bytes of the cold read are counted when it opens, from the meta, and a body that
    /// fails later counts an error as well.
    ///
    /// Where markers may be in force (see `mark_reads.rs`) a miss reads the marker of the key in
    /// parallel with the cold open, and a marker that says the key is deleted makes the answer the
    /// hot tier's second read or `NotFound`, whatever the cold tier gave; the ask for the copy-back
    /// is then the creation of a `WANT` marker, not an entry.
    ///
    /// The hot tier's level is looked at only once the cold tier has opened the read, since only a
    /// cold hit has a use for it (to count the skipped copy-back and to decide the ask): a key
    /// that neither tier has costs the two reads and no read of the usage object.
    pub(super) async fn open_object(
        self: &Arc<Self>,
        key: &str,
        span: Option<Span>,
    ) -> Result<Opened, Error> {
        match self.hot.open_read(key, span).await {
            Ok(opened) => {
                self.count_hot_open(&opened);
                return Ok(opened);
            }
            Err(Error::NotFound(_)) => self.counters.miss(Tier::Hot, Read::Get),
            Err(error) => {
                self.counters.error(Tier::Hot, Read::Get);
                return Err(error);
            }
        }
        let cold = move || self.cold_open(key, span);
        let hot_again = move || self.hot.open_read(key, span);
        let (cold, copy_back) = match self.miss_read(key, cold, hot_again).await? {
            Missed::Hot(opened) => {
                self.count_hot_open(&opened);
                return Ok(opened);
            }
            Missed::Cold { found, copy_back } => (found, copy_back),
        };
        let Opened { meta, window, body } = cold;
        if span.is_some() {
            // A read of a part asks the cold tier for that part alone and asks for no copy-back: a
            // whole read per chunk of a long blob would be quadratic, and the memory of a read is
            // its range.
            self.note_critical().await;
            let served = window.bytes(meta.size) as usize;
            self.counters.hit(Tier::Cold, Read::Get);
            self.counters.bytes_read(Tier::Cold, served);
            self.counters.served(Tier::Cold, served);
            return Ok(Opened {
                meta,
                window,
                body: self.counting_errors(body),
            });
        }
        self.counters.hit(Tier::Cold, Read::Get);
        self.counters.bytes_read(Tier::Cold, meta.size as usize);
        self.counters.served(Tier::Cold, meta.size as usize);
        if !self.note_critical().await {
            self.ask_copy_back_of(copy_back, key, meta.size).await;
        }
        Ok(Opened {
            meta,
            window: Window::Whole,
            body: self.counting_errors(body),
        })
    }

    /// Counts a read the hot tier opened: a hit, and the bytes of its window.
    fn count_hot_open(&self, opened: &Opened) {
        let served = opened.served() as usize;
        self.counters.hit(Tier::Hot, Read::Get);
        self.counters.bytes_read(Tier::Hot, served);
        self.counters.served(Tier::Hot, served);
    }

    /// Whether the hot tier is at its critical mark, counted as a read for which no copy-back is
    /// asked when it is. It is asked only of a cold read that opened: a read the cold tier does not
    /// have, or fails, has no use for the level, and the level can cost a read of the usage object.
    pub(super) async fn note_critical(&self) -> bool {
        let critical = self.level().await >= Level::Critical;
        if critical {
            self.counters.skipped(Skipped::Critical);
        }
        critical
    }

    /// The body of a cold read, which counts an error of the cold tier each time it ends with one.
    fn counting_errors(self: &Arc<Self>, body: ReadBody) -> ReadBody {
        let inner = Arc::clone(self);
        body.inspect(move |piece| {
            if piece.is_err() {
                inner.counters.error(Tier::Cold, Read::Get);
            }
        })
        .boxed()
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
