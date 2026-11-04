//! A backend that holds a call for a test, so that a race happens at a point the test names and
//! not at a point the scheduler happens to pick.
//!
//! The waits here are unbounded on purpose: the repository's nextest configuration is the hang
//! guard and says no test carries a timeout of its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{Backend, ByteRange, Error, Listed, Meta};
use tokio::sync::Notify;

/// A backend that holds the first `get` of one key: the call says it has arrived (`entered`) and
/// goes on when `release` is notified. Every `get` of the key is counted (`gets`), the held one
/// included. Every other call goes straight through to the backend it wraps.
pub struct HoldGet {
    inner: Arc<dyn Backend>,
    key: &'static str,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    spent: AtomicBool,
    gets: AtomicUsize,
}

impl HoldGet {
    pub fn new(
        inner: Arc<dyn Backend>,
        key: &'static str,
        entered: Arc<Notify>,
        release: Arc<Notify>,
    ) -> Self {
        Self {
            inner,
            key,
            entered,
            release,
            spent: AtomicBool::new(false),
            gets: AtomicUsize::new(0),
        }
    }

    /// How many `get`s of the key have reached this backend.
    pub fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Backend for HoldGet {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        if key == self.key {
            self.gets.fetch_add(1, Ordering::SeqCst);
            if !self.spent.swap(true, Ordering::SeqCst) {
                // The body is read before the call is held, as a read that was answered and is
                // still on its way back to the caller.
                let found = self.inner.get(key, range).await;
                self.entered.notify_one();
                self.release.notified().await;
                return found;
            }
        }
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.inner.delete_if_match(key, etag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        self.inner.list_page(prefix, start_after, max_keys).await
    }
}
