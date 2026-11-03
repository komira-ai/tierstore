//! A memory backend whose reads with the mark (the comparison of a takeover reads both tiers so)
//! can end short, end late or fail, and are counted, so that a test can tell how far a comparison
//! read and can make two tiers that hold the same bytes disagree on a read.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{Backend, ByteRange, Error, Listed, MemoryBackend, Meta};

/// How a read with the mark of [`MarkedReads`] ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// With the last byte.
    Whole,
    /// One byte early, as a read of an object that was replaced by a shorter one.
    Short,
    /// One byte late, as a read of an object that was replaced by a longer one.
    Long,
    /// With an error.
    Broken,
}

/// A memory backend (everything else is its own) whose reads with the mark end as `ending` says.
pub struct MarkedReads {
    mem: Arc<MemoryBackend>,
    ending: Ending,
    reads: Arc<AtomicUsize>,
}

impl MarkedReads {
    pub fn new(mem: Arc<MemoryBackend>, ending: Ending) -> Self {
        Self {
            mem,
            ending,
            reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// How many reads with the mark this backend has served so far.
    pub fn reads(&self) -> Arc<AtomicUsize> {
        self.reads.clone()
    }
}

#[async_trait]
impl Backend for MarkedReads {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.mem.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.mem.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.mem.get(key, range).await
    }

    async fn get_marked(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, bool, String), Error> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let (mut bytes, reservation, tag) = self.mem.get_marked(key, range).await?;
        match self.ending {
            Ending::Short => bytes.truncate(bytes.len().saturating_sub(1)),
            Ending::Long => bytes = Bytes::from([bytes.as_ref(), b"z"].concat()),
            Ending::Broken => return Err(Error::Unavailable("read broke".to_owned())),
            Ending::Whole => {}
        }
        Ok((bytes, reservation, tag))
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.mem.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.mem.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.mem.delete_if_match(key, etag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.mem.list(prefix).await
    }
}
