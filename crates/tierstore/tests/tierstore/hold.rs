//! Backends that hold a call for a test, so that a race or a kill happens at a point the test
//! names and not at a point the scheduler happens to pick.
//!
//! The waits here are unbounded on purpose: the repository's nextest configuration is the hang
//! guard and says no test carries a timeout of its own. A change to the code under test that
//! skips a gate's stall point therefore fails the test by nextest's slow-timeout, not at once.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use tierstore::{Backend, ByteRange, Error, Listed, Meta, PartBody, ReadBody};
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

/// A backend over another that holds the first delete of one key: the call says it has arrived
/// (`entered`) and goes on when `release` is notified. Every other call goes straight through.
pub struct HoldDelete {
    inner: Arc<dyn Backend>,
    key: String,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    spent: AtomicBool,
}

impl HoldDelete {
    pub fn new(
        inner: Arc<dyn Backend>,
        key: &str,
        entered: Arc<Notify>,
        release: Arc<Notify>,
    ) -> Self {
        Self {
            inner,
            key: key.to_owned(),
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
        if key == self.key && !self.spent.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.delete(key).await
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

/// What an [`OnHead`] does at one HEAD of its key. The object is changed in the backend the
/// `OnHead` wraps.
pub enum AtHead {
    /// Fails the call with this error.
    Fail(Error),
    /// Deletes the object, then fails the call with this error.
    Lost(Error),
    /// Replaces the object with these bytes (deletes it for `None`), then answers.
    Before(Option<&'static str>),
    /// Answers, then replaces the object with these bytes (deletes it for `None`).
    After(Option<&'static str>),
}

/// A backend over another that does something at the HEADs of one key, in the order they come,
/// so that another client's write happens at the point the test names: between two calls of the
/// code under test.
pub struct OnHead {
    inner: Arc<dyn Backend>,
    key: String,
    script: Mutex<VecDeque<Option<AtHead>>>,
}

impl OnHead {
    /// A backend whose HEADs of `key` follow `script`, one entry for each HEAD: `None` lets the
    /// call through, and so does every HEAD after the script ends.
    pub fn new(inner: Arc<dyn Backend>, key: &str, script: Vec<Option<AtHead>>) -> Self {
        Self {
            inner,
            key: key.to_owned(),
            script: Mutex::new(script.into()),
        }
    }

    fn next(&self) -> Option<AtHead> {
        let mut script = self.script.lock().unwrap_or_else(PoisonError::into_inner);
        script.pop_front().flatten()
    }

    async fn replace(&self, with: Option<&'static str>) {
        self.inner.delete(&self.key).await.unwrap();
        if let Some(text) = with {
            let body = Bytes::from_static(text.as_bytes());
            self.inner.put(&self.key, body).await.unwrap();
        }
    }
}

#[async_trait]
impl Backend for OnHead {
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
        let step = if key == self.key { self.next() } else { None };
        match step {
            None => self.inner.head(key).await,
            Some(AtHead::Fail(error)) => Err(error),
            Some(AtHead::Lost(error)) => {
                self.replace(None).await;
                Err(error)
            }
            Some(AtHead::Before(with)) => {
                self.replace(with).await;
                self.inner.head(key).await
            }
            Some(AtHead::After(with)) => {
                let meta = self.inner.head(key).await;
                self.replace(with).await;
                meta
            }
        }
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

/// The prefix [`Reads`] puts on the tags it reports.
const OTHER_TAG: &str = "other:";

/// A backend over another whose tags are another function of the content than the inner
/// backend's (every tag it reports has a prefix), and whose streamed reads give only the first
/// `keep` bytes and then end, with `error` if there is one. Everything else goes straight
/// through.
pub struct Reads {
    inner: Arc<dyn Backend>,
    keep: usize,
    error: Option<Error>,
}

impl Reads {
    pub fn new(inner: Arc<dyn Backend>, keep: usize, error: Option<Error>) -> Self {
        Self { inner, keep, error }
    }
}

#[async_trait]
impl Backend for Reads {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        let whole = self.inner.get(key, range).await?;
        let kept = whole.slice(0..self.keep.min(whole.len()));
        let mut items: Vec<Result<Bytes, Error>> = vec![Ok(kept)];
        items.extend(self.error.clone().map(Err));
        Ok(stream::iter(items).boxed())
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let mut meta = self.inner.head(key).await?;
        meta.tag = format!("{OTHER_TAG}{}", meta.tag);
        Ok(meta)
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        let tag = etag.strip_prefix(OTHER_TAG).unwrap_or(etag);
        self.inner.delete_if_match(key, tag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}
