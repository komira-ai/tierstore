//! A memory backend whose streamed reads come in chunks of a size a test names, so that two tiers
//! that hold the same bytes can be read in different pieces, and a read can end short or fail.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use tierstore::{Backend, ByteRange, Error, Listed, MemoryBackend, Meta, ReadBody};

/// How a streamed read of [`Chunked`] ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// With the last byte.
    Whole,
    /// One byte early, as a read of an object that was replaced by a shorter one.
    Short,
    /// With an error after the first chunk.
    Broken,
}

/// A memory backend (everything else is its own) whose streamed read gives the object in chunks
/// of `size` bytes with an empty chunk before each, and ends as `ending` says.
pub struct Chunked {
    mem: Arc<MemoryBackend>,
    size: usize,
    ending: Ending,
}

impl Chunked {
    pub fn new(mem: Arc<MemoryBackend>, size: usize, ending: Ending) -> Self {
        Self { mem, size, ending }
    }
}

#[async_trait]
impl Backend for Chunked {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.mem.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.mem.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.mem.get(key, range).await
    }

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        let mut whole = self.mem.get(key, range).await?;
        if self.ending == Ending::Short {
            whole.truncate(whole.len().saturating_sub(1));
        }
        let mut items: Vec<Result<Bytes, Error>> = Vec::new();
        while !whole.is_empty() {
            let chunk = whole.split_to(self.size.min(whole.len()));
            items.push(Ok(Bytes::new()));
            items.push(Ok(chunk));
        }
        if self.ending == Ending::Broken {
            items.truncate(2);
            items.push(Err(Error::Unavailable("read broke".to_owned())));
        }
        Ok(futures::stream::iter(items).boxed())
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.mem.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.mem.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.mem.list(prefix).await
    }
}
