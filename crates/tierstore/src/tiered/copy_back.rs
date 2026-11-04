//! The copy of an object read from the cold tier into the hot tier.

use std::sync::Arc;

use bytes::Bytes;

use super::Inner;
use crate::Error;
use crate::entry::Take;
use crate::metrics::Skipped;

impl Inner {
    /// Asks for the copy-back of `key`, which a read found in the cold tier and not in the hot
    /// tier, on the entry of the key (format active): a create-only write of `(CB, PENDING)` in
    /// a task of its own, so that the read never waits for it. The read is not told whether it
    /// was made: if an entry of the key exists (a put, a delete or an earlier ask) the create
    /// does nothing, and a failure is ignored. No entry is read and no byte is copied here: the
    /// reconciler copies the cold object that the key holds when it takes the entry.
    pub(super) fn ask_copy_back(self: &Arc<Self>, key: &str) {
        let inner = Arc::clone(self);
        let key = key.to_owned();
        drop(tokio::spawn(async move {
            let _ = inner.entries.create(&key, Take::CopyBack).await;
        }));
    }

    /// Copies an object read from the cold tier into the hot tier, and journals the copy in
    /// the background as a mirror of the cold object: the entry names the content tag the cold
    /// object has, which the reconciler compares with the cold tier's before it keeps the copy,
    /// and the tag the hot tier's create-only write gave the copy (the same operation gives the
    /// bytes their place and the tag, with no HEAD after it), by which the reconciler deletes or
    /// evicts it. The two tags are never compared with each other.
    ///
    /// Every removal of the copy is by the hot tag (`If-Match`), so an object another proxy puts
    /// under the key after this write is never deleted by this copy, whichever path removes it
    /// (a failed journal entry here, the reconciler later). A write that gave no tag leaves no
    /// tag to delete by, and deletes nothing.
    ///
    /// `tag` is the tag the cold tier's read gave with the bytes (`Backend::get_tagged`), so it
    /// is the tag of the object the bytes were read from, and not of one that was in the key
    /// before or after the read, even one that has the same tag again after a change and a change
    /// back. No HEAD of the cold tier is made. If the key has changed since the read, the mirror names a tag that
    /// the cold tier no longer has, and the reconciler removes the copy.
    pub(super) async fn copy_back(self: &Arc<Self>, key: &str, body: &Bytes, tag: String) {
        let put = self.hot.put_tagged(key, body.clone()).await;
        self.copied(key, body.len() as u64, tag, put);
    }

    /// Settles the copy of the `size` bytes of the object `key` that the hot tier's create-only
    /// write `put` was: counts it, and journals it as [`Inner::copy_back`] says. The write is
    /// that of a read of the cold tier whose body was held ([`Inner::copy_back`]) or streamed
    /// ([`relay`](super::relay)): an abandoned stream is a write that failed, and counted so.
    pub(super) fn copied(
        self: &Arc<Self>,
        key: &str,
        size: u64,
        tag: String,
        put: Result<String, Error>,
    ) {
        match put {
            Ok(hot_tag) => {
                // Counted when the hot tier has the object, before the journal entry below,
                // which can fail.
                self.counters.promoted(size as usize);
                let inner = Arc::clone(self);
                let key = key.to_owned();
                drop(tokio::spawn(async move {
                    // The entry carries the hot tier's own tag of the copy, which the write above
                    // gave, apart from the cold tag: the two tiers need not tag the same bytes
                    // alike. A copy whose entry cannot be written is taken out by that tag.
                    let _ = inner.journal_mirror(&key, size, tag, hot_tag).await;
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
