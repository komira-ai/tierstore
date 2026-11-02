//! The put paths of the proxy: a held body and a streamed one, to the hot tier or, where the hot
//! tier is full, to the cold tier, and the creation of the object of a multipart upload. The
//! rules they follow are those of the type's rustdoc.

use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;

use bytes::Bytes;
use futures::{StreamExt, stream};
use tokio::sync::Mutex as AsyncMutex;

use super::{Inner, Level, refuse_reserved};
use crate::{Error, JournalOp, PartBody};

/// The body of a streamed put, which more than one tier may be offered in turn: a view of it is
/// a stream of its chunks, and the next view goes on where the last one stopped. The proxy
/// counts the bytes that were taken (the size of the object, for the journal), and a tier that
/// refuses the put before it took a byte is followed by the other tier, which gets the whole
/// body.
struct Offered<'a> {
    /// The rest of the body; `None` once it has ended or failed.
    rest: AsyncMutex<Option<PartBody<'a>>>,
    taken: AtomicU64,
}

impl<'a> Offered<'a> {
    fn new(body: PartBody<'a>) -> Self {
        Self {
            rest: AsyncMutex::new(Some(body)),
            taken: AtomicU64::new(0),
        }
    }

    /// The number of bytes of the body taken so far.
    fn taken(&self) -> u64 {
        self.taken.load(Ordering::SeqCst)
    }

    /// The rest of the body as a stream. It ends after the end of the body or an error of it.
    fn view(&self) -> PartBody<'_> {
        stream::unfold(self, |offered| async move {
            let mut rest = offered.rest.lock().await;
            let next = rest.as_mut()?.next().await;
            match &next {
                Some(Ok(chunk)) => {
                    offered
                        .taken
                        .fetch_add(chunk.len() as u64, Ordering::SeqCst);
                }
                Some(Err(_)) | None => *rest = None,
            }
            drop(rest);
            Some((next?, offered))
        })
        .boxed()
    }
}

