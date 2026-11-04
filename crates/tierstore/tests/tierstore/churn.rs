//! A backend that changes what a tier holds at a point a test names: in a listing.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use tierstore::{Backend, ByteRange, Error, Listed, MemoryBackend, Meta};

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
