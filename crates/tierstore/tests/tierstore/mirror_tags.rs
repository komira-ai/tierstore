//! What a mirror (the journal entry of a copy-back) owns in the hot tier: the hot tier's own tag
//! of the copy, kept apart from the cold tier's tag of the object read, and never compared with
//! it. The two tiers of most tests here tag content alike (both are memory backends); `Prefixed`
//! gives one of them a tag function of its own, as a directory tier (SHA-256) and an S3 tier
//! (ETag) have.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{
    Backend, ByteRange, Error, JournalEntry, JournalOp, Listed, MemoryBackend, Meta, Op, Proxy,
    Reconciler, encode_journal,
};

use crate::support::{Rig, assert_absent, body, journal, journal_key, journal_keys, read};

fn down() -> Error {
    Error::Unavailable("down".to_owned())
}

/// A memory backend whose tags are the tags of the memory backend behind a fixed prefix, so that
/// a tag of this tier is never equal to the tag of a memory backend that holds the same bytes.
struct Prefixed {
    mem: Arc<MemoryBackend>,
    prefix: &'static str,
}

impl Prefixed {
    fn new(mem: Arc<MemoryBackend>, prefix: &'static str) -> Self {
        Self { mem, prefix }
    }

    /// The tag of the memory backend that a tag of this tier stands for, if it is one.
    fn plain<'a>(&self, key: &str, etag: &'a str) -> Result<&'a str, Error> {
        etag.strip_prefix(self.prefix)
            .ok_or_else(|| Error::PreconditionFailed(key.to_owned()))
    }
}

#[async_trait]
impl Backend for Prefixed {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.mem.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let plain = self.plain(key, etag)?;
        let tag = self.mem.put_if_match(key, body, plain).await?;
        Ok(format!("{}{tag}", self.prefix))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.mem.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let mut meta = self.mem.head(key).await?;
        meta.tag = format!("{}{}", self.prefix, meta.tag);
        Ok(meta)
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.mem.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        let plain = self.plain(key, etag)?;
        self.mem.delete_if_match(key, plain).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.mem.list(prefix).await
    }
}

/// The tag that an object of `text` has in a memory backend.
async fn tag_of(text: &str) -> String {
    let scratch = MemoryBackend::new();
    scratch.put("k", body(text)).await.unwrap();
    scratch.head("k").await.unwrap().tag
}

fn mirror(key: &str, cold_tag: Option<&str>, hot_tag: Option<&str>) -> JournalEntry {
    JournalEntry {
        op: JournalOp::PutMirror,
        key: key.to_owned(),
        size: 5,
        time: 0,
        cold_tag: cold_tag.map(str::to_owned),
        hot_tag: hot_tag.map(str::to_owned),
    }
}

/// A rig with proxy `p1` registered and `entries` as its journal object 1.
async fn with_journal(entries: &[JournalEntry]) -> Rig {
    let rig = Rig::new();
    let _proxy = rig.proxy("p1", 1000).await;
    let object = encode_journal(entries);
    let put = rig.hot_mem.put(&journal_key("p1", 1), object);
    put.await.unwrap();
    rig
}

/// Asserts that a hot object that replaced the mirror's copy, whose own journal entry this pass
/// has not read, is not made evictable by a mirror whose cold tag the cold object has: cold holds
/// A and the mirror of `k` names A, but hot holds B under `k`. The hot object of `k` is left, and
/// is not counted as copied, while the hot copy of the other mirror (`j`, which is as it names)
/// is the one eviction removes.
#[tokio::test(start_paused = true)]
async fn a_hot_object_that_replaced_the_mirrored_copy_is_not_evicted() {
    let tag_a = tag_of("aaaaa").await;
    let tag_j = tag_of("jjjjj").await;
    let rig = with_journal(&[
        mirror("k", Some(&tag_a), Some(&tag_a)),
        mirror("j", Some(&tag_j), Some(&tag_j)),
    ])
    .await;
    rig.cold_mem.put("k", body("aaaaa")).await.unwrap();
    rig.cold_mem.put("j", body("jjjjj")).await.unwrap();
    rig.hot_mem.put("k", body("bbbbb")).await.unwrap();
    rig.hot_mem.put("j", body("jjjjj")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 6);

    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_eq!(report.evicted, 1);
    assert_eq!(read(&*rig.hot_mem, "k").await, body("bbbbb"));
    assert_absent(&*rig.hot_mem, "j").await;
    assert_eq!(read(&*rig.cold_mem, "k").await, body("aaaaa"));
}

