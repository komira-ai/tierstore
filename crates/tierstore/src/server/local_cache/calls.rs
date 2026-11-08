//! The cache's reads of its upstream, each counted where it is sent.
//!
//! Every GET and every HEAD the cache sends the upstream goes through one function here, so that
//! `tierstore_cache_upstream_reads_total{op}` counts each call once and
//! `tierstore_cache_upstream_bytes_read_total` the bytes the GETs that returned brought back. The
//! listings are counted where they are sent (`listing`), one for each page. The writes are
//! counted by `keep` and `write`, by what the upstream answered.

use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::StreamExt as _;

use crate::{ByteRange, Error, Meta, PartBody};

use super::Inner;
use super::metrics::{Call, add};
use super::stream::read_within;

impl Inner {
    /// A HEAD of `key` at the upstream, counted.
    pub(super) async fn upstream_head(&self, key: &str) -> Result<Meta, Error> {
        self.counters.called(Call::Head);
        self.upstream.head(key).await
    }

    /// A GET of `range` of `key` at the upstream, held whole, counted: the call, and the bytes
    /// once it has returned them all.
    pub(super) async fn upstream_get(&self, key: &str, range: ByteRange) -> Result<Bytes, Error> {
        self.counters.called(Call::Get);
        let bytes = self.upstream.get(key, Some(range)).await?;
        add(&self.counters.upstream_bytes_read, bytes.len() as u64);
        Ok(bytes)
    }

    /// A GET of `range` of `key` at the upstream as a stream, read to its end, of at most `want`
    /// bytes (see [`read_within`]), counted as [`upstream_get`](Self::upstream_get) is: a body
    /// that fails part way counts the call and none of its bytes.
    pub(super) async fn upstream_get_within(
        &self,
        key: &str,
        range: ByteRange,
        want: u64,
    ) -> Result<Bytes, Error> {
        self.counters.called(Call::Get);
        let body = self.upstream.get_stream(key, Some(range)).await?;
        let bytes = read_within(body, want).await?;
        add(&self.counters.upstream_bytes_read, bytes.len() as u64);
        Ok(bytes)
    }
}

/// `body`, passed on as it is, with the bytes of each chunk that passes added to `passed`: what
/// the upstream was given of a put or a part.
pub(super) fn counting<'a>(body: PartBody<'a>, passed: &'a AtomicU64) -> PartBody<'a> {
    body.inspect(move |chunk| {
        if let Ok(chunk) = chunk {
            let _ = passed.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    })
    .boxed()
}
