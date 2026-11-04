//! The put paths of the proxy: a held body and a streamed one, to the hot tier or, where the hot
//! tier is full, to the cold tier, and the creation of the object of a multipart upload. The
//! rules they follow are those of the type's rustdoc.

use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::{StreamExt, stream};
use tokio::sync::Mutex as AsyncMutex;

use super::{Inner, Level};
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
    /// is written, since a put is refused where either tier holds the key.
    async fn check_not_in_hot(&self, key: &str) -> Result<(), Error> {
        match self.hot.head(key).await {
            Ok(_) => Err(Error::AlreadyExists(key.to_owned())),
            Err(Error::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Writes the object to the cold tier, create-only, when the hot tier is full.
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
        self.admit_write(key).await?;
        self.check_not_in_hot(key).await?;
        self.cold.complete_upload(key, id, parts).await
    }

    /// Asks the cold tier again, after this call created `key` in the hot tier. The first ask
    /// (before the create) can be stale: the reconciler copies an object to the cold tier before it
    /// deletes it from the hot tier, so a create that wins the hot tier after that delete finds the
    /// cold copy here. If the cold tier holds the key, the object this call created is removed from
    /// the hot tier (best effort) and the put is [`Error::AlreadyExists`]; if the cold tier cannot
    /// be asked, the object is removed and the put fails with that error, since it cannot be told
    /// to be the only one.
    async fn confirm_not_tiered(&self, key: &str) -> Result<(), Error> {
        let refused = match self.cold.head(key).await {
            Ok(_) => Error::AlreadyExists(key.to_owned()),
            Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => error,
        };
        let _ = self.hot.delete(key).await;
        Err(refused)
    }

    async fn put_hot(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let size = body.len() as u64;
        let stored = match self.hot.put(key, body.clone()).await {
            Ok(()) => match self.confirm_not_tiered(key).await {
                Ok(()) => self.journal_put(JournalOp::Put, key, size, None).await,
                refused => refused,
            },
            failed => failed,
        };
        match stored {
            Err(Error::Full(_)) => {
                self.latch_full();
                self.put_cold(key, body).await
            }
            other => other,
        }
    }

    pub(super) async fn put_object(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.admit_write(key).await?;
        match self.cold.head(key).await {
            Ok(_) => return Err(Error::AlreadyExists(key.to_owned())),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        match self.level().await {
            Level::Full => self.put_cold(key, body).await,
            Level::Critical | Level::Open => self.put_hot(key, body).await,
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

    /// Writes the object to the hot tier from the body, then journals it. The object is in the
    /// hot tier only when the body has ended whole (the backend sees to it), and the journal
    /// entry comes after that, so a body that fails leaves neither. A hot tier with no room is
    /// latched full; the body goes to the cold tier instead if the hot tier took none of it,
    /// and else the put fails with [`Error::Full`], since the bytes taken cannot be offered
    /// again (the client's retry goes to the cold tier).
    async fn put_hot_stream(
        &self,
        key: &str,
        len: Option<u64>,
        offered: &Offered<'_>,
        hot_only: bool,
    ) -> Result<(), Error> {
        let stored = match self.hot.put_stream(key, len, offered.view()).await {
            Ok(()) => match self.confirm_not_tiered(key).await {
                Ok(()) => {
                    let taken = offered.taken();
                    self.journal_put(JournalOp::Put, key, taken, None).await
                }
                refused => refused,
            },
            failed => failed,
        };
        match stored {
            Err(Error::Full(_)) if hot_only => {
                self.latch_full();
                Err(Error::Full(key.to_owned()))
            }
            Err(Error::Full(_)) if offered.taken() == 0 => {
                self.latch_full();
                self.put_cold_stream(key, len, offered).await
            }
            Err(error @ Error::Full(_)) => {
                self.latch_full();
                Err(error)
            }
            other => other,
        }
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
        match self.cold.head(key).await {
            Ok(_) => return Err(Error::AlreadyExists(key.to_owned())),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let offered = Offered::new(body);
        match self.level().await {
            Level::Full if hot_only => Err(Error::Full(key.to_owned())),
            Level::Full => self.put_cold_stream(key, len, &offered).await,
            Level::Critical | Level::Open => {
                self.put_hot_stream(key, len, &offered, hot_only).await
            }
        }
    }
}