impl Inner {
    /// Checks that the hot tier has no object `key`: the hot tier is asked before the cold tier
    /// is written by a put that does not reserve, since a put is refused where either tier holds
    /// the key.
    async fn check_not_in_hot(&self, key: &str) -> Result<(), Error> {
        match self.hot.head(key).await {
            Ok(_) => Err(Error::AlreadyExists(key.to_owned())),
            Err(Error::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Writes the object to the cold tier, create-only, when the hot tier is full. The cold
    /// tier's create decides: it is refused where the cold tier holds the key as an object or as
    /// a put's reservation. No reservation is written, and nothing is journaled.
    async fn put_cold(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.check_not_in_hot(key).await?;
        self.cold.put(key, body).await
    }

    /// Creates the object of a multipart upload in the cold tier, create-only, as `put_cold`
    /// does. The cold tier's own completion refuses a key it holds. The object is not
    /// journaled, as a put to the cold tier is not: it is in the cold tier, which is where the
    /// reconciler would copy it to.
    pub(super) async fn complete_cold(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        refuse_reserved(key)?;
        self.check_not_in_hot(key).await?;
        self.cold.complete_upload(key, id, parts).await
    }

    /// Writes the object to the hot tier, and its journal entry to the journal, after the
    /// reservation of the key in the cold tier, and acknowledges the put when both are written.
    /// The hot tier's create is polled first: if it answers at once the entry is appended after
    /// it (nothing is journaled for a create that failed); if it has not, the entry is appended
    /// while the create goes on, and the put waits for both.
    ///
    /// If either fails, the put fails and removes what it wrote: its own hot object, by tag, and
    /// its own reservation, by tag. A hot tier with no room (for the object or for the journal
    /// object) is latched full, and the object goes to the cold tier instead.
    async fn put_hot(self: &Arc<Self>, key: &str, body: Bytes) -> Result<(), Error> {
        let hold = self.reserve(key).await?;
        let size = body.len() as u64;
        let entry = self.entry(
            JournalOp::PutReserved,
            key,
            size,
            Some(hold.tag().to_owned()),
        );
        let (hot, landing, offered) = (&self.hot, &hold, body.clone());
        let mut create = pin!(async move {
            let created = hot.put(key, offered).await;
            if created.is_ok() {
                landing.hot_landed();
            }
            created
        });
        let first = poll_fn(|cx| Poll::Ready(create.as_mut().poll(cx))).await;
        let failure = match first {
            Poll::Ready(Err(error)) => Some(error),
            Poll::Ready(Ok(())) => self.append(entry).await.err(),
            Poll::Pending => {
                let (created, journaled) = futures::join!(create, self.append(entry));
                created.err().or(journaled.err())
            }
        };
        let Some(failure) = failure else {
            hold.settle();
            return Ok(());
        };
        hold.abandon().await;
        match failure {
            Error::Full(_) => {
                self.latch_full();
                self.put_cold(key, body).await
            }
            other => Err(other),
        }
    }

    pub(super) async fn put_object(self: &Arc<Self>, key: &str, body: Bytes) -> Result<(), Error> {
        refuse_reserved(key)?;
        match self.level().await {
            Level::Full => self.put_cold(key, body).await,
            Level::Critical => {
                tokio::time::sleep(self.config.back_pressure).await;
                self.put_hot(key, body).await
            }
            Level::Open => self.put_hot(key, body).await,
        }
    }

    /// Writes the object to the cold tier from the body, create-only, when the hot tier is full.
    async fn put_cold_stream(
        &self,
        key: &str,
        len: Option<u64>,
        offered: &Offered<'_>,
    ) -> Result<(), Error> {
        self.check_not_in_hot(key).await?;
        self.cold.put_stream(key, len, offered.view()).await
    }

    /// Writes the object to the hot tier from the body, then journals it, after the
    /// reservation of the key in the cold tier. The object is in the hot tier only when the body
    /// has ended whole (the backend sees to it), and the journal entry comes after that, so a
    /// body that fails leaves neither, and its reservation is released. A hot tier with no room
    /// is latched full; the body goes to the cold tier instead if the hot tier took none of it,
    /// and else the put fails with [`Error::Full`], since the bytes taken cannot be offered
    /// again (the client's retry goes to the cold tier).
    async fn put_hot_stream(
        self: &Arc<Self>,
        key: &str,
        len: Option<u64>,
        offered: &Offered<'_>,
        hot_only: bool,
    ) -> Result<(), Error> {
        let hold = self.reserve(key).await?;
        let failure = match self.hot.put_stream(key, len, offered.view()).await {
            Ok(()) => {
                hold.hot_landed();
                let tag = Some(hold.tag().to_owned());
                let entry = self.entry(JournalOp::PutReserved, key, offered.taken(), tag);
                match self.append(entry).await {
                    Ok(()) => {
                        hold.settle();
                        return Ok(());
                    }
                    Err(error) => error,
                }
            }
            Err(error) => error,
        };
        hold.abandon().await;
        match failure {
            Error::Full(_) if hot_only => {
                self.latch_full();
                Err(Error::Full(key.to_owned()))
            }
            Error::Full(_) if offered.taken() == 0 => {
                self.latch_full();
                self.put_cold_stream(key, len, offered).await
            }
            error @ Error::Full(_) => {
                self.latch_full();
                Err(error)
            }
            other => Err(other),
        }
    }

    /// As `put_object`, for a body that arrives as chunks: it goes to the hot tier, or to the
    /// cold tier where the hot tier is full, without being held.
    pub(super) async fn put_object_stream(
        self: &Arc<Self>,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        hot_only: bool,
    ) -> Result<(), Error> {
        refuse_reserved(key)?;
        let offered = Offered::new(body);
        match self.level().await {
            Level::Full if hot_only => Err(Error::Full(key.to_owned())),
            Level::Full => self.put_cold_stream(key, len, &offered).await,
            Level::Critical => {
                tokio::time::sleep(self.config.back_pressure).await;
                self.put_hot_stream(key, len, &offered, hot_only).await
            }
            Level::Open => self.put_hot_stream(key, len, &offered, hot_only).await,
        }
    }
}
