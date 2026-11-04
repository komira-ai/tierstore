//! The body of a read of the cold tier, given to the client as it arrives and, on the way, to a
//! streamed write of the same bytes into the hot tier: the cold object is read once.
//!
//! The client is never made to wait for the hot write. The chunks the client is given are also
//! queued, in a channel with no limit of its own but a bound in bytes ([`BACKLOG_BYTES`]) that is
//! checked as each chunk is queued, for the write, which runs in a task of its own. A write that
//! falls more than the bound behind, or that ends before the body does, is abandoned: the queue
//! is dropped, the write fails and leaves nothing, and the client's body goes on. A body that is
//! dropped before the last byte of the object was given (a client that went away), that fails, or
//! that has more or fewer bytes than the cold tier's meta said, is abandoned in the same way, so
//! that no partial object is ever written under the key. The chunk that brings the bytes given to
//! the size of the meta is queued only when the body has ended cleanly there, so the object is
//! made whole by the end of the body and not by its size being reached. Only a read of the whole
//! object is relayed: a read of a range asks the cold tier for the range alone and copies nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use bytes::Bytes;
use futures::{StreamExt, stream};
use tokio::sync::mpsc;

use super::Inner;
use crate::metrics::{Read, Tier};
use crate::{Error, PartBody, ReadBody};

/// The most bytes of the cold read that wait for the hot write at one time. A hot write that is
/// further behind than this is abandoned, and the client is served on.
pub(super) const BACKLOG_BYTES: u64 = 16 * 1024 * 1024;

/// The write is still being fed.
const OPEN: u8 = 0;
/// Every byte of the object was queued: the write is to end the object.
const WHOLE: u8 = 1;
/// The write is not to make an object: it fails as soon as it looks.
const ABANDONED: u8 = 2;

/// The queue of the write: the sender, and the bytes in the queue.
struct Live {
    chunks: mpsc::UnboundedSender<Bytes>,
    queued: Arc<AtomicU64>,
}

/// Where the feeding of the write is.
enum Feed {
    /// Nothing has been queued: the write is not started, so a read whose body is never taken
    /// (a 304, a 412, a 416) opens no write.
    Idle,
    Live(Live),
    /// Ended, whole or abandoned.
    Done,
}

/// The copy of one cold read into the hot tier, fed by the chunks the read gives.
pub(super) struct Tee {
    inner: Arc<Inner>,
    key: String,
    /// The size the cold tier's meta gave, the length of the write.
    size: u64,
    /// The cold tier's tag of the bytes, for the journal entry.
    tag: String,
    /// The bytes given so far.
    seen: u64,
    /// The chunk that brought `seen` to `size`: it is queued only once the body has ended cleanly
    /// (see [`Tee::finish`]), so that the write cannot end the object before the end of the body
    /// is known to be where the meta said.
    held: Option<Bytes>,
    feed: Feed,
    outcome: Arc<AtomicU8>,
}

impl Tee {
    pub(super) fn new(inner: &Arc<Inner>, key: &str, size: u64, tag: String) -> Self {
        Self {
            inner: Arc::clone(inner),
            key: key.to_owned(),
            size,
            tag,
            seen: 0,
            held: None,
            feed: Feed::Idle,
            outcome: Arc::new(AtomicU8::new(OPEN)),
        }
    }

    /// The queue of the write, which a first call starts; `None` once the feeding has ended.
    fn live(&mut self) -> Option<&Live> {
        if matches!(self.feed, Feed::Idle) {
            self.feed = Feed::Live(self.start());
        }
        match &self.feed {
            Feed::Live(live) => Some(live),
            _ => None,
        }
    }