/// Asserts that a mirror entry with no hot tag (one a release wrote before the field existed)
/// owns no hot object, for each state of the cold tier: with the cold object it names, with
/// another one, and with none, the hot copy stays and nothing is counted as copied or evicted
/// even with the hot tier over its water mark.
#[tokio::test(start_paused = true)]
async fn a_mirror_entry_without_a_hot_tag_leaves_the_hot_copy_and_evicts_nothing() {
    let tag = tag_of("hot!!").await;
    for cold in [Some("hot!!"), Some("other"), None] {
        let rig = with_journal(&[mirror("k", Some(&tag), None)]).await;
        rig.hot_mem.put("k", body("hot!!")).await.unwrap();
        if let Some(cold) = cold {
            rig.cold_mem.put("k", body(cold)).await.unwrap();
        }
        let mut reconciler = rig.reconciler("r1", 5);

        let report = reconciler.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new());
        assert_eq!(report.copied, 0);
        assert_eq!(report.evicted, 0);
        assert_eq!(read(&*rig.hot_mem, "k").await, body("hot!!"));
    }
}

/// Asserts that a copy-back journals the tag the hot tier gives the copy next to the tag of the
/// cold object, and that reconciling a delete removes the copy when the tiers tag the same bytes
/// differently: the cold tier deletes the object after the copy-back, and the pass deletes the hot
/// copy by the hot tier's tag (the cold tag, given to the hot tier, would not match).
#[tokio::test(start_paused = true)]
async fn a_deleted_object_has_its_hot_copy_removed_when_the_tiers_tag_alike_bytes_differently() {
    let rig = Rig::new();
    let hot = Arc::new(Prefixed::new(rig.hot_mem.clone(), "hot-"));
    rig.cold_mem.put("k", body("value")).await.unwrap();
    let cold_tag = rig.cold_mem.head("k").await.unwrap().tag;
    let config = rig.config("p1", 1000);
    let proxy = Proxy::open(hot.clone(), rig.cold.clone(), config)
        .await
        .unwrap();

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].cold_tag.as_deref(), Some(cold_tag.as_str()));
    let hot_tag = format!("hot-{cold_tag}");
    assert_eq!(entries[0].hot_tag.as_deref(), Some(hot_tag.as_str()));

    rig.cold_mem.delete("k").await.unwrap();
    let config = rig.reconciler_config("r1", 1000);
    let mut reconciler = Reconciler::new(hot.clone(), rig.cold.clone(), config);
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 0);
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts that a copy-back whose object the cold tier still holds is an eviction candidate when
/// the tiers tag the bytes differently, and eviction deletes it by the hot tier's tag: the hot
/// copy goes and the cold object stays.
#[tokio::test(start_paused = true)]
async fn a_copy_back_is_evicted_by_the_hot_tag_when_the_tiers_tag_alike_bytes_differently() {
    let rig = Rig::new();
    let hot = Arc::new(Prefixed::new(rig.hot_mem.clone(), "hot-"));
    rig.cold_mem.put("k", body("value")).await.unwrap();
    let config = rig.config("p1", 1000);
    let proxy = Proxy::open(hot.clone(), rig.cold.clone(), config)
        .await
        .unwrap();
    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;

    let config = rig.reconciler_config("r1", 5);
    let mut reconciler = Reconciler::new(hot.clone(), rig.cold.clone(), config);
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_eq!(report.evicted, 1);
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(read(&*rig.cold_mem, "k").await, body("value"));
}

/// Asserts that a copy-back whose hot tag cannot be read (the HEAD of the copy fails) journals
/// nothing and takes the copy out of the hot tier, which nothing would own, while the read itself
/// is answered.
#[tokio::test(start_paused = true)]
async fn a_copy_back_whose_hot_tag_cannot_be_read_is_taken_out_again() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();
    rig.hot.fail_next_on(Op::Head, "k", down());

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_absent(&*rig.hot_mem, "k").await;
    let keys = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(keys, Vec::<String>::new());
}
