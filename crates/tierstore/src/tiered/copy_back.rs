//! The copy of an object read from the cold tier into the hot tier.

use std::sync::Arc;

use bytes::Bytes;

use super::Inner;
use crate::Error;
use crate::metrics::Skipped;

impl Inner {
    /// Copies an object read from the cold tier into the hot tier, and journals the copy in
    /// the background as a mirror of the cold object: the entry names the content tag the cold
    /// object has, which the reconciler compares with the cold tier's before it keeps the copy,
    /// and the tag the hot tier gave the copy, by which the reconciler deletes or evicts it. The
    /// two tags are never compared with each other.
    ///
    /// `tag` is the tag the cold tier's read gave with the bytes (`Backend::get_marked`), so it
    /// is the tag of the object the bytes were read from, and not of one that was in the key
    /// before or after the read, even one that has the same tag again after a change and a change
    /// back. No HEAD of the cold tier is made. If the key has changed since the read, the mirror names a tag that
    /// the cold tier no longer has, and the reconciler removes the copy.
    pub(super) async fn copy_back(self: &Arc<Self>, key: &str, body: &Bytes, tag: String) {
        match self.hot.put(key, body.clone()).await {
            Ok(()) => {
                // Counted when the hot tier has the object, before the journal entry below,
                // which can fail.
                self.counters.promoted(body.len());
                let inner = Arc::clone(self);
                let key = key.to_owned();
                let size = body.len() as u64;
                drop(tokio::spawn(async move {
                    // The hot tier's own tag of the copy, which the entry carries apart from the
                    // cold tag: the two tiers need not tag the same bytes alike. The create-only
                    // write above put the copy there, so the HEAD that follows describes it; a copy
                    // whose tag cannot be read is taken out again, as one whose journal entry
                    // cannot be written is, since nothing would own it.
                    match inner.hot.head(&key).await {
                        Ok(meta) => {
                            let _ = inner.journal_mirror(&key, size, tag, meta.tag).await;
                        }
                        Err(_) => {
                            let _ = inner.hot.delete(&key).await;
                        }
                    }
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