    /// Starts the write in a task of its own, over a body that is the queue.
    fn start(&self) -> Live {
        let (chunks, waiting) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicU64::new(0));
        let source = queue(waiting, Arc::clone(&queued), Arc::clone(&self.outcome));
        let (inner, key, size, tag) = (
            Arc::clone(&self.inner),
            self.key.clone(),
            self.size,
            self.tag.clone(),
        );
        drop(tokio::spawn(async move {
            let put = inner.hot.put_stream_tagged(&key, Some(size), source).await;
            inner.copied(&key, size, tag, put);
        }));
        Live { chunks, queued }
    }

    /// Queues a chunk the client is given. The chunk that makes up the size of the object is held
    /// back until the body ends; a chunk that goes past the size abandons the write.
    fn offer(&mut self, chunk: &Bytes) {
        let len = chunk.len() as u64;
        if len == 0 {
            // A chunk of no bytes adds nothing, and must not take the place of the held one.
            return;
        }
        self.seen += len;
        let (size, seen) = (self.size, self.seen);
        let Some(live) = self.live() else { return };
        if seen > size {
            self.abandon();
        } else if seen == size {
            // The held chunk waits for the write like the queued ones, so it counts in the bound.
            if live.queued.load(Ordering::SeqCst) + len > BACKLOG_BYTES {
                self.abandon();
            } else {
                self.held = Some(chunk.clone());
            }
        } else {
            let waiting = live.queued.fetch_add(len, Ordering::SeqCst) + len;
            if waiting > BACKLOG_BYTES || live.chunks.send(chunk.clone()).is_err() {
                self.abandon();
            }
        }
    }

    /// The cold body ended cleanly: the write is whole if the body gave the bytes the meta said,
    /// and abandoned if it gave fewer.
    fn finish(&mut self) {
        if self.seen != self.size {
            self.abandon();
            return;
        }
        let held = self.held.take();
        if let Some(live) = self.live() {
            if let Some(chunk) = held {
                live.queued.fetch_add(chunk.len() as u64, Ordering::SeqCst);
                let _ = live.chunks.send(chunk);
            }
            self.outcome.store(WHOLE, Ordering::SeqCst);
            self.feed = Feed::Done;
        }
    }

    /// Fails the write, which leaves nothing, unless it was ended whole.
    fn abandon(&mut self) {
        let _ = self
            .outcome
            .compare_exchange(OPEN, ABANDONED, Ordering::SeqCst, Ordering::SeqCst);
        self.held = None;
        self.feed = Feed::Done;
    }
}

impl Drop for Tee {
    /// A body that is dropped before it gave every byte, as one is for a client that went away.
    fn drop(&mut self) {
        self.abandon();
    }
}

/// The error a write ends with when it is abandoned.
fn abandoned() -> Error {
    Error::Other("the copy into the hot tier was abandoned".to_owned())
}

/// The body of the hot write: the chunks of the queue, ended by the whole object, or failed.
fn queue(
    waiting: mpsc::UnboundedReceiver<Bytes>,
    queued: Arc<AtomicU64>,
    outcome: Arc<AtomicU8>,
) -> PartBody<'static> {
    stream::unfold(
        (waiting, queued, outcome),
        |(mut waiting, queued, outcome)| async move {
            if outcome.load(Ordering::SeqCst) == ABANDONED {
                return Some((Err(abandoned()), (waiting, queued, outcome)));
            }
            let item = match waiting.recv().await {
                Some(chunk) => {
                    queued.fetch_sub(chunk.len() as u64, Ordering::SeqCst);
                    Some(Ok(chunk))
                }
                None if outcome.load(Ordering::SeqCst) == WHOLE => None,
                None => Some(Err(abandoned())),
            };
            item.map(|item| (item, (waiting, queued, outcome)))
        },
    )
    .boxed()
}

/// The cold body being given to the client.
struct Relay {
    inner: Arc<Inner>,
    body: ReadBody,
    tee: Option<Tee>,
    ended: bool,
}

impl Relay {
    async fn next(mut self) -> Option<(Result<Bytes, Error>, Relay)> {
        while !self.ended {
            match self.body.next().await {
                Some(Ok(chunk)) => {
                    if let Some(tee) = &mut self.tee {
                        tee.offer(&chunk);
                    }
                    return Some((Ok(chunk), self));
                }
                Some(Err(error)) => {
                    self.inner.counters.error(Tier::Cold, Read::Get);
                    self.tee.iter_mut().for_each(Tee::abandon);
                    self.ended = true;
                    return Some((Err(error), self));
                }
                None => {
                    if let Some(tee) = &mut self.tee {
                        tee.finish();
                    }
                    self.ended = true;
                }
            }
        }
        None
    }
}

/// The body the client is given for a whole cold read; `tee`, if there is one, copies every byte
/// of it.
pub(super) fn relay(inner: &Arc<Inner>, body: ReadBody, tee: Option<Tee>) -> ReadBody {
    let relay = Relay {
        inner: Arc::clone(inner),
        body,
        tee,
        ended: false,
    };
    stream::unfold(relay, Relay::next).boxed()
}
