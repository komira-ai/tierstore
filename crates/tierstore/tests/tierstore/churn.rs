//! Backends that change what a tier holds at a point a test names: between a look at an object
//! and the conditional call that follows it, or in a listing.

use std::collections::VecDeque;
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
    /// Replaces it with a real object of these bytes.
    Object(Bytes),
    /// Removes it.
    Remove,
}

/// A memory backend whose `put_if_match` and `delete_if_match` each first apply the next of the
/// changes it was given (if any are left) to the object under the key, so that the call finds
/// what a racing writer left, as it would between the look that gave the caller the tag and the
/// call; and, if asked to, whose first read with the mark is preceded and followed by a change,
/// so that the read finds, and a look after it finds, what a racing writer left, or whose first
/// plain read is preceded by one. Everything else is the memory backend's
/// own.
pub struct Churn {
    mem: Arc<MemoryBackend>,
    left: Mutex<VecDeque<Change>>,
    after_read: Mutex<Option<Change>>,
    before_read: Mutex<Option<Change>>,
    before_marked_read: Mutex<Option<Change>>,
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
            before_read: Mutex::new(None),
            before_marked_read: Mutex::new(None),
            only: None,
        }
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

    async fn put_reservation(&self, key: &str, body: Bytes) -> Result<String, Error> {
        self.mem.put_reservation(key, body).await
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

    async fn put_reservation_if_match(
        &self,
        key: &str,
        body: Bytes,
        etag: &str,
    ) -> Result<String, Error> {
        self.mem.put_reservation_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        if self.only.as_deref().is_none_or(|only| only == key) {
            let change = self.before_read.lock().unwrap().take();
            if let Some(change) = change {
                self.apply(key, change).await;
            }
        }
        self.mem.get(key, range).await
    }

    async fn get_tagged(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, bool, String), Error> {
        if self.only.as_deref().is_some_and(|only| only != key) {
            return self.mem.get_tagged(key, range).await;
        }
        let before = self.before_marked_read.lock().unwrap().take();
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

/// A memory backend whose listings also show one object that is not there: an object that was
/// deleted after the listing was cut, with the size of a reservation. A HEAD of it is not found.
pub struct Ghost {
    mem: Arc<MemoryBackend>,
    key: String,
}

impl Ghost {
    pub fn new(mem: Arc<MemoryBackend>, key: &str) -> Self {
        Self {
            mem,
            key: key.to_owned(),
        }
    }
}

#[async_trait]
impl Backend for Ghost {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.mem.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.mem.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.mem.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.mem.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.mem.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let mut listed = self.mem.list(prefix).await?;
        if self.key.starts_with(prefix) {
            let ghost = Listed {
                key: self.key.clone(),
                size: 56,
                modified: 0,
            };
            listed.push(ghost);
            listed.sort();
        }
        Ok(listed)
    }
}
