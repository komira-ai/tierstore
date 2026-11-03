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
    Backend, ByteRange, Error, FaultyBackend, JournalEntry, JournalOp, Listed, MemoryBackend, Meta,
    Op, Proxy, Reconciler, encode_journal,
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
    let key = journal_key("p1", 1);
    rig.hot_mem.put(&key, object).await.unwrap();
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

/// Asserts that a copy-back journals the tag its create-only write gave the copy, and makes no
/// HEAD of the hot tier to find it: the entry's hot tag is the tag of the bytes written.
#[tokio::test(start_paused = true)]
async fn a_copy_back_takes_the_hot_tag_from_its_own_create_and_makes_no_head() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();
    let heads = rig.hot.calls(Op::Head);

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].hot_tag, Some(tag_of("value").await));
    assert_eq!(rig.hot.calls(Op::Head), heads);
}

/// Asserts that a copy-back whose create-only write fails (the hot tier is down for the write)
/// answers the read from the cold tier, journals nothing and deletes nothing: there is no tag to
/// delete by.
#[tokio::test(start_paused = true)]
async fn a_copy_back_whose_create_fails_journals_and_deletes_nothing() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();
    rig.hot.fail_next_on(Op::Put, "k", down());

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_absent(&*rig.hot_mem, "k").await;
    let keys = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(keys, Vec::<String>::new());
    assert_eq!(rig.hot.calls(Op::Delete), 0);
}

/// A hot tier that, when the create-only write of `key` returns, has already had the object
/// replaced by `replacement` (as another proxy would, between that write and the journal entry
/// that follows it), and still gives the tag of the object the write created.
struct Replacing {
    inner: Arc<FaultyBackend>,
    mem: Arc<MemoryBackend>,
    key: &'static str,
    replacement: &'static str,
}

#[async_trait]
impl Backend for Replacing {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        let tag = self.inner.put_tagged(key, body).await?;
        if key == self.key {
            self.mem.delete(key).await?;
            let newer = crate::support::body(self.replacement);
            self.mem.put(key, newer).await?;
        }
        Ok(tag)
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

/// A rig and a proxy `p1` whose hot tier is a [`Replacing`] that replaces `key` with
/// `replacement` right after the copy-back's create.
async fn replaced_after_creation(key: &'static str, replacement: &'static str) -> (Rig, Proxy) {
    let rig = Rig::new();
    let hot = Arc::new(Replacing {
        inner: rig.hot.clone(),
        mem: rig.hot_mem.clone(),
        key,
        replacement,
    });
    let config = rig.config("p1", 1000);
    let proxy = Proxy::open(hot, rig.cold.clone(), config).await.unwrap();
    (rig, proxy)
}

/// Asserts, with the hot object of `k` replaced by another proxy's bytes right after the
/// copy-back created it and before its entry was journaled, that the replacement is never
/// deleted by the mirror, whatever the cold tier holds when the reconciler judges it (the object
/// gone, another object, the object the mirror names; the hot tier over its water mark): the
/// entry's hot tag is the tag of the created copy and does not match the replacement.
#[tokio::test(start_paused = true)]
async fn an_object_that_replaced_the_copy_before_its_entry_was_journaled_is_never_deleted() {
    for cold in [None, Some("other"), Some("value")] {
        let (rig, proxy) = replaced_after_creation("k", "newer").await;
        rig.cold_mem.put("k", body("value")).await.unwrap();
        assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        let entries = journal(&*rig.hot_mem, "p1", 1).await;
        assert_eq!(entries[0].hot_tag, Some(tag_of("value").await));
        assert_eq!(read(&*rig.hot_mem, "k").await, body("newer"));

        rig.cold_mem.delete("k").await.unwrap();
        if let Some(cold) = cold {
            rig.cold_mem.put("k", body(cold)).await.unwrap();
        }
        let mut reconciler = rig.reconciler("r1", 5);
        let report = reconciler.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new());
        assert_eq!(report.copied, 0);
        assert_eq!(report.evicted, 0);
        assert_eq!(read(&*rig.hot_mem, "k").await, body("newer"));
    }
}

/// Asserts that when the journal entry of a copy-back cannot be written, the copy is taken out of
/// the hot tier by its tag: an untouched copy goes, and an object that replaced it right after
/// the create stays.
#[tokio::test(start_paused = true)]
async fn a_failed_journal_entry_removes_the_copy_by_its_tag_and_leaves_a_replacement() {
    for (replaced_key, kept) in [("nothing", None), ("k", Some("newer"))] {
        let (rig, proxy) = replaced_after_creation(replaced_key, "newer").await;
        rig.cold_mem.put("k", body("value")).await.unwrap();
        rig.hot.fail_next_on(Op::Put, "_journal/", down());

        assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        let keys = journal_keys(&*rig.hot_mem, "p1").await;
        assert_eq!(keys, Vec::<String>::new());
        match kept {
            Some(text) => assert_eq!(read(&*rig.hot_mem, "k").await, body(text)),
            None => assert_absent(&*rig.hot_mem, "k").await,
        }
    }
}

/// Asserts that a mirror that names no hot tag stays unowned when a reconciler rebuilds its books
/// from the stored checkpoint: a first reconciler applies journal object 1 (a plain put of `j`,
/// then the tagless mirror of `k`), stores its checkpoint, which keeps the cursor at `j`, and
/// stops; a second one resumes from it and reads the object again, as applied, under eviction
/// pressure. It evicts `j` and `m` (a mirror that names its hot tag, which stays a candidate) and
/// leaves the hot object of `k`: the tagless mirror is not an eviction candidate, and no current
/// tag of the hot tier stands in for the one it lacks.
#[tokio::test(start_paused = true)]
async fn a_mirror_without_a_hot_tag_stays_unowned_when_the_books_are_rebuilt_from_a_checkpoint() {
    let tag = tag_of("hot!!").await;
    let plain = JournalEntry {
        op: JournalOp::Put,
        key: "j".to_owned(),
        size: 5,
        time: 0,
        cold_tag: None,
        hot_tag: None,
    };
    let tag_m = tag_of("mmmmm").await;
    let entries = [
        plain,
        mirror("k", Some(&tag), None),
        mirror("m", Some(&tag_m), Some(&tag_m)),
    ];
    let rig = with_journal(&entries).await;
    for (key, text) in [("j", "jjjjj"), ("k", "hot!!"), ("m", "mmmmm")] {
        rig.hot_mem.put(key, body(text)).await.unwrap();
        rig.cold_mem.put(key, body(text)).await.unwrap();
    }

    let mut first = rig.reconciler("r1", 1000);
    let report = first.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 2);
    assert_eq!(report.evicted, 0);
    first.stop().await;

    let mut second = rig.reconciler("r2", 5);
    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "j").await;
    assert_absent(&*rig.hot_mem, "m").await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body("hot!!"));
}
