//! The put paths of the proxy: a held body and a streamed one, to the hot tier or, where the hot
//! tier is full, to the cold tier, and (in `uploads.rs`) the completion of a multipart upload. Each is
//! made on the entry of its key (`entry_put.rs`); the rules are those of the type's rustdoc.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use futures::stream;
use tokio::sync::Mutex as AsyncMutex;

use super::Inner;
use crate::{Error, PartBody};
use futures::StreamExt;

/// The body of a streamed put, which more than one tier may be offered in turn: a view of it is
/// a stream of its chunks, and the next view goes on where the last one stopped. The proxy
/// counts the bytes that were taken, and a tier that
/// refuses the put before it took a byte is followed by the other tier, which gets the whole
/// body.
pub(super) struct Offered<'a> {
    /// The rest of the body; `None` once it has ended or failed.
    rest: AsyncMutex<Option<PartBody<'a>>>,
    taken: AtomicU64,
    /// Whether the body ended with an error.
    failed: AtomicBool,
}

impl<'a> Offered<'a> {
    pub(super) fn new(body: PartBody<'a>) -> Self {
        Self {
            rest: AsyncMutex::new(Some(body)),
            taken: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        }
    }

    /// The number of bytes of the body taken so far.
    pub(super) fn taken(&self) -> u64 {
        self.taken.load(Ordering::SeqCst)
    }

    /// Whether the body ended with an error.
    pub(super) fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    /// The rest of the body as a stream. It ends after the end of the body or an error of it.
    pub(super) fn view(&self) -> PartBody<'_> {
        stream::unfold(self, |offered| async move {
            let mut rest = offered.rest.lock().await;
            let next = rest.as_mut()?.next().await;
            match &next {
                Some(Ok(chunk)) => {
                    offered
                        .taken
                        .fetch_add(chunk.len() as u64, Ordering::SeqCst);
                }
                Some(Err(_)) => {
                    offered.failed.store(true, Ordering::SeqCst);
                    *rest = None;
                }
                None => *rest = None,
            }
            drop(rest);
            Some((next?, offered))
        })
        .boxed()
    }
}

impl Inner {
    /// Puts a held body on the entry of `key`.
    pub(super) async fn put_object(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.admit_write(key).await?;
        self.put_held_on_entry(key, body).await
    }

    /// As `put_object`, for a body that arrives as chunks: it goes to the hot tier, or to the
    /// cold tier where the hot tier is full, without being held.
    pub(super) async fn put_object_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        hot_only: bool,
    ) -> Result<(), Error> {
        self.admit_write(key).await?;
        self.put_streamed_on_entry(key, len, body, hot_only).await
    }
}
