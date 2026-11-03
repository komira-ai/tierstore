//! The reconciler where its copies meet other writes: that a copy to the cold tier passes the hot
//! tier's object on as the chunks it is read in and never holds it whole, and that a mirror entry
//! that a later put or delete of its key supersedes does not touch the newer object.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use tierstore::{
    Backend, ByteRange, Error, JournalEntry, JournalOp, Listed, MemoryBackend, Meta, PartBody,
    ReadBody, encode_journal,
};

use crate::conformance::reservation;
use crate::support::{Rig, body, journal_key, read};

fn entry(op: JournalOp, key: &str, size: u64, tag: Option<&str>) -> JournalEntry {
    JournalEntry {
        op,
        key: key.to_owned(),
        size,
        time: 0,
        cold_tag: tag.map(str::to_owned),
    }
}

/// Writes `entries` as journal object `seq` of proxy `p1`.
async fn add_journal(rig: &Rig, seq: u64, entries: &[JournalEntry]) {
    let object = encode_journal(entries);
    rig.hot_mem
        .put(&journal_key("p1", seq), object)
        .await
        .unwrap();
}

/// A hot tier that cannot give an object of the data (a key that does not start with `_`) whole:
/// a read of the bytes is an error, and a streamed read gives three chunks.
struct Chunked {
    mem: Arc<MemoryBackend>,
}

fn is_data(key: &str) -> bool {
    !key.starts_with('_')
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
        if is_data(key) {
            return Err(Error::Other(format!("{key} was read whole")));
        }
        self.mem.get(key, range).await
    }

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        let whole = self.mem.get(key, range).await?;
        if !is_data(key) {
            return Ok(futures::stream::once(std::future::ready(Ok(whole))).boxed());
        }
        let pieces: Vec<Result<Bytes, Error>> = whole
            .chunks(whole.len().div_ceil(3))
            .map(|piece| Ok(Bytes::copy_from_slice(piece)))
            .collect();
        Ok(futures::stream::iter(pieces).boxed())
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

/// What a streamed write of the cold tier was given: the call, the length it was told, and the
/// number of chunks of the body.
type Seen = (&'static str, Option<u64>, usize);

/// A cold tier that refuses a write of bytes held whole (`put`, `put_if_match`) and takes the
/// streamed ones, noting what each was given.
struct Streamed {
    mem: Arc<MemoryBackend>,
    seen: Mutex<Vec<Seen>>,
}

impl Streamed {
    fn seen(&self) -> Vec<Seen> {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        seen.sort_unstable();
        seen
    }

    /// The body joined, and the number of its chunks.
    async fn take(mut body: PartBody<'_>) -> Result<(Bytes, usize), Error> {
        let mut whole = Vec::new();
        let mut chunks = 0;
        while let Some(chunk) = body.next().await {
            whole.extend_from_slice(&chunk?);
            chunks += 1;
        }
        Ok((Bytes::from(whole), chunks))
    }

    fn note(&self, call: &'static str, len: Option<u64>, chunks: usize) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.push((call, len, chunks));
    }
}

#[async_trait]
impl Backend for Streamed {
    async fn put(&self, key: &str, _body: Bytes) -> Result<(), Error> {
        Err(Error::Other(format!("{key} was written whole")))
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        let (whole, chunks) = Self::take(body).await?;
        self.note("put_stream", len, chunks);
        self.mem.put(key, whole).await
    }

    async fn put_if_match(&self, key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::Other(format!("{key} was replaced whole")))
    }

    async fn put_stream_if_match(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        let (whole, chunks) = Self::take(body).await?;
        self.note("put_stream_if_match", len, chunks);
        self.mem.put_if_match(key, whole, etag).await
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

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.mem.delete_if_match(key, etag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.mem.list(prefix).await
    }
}

/// Asserts that the reconciler gives the cold tier the hot tier's object as a stream and never
/// holds it whole: with a hot tier that refuses to give the bytes of an object whole (it gives
/// three chunks) and a cold tier that refuses to take bytes held whole, a reservation named by its
/// entry is replaced by the object (a streamed conditional write, told the length of the object,
/// given the three chunks), and a plain put to an empty cold tier is copied by a streamed create
/// (the same), and the reconciler counts both copied.
#[tokio::test(start_paused = true)]
async fn the_hot_object_goes_to_the_cold_tier_as_a_stream_whether_it_replaces_or_creates() {
    let rig = Rig::new();
    let _proxy = rig.proxy("p1", 1000).await;
    let tag = rig
        .cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    add_journal(
        &rig,
        1,
        &[
            entry(JournalOp::PutReserved, "k", 9, Some(&tag)),
            entry(JournalOp::Put, "j", 9, None),
        ],
    )
    .await;
    rig.hot_mem.put("k", body("abcdefghi")).await.unwrap();
    rig.hot_mem.put("j", body("jklmnopqr")).await.unwrap();
    let hot = Arc::new(Chunked {
        mem: rig.hot_mem.clone(),
    });
    let cold = Arc::new(Streamed {
        mem: rig.cold_mem.clone(),
        seen: Mutex::new(Vec::new()),
    });
    let config = rig.reconciler_config("r1", 1000);
    let mut reconciler = tierstore::Reconciler::new(hot, cold.clone(), config);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 2);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("abcdefghi"));
    assert!(!rig.cold_mem.head("k").await.unwrap().reservation);
    assert_eq!(read(&*rig.cold_mem, "j").await, body("jklmnopqr"));
    let streamed = [
        ("put_stream", Some(9), 3),
        ("put_stream_if_match", Some(9), 3),
    ];
    assert_eq!(cold.seen(), streamed);
}

/// Asserts that a mirror entry that a later put of its key supersedes does not remove the newer
/// object: the journal objects are a mirror of `k` (a copy-back), a delete of `k`, and a put of
/// `k`, read together by one pass over a cold tier that holds no `k` (the delete took it), and the
/// hot tier holds the put's object. The old mirror is skipped, the put is copied to the cold tier,
/// and the hot object is still there.
#[tokio::test(start_paused = true)]
async fn a_mirror_that_a_delete_and_a_later_put_supersede_leaves_the_newer_object() {
    let rig = Rig::new();
    let _proxy = rig.proxy("p1", 1000).await;
    add_journal(
        &rig,
        1,
        &[entry(JournalOp::PutMirror, "k", 5, Some("old-tag"))],
    )
    .await;
    add_journal(&rig, 2, &[entry(JournalOp::Delete, "k", 0, None)]).await;
    add_journal(&rig, 3, &[entry(JournalOp::Put, "k", 5, None)]).await;
    rig.hot_mem.put("k", body("new!!")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.hot_mem, "k").await, body("new!!"));
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new!!"));
    assert_eq!(report.copied, 1);
}
