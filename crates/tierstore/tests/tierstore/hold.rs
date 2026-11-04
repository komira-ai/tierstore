//! Backends that hold a call for a test, so that a race or a kill happens at a point the test
//! names and not at a point the scheduler happens to pick.
//!
//! The waits here are unbounded on purpose: the repository's nextest configuration is the hang
//! guard and says no test carries a timeout of its own. A change to the code under test that
//! skips a gate's stall point therefore fails the test by nextest's slow-timeout, not at once.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{Backend, ByteRange, Error, Listed, MemoryBackend, Meta, PartBody};
use tokio::sync::{Barrier, Notify};

/// A backend whose first `parties` writes of one key wait for each other: every writer is
/// inside the call, past every check of the proxy, before any of them goes on to the backend it
/// wraps. A call after those goes straight through, and so does a call on another key.
pub struct Gate {
    inner: Arc<dyn Backend>,
    key: String,
    barrier: Barrier,
    arrivals: AtomicUsize,
    parties: usize,
    /// Only a put that replaces on a content tag waits.
    replaces_only: bool,
}

impl Gate {
    /// A gate on the writes of `key`.
    pub fn new(inner: Arc<dyn Backend>, key: &str, parties: usize) -> Self {
        Self {
            inner,
            key: key.to_owned(),
            barrier: Barrier::new(parties),
            arrivals: AtomicUsize::new(0),
            parties,
            replaces_only: false,
        }
    }

    /// A gate on the puts of `key` that replace on a content tag (`If-Match`), and on nothing
    /// else.
    pub fn on_replaces(inner: Arc<dyn Backend>, key: &str, parties: usize) -> Self {
        Self {
            replaces_only: true,
            ..Self::new(inner, key, parties)
        }
    }

    async fn wait(&self, key: &str) {
        if !self.replaces_only {
            self.arrive(key).await;
        }
    }

    async fn arrive(&self, key: &str) {
        if key == self.key && self.arrivals.fetch_add(1, Ordering::SeqCst) < self.parties {
            self.barrier.wait().await;
        }
    }
}

#[async_trait]
impl Backend for Gate {
    fn needs_length(&self) -> bool {
        self.inner.needs_length()
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.wait(key).await;
        self.inner.put(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.wait(key).await;
        self.inner.put_stream(key, len, body).await
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.wait(key).await;
        self.inner.put_stream_hot_only(key, len, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.arrive(key).await;
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
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
}

/// A backend that stops the first write whose key starts with a prefix: the call says it has
/// arrived (`entered`), and then either never goes on (`release` is None, the writer is to be
/// killed here) or goes on when `release` is notified. Nothing reaches the backend it wraps from
/// that call until then. Later calls, and calls on other keys, go straight through.
pub struct Stall {
    inner: Arc<dyn Backend>,
    prefix: String,
    entered: Arc<Notify>,
    release: Option<Arc<Notify>>,
    spent: AtomicBool,
}

impl Stall {
    pub fn new(
        inner: Arc<dyn Backend>,
        prefix: &str,
        entered: Arc<Notify>,
        release: Option<Arc<Notify>>,
    ) -> Self {
        Self {
            inner,
            prefix: prefix.to_owned(),
            entered,
            release,
            spent: AtomicBool::new(false),
        }
    }

    async fn stop(&self, key: &str) {
        if key.starts_with(&self.prefix) && !self.spent.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            match &self.release {
                Some(release) => release.notified().await,
                None => std::future::pending::<()>().await,
            }
        }
    }
}

#[async_trait]
impl Backend for Stall {
    fn needs_length(&self) -> bool {
        self.inner.needs_length()
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.stop(key).await;
        self.inner.put(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.stop(key).await;
        self.inner.put_stream(key, len, body).await
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.stop(key).await;
        self.inner.put_stream_hot_only(key, len, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
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
}

/// A backend that, on the first HEAD of one key, first writes a body to the key in a memory
/// backend (the one under the wrapped backend), as another process would between two calls of the
/// caller: the HEAD that follows sees it. Everything else goes straight through.
pub struct Interpose {
    inner: Arc<dyn Backend>,
    under: Arc<MemoryBackend>,
    key: String,
    body: Bytes,
    spent: AtomicBool,
}

impl Interpose {
    pub fn new(inner: Arc<dyn Backend>, under: Arc<MemoryBackend>, key: &str, body: Bytes) -> Self {
        Self {
            inner,
            under,
            key: key.to_owned(),
            body,
            spent: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Backend for Interpose {
    fn needs_length(&self) -> bool {
        self.inner.needs_length()
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_stream(key, len, body).await
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_stream_hot_only(key, len, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        if key == self.key && !self.spent.swap(true, Ordering::SeqCst) {
            self.under.put(key, self.body.clone()).await?;
        }
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.inner.delete_if_match(key, etag).await
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

/// A backend that holds the first conditional delete (`delete_if_match`) of one key: the call says
/// it has arrived (`entered`) and goes on when `release` is notified. Every other call goes
/// straight through to the backend it wraps.
pub struct HoldDelete {
    inner: Arc<dyn Backend>,
    key: &'static str,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    spent: AtomicBool,
}

impl HoldDelete {
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
        }
    }
}

#[async_trait]
impl Backend for HoldDelete {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        if key == self.key && !self.spent.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
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
