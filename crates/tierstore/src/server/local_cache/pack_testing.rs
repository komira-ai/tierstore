//! Fixtures of the tests of the fill socket: an upstream object made of blobs, and the questions a
//! test asks of a cache after the fills.

use super::LocalCache;
use super::layout::name_of;
use super::testing::{Rig, digest};

/// `len` bytes that differ from the bytes of any other `seed`.
pub(in crate::server) fn distinct(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 7 + seed * 31 + 3) % 251) as u8)
        .collect()
}

/// An upstream object made of blobs, one after the other: what the fills of the fill socket read
/// from.
pub(in crate::server) struct Pack {
    pub(in crate::server) key: String,
    /// The digest, offset and size of each blob.
    pub(in crate::server) blobs: Vec<(String, u64, u64)>,
    bytes: Vec<u8>,
}

impl Pack {
    /// The pack at `key` of blobs of `sizes`.
    pub(in crate::server) fn new(key: &str, sizes: &[usize]) -> Self {
        let mut bytes = Vec::new();
        let mut blobs = Vec::new();
        for (seed, size) in sizes.iter().enumerate() {
            let blob = distinct(seed, *size);
            blobs.push((digest(&blob), bytes.len() as u64, *size as u64));
            bytes.extend_from_slice(&blob);
        }
        Self {
            key: key.to_owned(),
            blobs,
            bytes,
        }
    }

    /// The pack at `key` of blobs of `sizes`, put in the upstream of `rig`.
    pub(in crate::server) async fn put(rig: &Rig, key: &str, sizes: &[usize]) -> Self {
        let pack = Self::new(key, sizes);
        rig.put(key, &pack.bytes).await;
        pack
    }

    /// The bytes of the whole pack.
    pub(in crate::server) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The blob `i` as the cache is asked for it.
    pub(in crate::server) fn blob(&self, i: usize) -> super::Blob<'_> {
        let (sha256, offset, size) = &self.blobs[i];
        super::Blob {
            sha256,
            size: *size,
            from: &self.key,
            offset: *offset,
        }
    }

    /// The bytes of the blob `i`.
    pub(in crate::server) fn content(&self, i: usize) -> &[u8] {
        let (_, offset, size) = &self.blobs[i];
        &self.bytes[*offset as usize..(*offset + *size) as usize]
    }
}

/// The fills `cache` has made.
pub(in crate::server) fn fills(cache: &LocalCache) -> u64 {
    super::metrics::read(&cache.counters().fills)
}

/// The names of the objects of `cache` in the order of their use, the least recently used first.
pub(in crate::server) fn recency(cache: &LocalCache) -> Vec<String> {
    let index = super::locked(&cache.inner.index);
    index
        .oldest_first()
        .into_iter()
        .map(|(hex, _)| hex)
        .collect()
}

/// The name on disk of the blob `sha256` of the fill socket.
pub(in crate::server) fn blob_hex(sha256: &str) -> String {
    name_of(&format!("_tiering/fill/{sha256}")).hex
}
