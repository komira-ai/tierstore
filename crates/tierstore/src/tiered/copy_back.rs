//! The copy of an object read from the cold tier into the hot tier.

use std::sync::Arc;

use bytes::Bytes;

use super::Inner;
use crate::metrics::Skipped;
use crate::{Error, JournalOp};

impl Inner {
    /// Copies an object read from the cold tier into the hot tier, and journals the copy in
    /// the background as a mirror of the cold object: the entry names the content tag the cold
    /// object has, which the reconciler compares with the cold tier's before it keeps the copy.
    ///
    /// `seen` is what a HEAD found before the read (`None` if the hot tier is too full to take a
    /// copy, so no HEAD was made). The tag is read with a second HEAD before the copy is made,
    /// and the copy is made only if both HEADs gave the same tag: then the object the bytes were
    /// read from is the object with that tag, and not one that replaced it between the read and
    /// the HEAD. A key the cold tier no longer holds as an object (a reservation, or nothing: it
    /// was deleted since the read) is not copied either.
    pub(super) async fn copy_back(
        self: &Arc<Self>,
        key: &str,
        body: &Bytes,
        seen: Option<Result<Option<String>, Error>>,
    ) {
        let Some(seen) = seen else {
            self.counters.skipped(Skipped::Critical);
            return;
        };
        let before = match seen {
            Ok(before) => before,
            Err(_) => {
                self.counters.skipped(Skipped::Failed);
                return;
            }
        };
        let tag = match self.cold.head(key).await {
            Ok(meta) if !meta.reservation && before.as_deref() == Some(meta.tag.as_str()) => {
                meta.tag
            }
            Ok(_) | Err(Error::NotFound(_)) => {
                self.counters.skipped(Skipped::Raced);
                return;
            }
            Err(_) => {
                self.counters.skipped(Skipped::Failed);
                return;
            }
        };
        match self.hot.put(key, body.clone()).await {
            Ok(()) => {
                // Counted when the hot tier has the object, before the journal entry below,
                // which can fail.
                self.counters.promoted(body.len());
                let inner = Arc::clone(self);
                let key = key.to_owned();
                let size = body.len() as u64;
                drop(tokio::spawn(async move {
                    let op = JournalOp::PutMirror;
                    let _ = inner.journal_put(op, &key, size, Some(tag)).await;
                }));
            }
            Err(Error::Full(_)) => {
                self.latch_full();
                self.counters.skipped(Skipped::Full);
            }
            Err(Error::AlreadyExists(_)) => self.counters.skipped(Skipped::Raced),
            Err(_) => self.counters.skipped(Skipped::Failed),
        }
    }
}
