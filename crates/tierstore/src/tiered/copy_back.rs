//! The ask for the copy of an object read from the cold tier into the hot tier.

use std::sync::Arc;

use super::Inner;
use crate::entry::Take;

impl Inner {
    /// Asks for the copy-back of `key`, which a read found in the cold tier and not in the hot
    /// tier, on the entry of the key (format active): a create-only write of `(CB, PENDING)` in
    /// a task of its own, so that the read never waits for it. The read is not told whether it
    /// was made: if an entry of the key exists (a put, a delete or an earlier ask) the create
    /// does nothing, and a failure is ignored. The read itself reads no entry and copies no byte
    /// (a create whose answer was lost is settled by reading the entry back, `Entries::create`,
    /// in the task and not in the read). The reconciler copies the cold object that the key holds
    /// when it takes the entry.
    pub(super) fn ask_copy_back(self: &Arc<Self>, key: &str) {
        let inner = Arc::clone(self);
        let key = key.to_owned();
        drop(tokio::spawn(async move {
            let _ = inner.entries.create(&key, Take::CopyBack).await;
        }));
    }
}
