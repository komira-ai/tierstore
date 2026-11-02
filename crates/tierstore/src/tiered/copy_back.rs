//! The copy-back of a get: an object read from the cold tier is put into the hot tier and
//! journaled, and is taken away again when the key left the cold tier meanwhile.

use std::sync::Arc;

use bytes::Bytes;

use super::{Inner, Level};
use crate::metrics::Skipped;
use crate::{Error, JournalOp};

impl Inner {
    /// Copies an object read from the cold tier into the hot tier, and journals the copy in
    /// the background.
    pub(super) async fn copy_back(self: &Arc<Self>, key: &str, body: &Bytes) {
        if self.level().await >= Level::Critical {
            self.counters.skipped(Skipped::Critical);
            return;
        }
        match self.hot.put(key, body.clone()).await {
            Ok(()) => {
                if self.keeps_copy(key, body).await {
                    // Counted when the hot tier has the object, before the journal entry
                    // below, which can fail.
                    self.counters.promoted(body.len());
                    let inner = Arc::clone(self);
                    let key = key.to_owned();
                    let size = body.len() as u64;
                    drop(tokio::spawn(async move {
                        let _ = inner.journal_put(&key, size).await;
                    }));
                } else {
                    // The key left the cold tier while the copy was made (it was deleted): the
                    // copy is gone, or is not ours, and nothing is journaled.
                    self.counters.skipped(Skipped::Raced);
                }
            }
            Err(Error::Full(_)) => {
                self.latch_full();
                self.counters.skipped(Skipped::Full);
            }
            Err(Error::AlreadyExists(_)) => self.counters.skipped(Skipped::Raced),
            Err(_) => self.counters.skipped(Skipped::Failed),
        }
    }

    /// Whether a copy that was just put into the hot tier stays: it does if the cold tier still
    /// holds the key. If it does not (the key was deleted since the get read it, or the cold
    /// tier cannot say), the copy is removed again, as far as it is the copy this call wrote,
    /// and the answer is no. The delete removes the cold tier's object before the hot tier's,
    /// so a copy that survives this check is removed by that delete, and one that does not is
    /// removed here.
    ///
    /// The answer is also yes where the copy could not be removed, so that the object in the
    /// hot tier is journaled and the reconciler can evict it.
    async fn keeps_copy(&self, key: &str, body: &Bytes) -> bool {
        if self.cold.head(key).await.is_ok() {
            return true;
        }
        match self.remove_own_copy(key, body).await {
            Ok(()) | Err(Error::NotFound(_) | Error::PreconditionFailed(_)) => false,
            Err(_) => true,
        }
    }

    /// Removes the object of the hot tier if it holds `body`, and only the object whose tag was
    /// read for that: an object put under the key after the delete is left alone, unless it has
    /// the same bytes and so the same tag.
    async fn remove_own_copy(&self, key: &str, body: &Bytes) -> Result<(), Error> {
        let tag = self.hot.head(key).await?.tag;
        if self.hot.get(key, None).await? != *body {
            return Ok(());
        }
        self.hot.delete_if_match(key, &tag).await
    }
}
