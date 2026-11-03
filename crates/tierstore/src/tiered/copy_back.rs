//! The copy of an object read from the cold tier into the hot tier.

use std::sync::Arc;

use bytes::Bytes;

use super::{Inner, Level};
use crate::Error;
use crate::metrics::Skipped;

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
                // Counted when the hot tier has the object, before the journal entry below,
                // which can fail.
                self.counters.promoted(body.len());
                let inner = Arc::clone(self);
                let key = key.to_owned();
                let size = body.len() as u64;
                drop(tokio::spawn(async move {
                    let _ = inner.journal_put(&key, size).await;
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
