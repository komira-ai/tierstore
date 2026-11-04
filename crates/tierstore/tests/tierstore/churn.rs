//! Backends that change what a tier holds at a point a test names: between a look at an object
//! and the conditional call that follows it, or around a read.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use tierstore::{Backend, ByteRange, Error, Listed, MemoryBackend, Meta, PartBody};

/// The chunks of `body` joined, or the error that ends it.
async fn collect(mut body: PartBody<'_>) -> Result<Bytes, Error> {
    let mut whole = BytesMut::new();
    while let Some(chunk) = body.next().await {
        whole.extend_from_slice(&chunk?);
    }
    Ok(whole.freeze())
}

/// What [`Churn`] does to the object under the key of a conditional call, just before the call.
#[derive(Clone, Debug)]
pub enum Change {
    /// Replaces it with an object of these bytes.
    Object(Bytes),
    /// Removes it.
    Remove,
}

/// A memory backend whose `put_if_match` and `delete_if_match` each first apply the next of the
/// changes it was given (if any are left) to the object under the key, so that the call finds
/// what a racing writer left, as it would between the look that gave the caller the tag and the
/// call; and, if asked to, whose first read with its tag is preceded and followed by a change, so
/// that the read finds, and a look after it finds, what a racing writer left. Everything else is
/// the memory backend's own.
pub struct Churn {
    mem: Arc<MemoryBackend>,
    left: Mutex<VecDeque<Change>>,
    after_read: Mutex<Option<Change>>,
    before_tagged_read: Mutex<Option<Change>>,
    /// The only key whose conditional calls apply the changes, if one was named.
    only: Option<String>,
}

impl Churn {
    pub fn new(mem: Arc<MemoryBackend>, changes: Vec<Change>) -> Self {
        let left = Mutex::new(changes.into());
        Self {
            mem,
            left,
            after_read: Mutex::new(None),
            before_tagged_read: Mutex::new(None),
            only: None,
        }
    }

    /// Applies the changes only before a conditional call on `key`: a call on another key (the
    /// lease, a checkpoint) leaves them for the one that is meant.
    pub fn on_key(mut self, key: &str) -> Self {
        self.only = Some(key.to_owned());
        self
    }

    /// A backend whose first read with its tag is followed by `change`.
    pub fn after_a_read(mem: Arc<MemoryBackend>, change: Change) -> Self {
        let churn = Self::new(mem, Vec::new());
        *churn.after_read.lock().unwrap() = Some(change);
        churn
    }

    /// A backend whose first read with its tag is preceded by `before` and followed by `after`:
    /// the object read is the one `before` left, and `after` can put back the one that was there
    /// first (a key that holds A, then B, then A again).
    pub fn around_a_tagged_read(mem: Arc<MemoryBackend>, before: Change, after: Change) -> Self {
        let churn = Self::after_a_read(mem, after);
        *churn.before_tagged_read.lock().unwrap() = Some(before);
        churn
    }

    /// Applies the next change to the object under `key`, if one is left.
    async fn interfere(&self, key: &str) {
        if self.only.as_deref().is_some_and(|only| only != key) {
            return;
        }
        let next = self.left.lock().unwrap().pop_front();
        if let Some(change) = next {
            self.apply(key, change).await;
        }
    }

    /// Applies `change` to the object under `key`.
    async fn apply(&self, key: &str, change: Change) {
        let tag = self.mem.head(key).await.map(|meta| meta.tag);
        match (change, tag) {
            (Change::Remove, _) => self.mem.delete(key).await.unwrap(),
            (Change::Object(body), Ok(tag)) => {
                self.mem.put_if_match(key, body, &tag).await.unwrap();
            }
            (Change::Object(body), Err(_)) => self.mem.put(key, body).await.unwrap(),
        }
    }
}

#[async_trait]
impl Backend for Churn {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.mem.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.interfere(key).await;
        self.mem.put_if_match(key, body, etag).await
    }

    /// A streamed replacement, as the replacement of its bytes whole: the change comes just before
    /// the conditional call.
    async fn put_stream_if_match(
        &self,
        key: &str,
        _len: Option<u64>,
        body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        let whole = collect(body).await?;
        self.put_if_match(key, whole, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.mem.get(key, range).await
    }

    async fn get_tagged(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, String), Error> {
        if self.only.as_deref().is_some_and(|only| only != key) {
            return self.mem.get_tagged(key, range).await;
        }
        let before = self.before_tagged_read.lock().unwrap().take();
        if let Some(change) = before {
            self.apply(key, change).await;
        }
        let read = self.mem.get_tagged(key, range).await;
        let change = self.after_read.lock().unwrap().take();
        if let Some(change) = change {
            self.apply(key, change).await;
        }
        read
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.mem.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.mem.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.interfere(key).await;
        self.mem.delete_if_match(key, etag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.mem.list(prefix).await
    }
}

/// A memory backend that fails the `n`th HEAD of one key (counting from one) with `Unavailable`,
/// and, if told to, every `delete_if_match`; everything else is the memory backend's own. It tells
/// the failure of the second look at a key from the failure of the first, which a fault injector
/// that fails the next call of a kind cannot.
pub struct Flaky {
    mem: Arc<MemoryBackend>,
    watched: String,
    fail_head_at: u32,
    fail_conditional_deletes: bool,
    heads: AtomicU32,
}

impl Flaky {
    /// A backend whose `n`th HEAD of `key` fails.
    pub fn head_fails_at(mem: Arc<MemoryBackend>, key: &str, n: u32) -> Self {
        Self {
            mem,
            watched: key.to_owned(),
            fail_head_at: n,
            fail_conditional_deletes: false,
            heads: AtomicU32::new(0),
        }
    }

    /// A backend whose conditional deletes fail.
    pub fn conditional_delete_fails(mem: Arc<MemoryBackend>) -> Self {
        Self {
            fail_conditional_deletes: true,
            ..Self::head_fails_at(mem, "", 0)
        }
    }

    /// The error each failing call gives.
    pub fn error() -> Error {
        Error::Unavailable("flaky".to_owned())
    }
}

#[async_trait]
impl Backend for Flaky {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.mem.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.mem.put_if_match(key, body, etag).await
    }

    async fn put_stream_if_match(
        &self,
        key: &str,
        _len: Option<u64>,
        body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        let whole = collect(body).await?;
        self.mem.put_if_match(key, whole, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.mem.get(key, range).await
    }

    async fn get_tagged(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, String), Error> {
        self.mem.get_tagged(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        if key == self.watched {
            let nth = self.heads.fetch_add(1, Ordering::SeqCst) + 1;
            if nth == self.fail_head_at {
                return Err(Self::error());
            }
        }
        self.mem.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.mem.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        if self.fail_conditional_deletes {
            return Err(Self::error());
        }
        self.mem.delete_if_match(key, etag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.mem.list(prefix).await
    }
}
