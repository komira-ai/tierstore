//! These tests assert the intended behaviour in edge cases of the proxy and the reconciler:
//! interleavings of a delete, a put and a copy to the cold tier that the documentation listed as
//! known limits. Each holds the interleaving at a point it names with a gate, not with a delay.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{Backend, ByteRange, Error, JournalOp, Listed, Meta, PartBody, Reconciler};
use tokio::sync::Notify;

use crate::hold::Stall;
use crate::support::{Rig, assert_absent, body, chunks, journal, journal_keys, read, set_usage};

/// Asserts that a key deleted and put again with new content inside one journal object, while
/// the cold copy of the older content is still in flight, reads as the new content once the hot
/// tier has evicted it: after the pass that applies that journal object, the cold tier holds the
/// new object (or the hot tier still holds it), not neither.
///
/// Catches a pass that counts the stale cold copy of the old content as the copy of the new
/// object (create-only AlreadyExists) and then deletes that copy as stale, so the eviction
/// leaves the key in neither tier.
#[tokio::test(start_paused = true)]
async fn a_delete_then_a_reput_in_one_journal_object_keeps_the_new_object() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("old")).await.unwrap();
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let slow: Arc<dyn Backend> = Arc::new(Stall::new(
        rig.cold.clone(),
        "k",
        entered.clone(),
        Some(release.clone()),
    ));
    let mut reconciler = Reconciler::new(rig.hot.clone(), slow, rig.reconciler_config("r1", 100));

    // Journal object 1 holds the put of the old content; its copy waits in the cold tier while
    // the proxy deletes k and puts it again, both in journal object 2.
    let replace = async {
        entered.notified().await;
        let (deleted, put) = tokio::join!(proxy.delete("k"), proxy.put("k", body("new")));
        deleted.unwrap();
        put.unwrap();
        release.notify_one();
    };
    let (report, ()) = tokio::join!(reconciler.pass(), replace);
    assert_eq!(report.failed, Vec::<Error>::new());
    // The old copy landed in the cold tier after the delete, and the copy that wrote it found
    // the hot tier holding another object, took it back, and copied the new one.
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new"));
    assert_eq!(journal_keys(&*rig.hot_mem, "p1").await.len(), 2);
    let entries = journal(&*rig.hot_mem, "p1", 2).await;
    let shape: Vec<(JournalOp, String)> = entries.into_iter().map(|e| (e.op, e.key)).collect();
    assert_eq!(
        shape,
        [
            (JournalOp::Delete, "k".to_owned()),
            (JournalOp::Put, "k".to_owned())
        ]
    );

    // The next pass applies journal object 2.
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());

    // Fill the hot tier past the high water so the pass evicts, oldest first: k goes first.
    let filler = "f".repeat(30);
    for key in ["f1", "f2", "f3"] {
        rig.clock.advance(10);
        proxy.put(key, body(&filler)).await.unwrap();
    }
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert!(report.evicted >= 1);
    assert_absent(&*rig.hot_mem, "k").await;

    assert_eq!(proxy.get("k", None).await, Ok(body("new")));
}

/// Asserts that an object put again after a delete, by a put that went straight to the cold
/// tier because the hot tier was full (so no journal entry says so), is still there, and still
/// has its new content, after the reconciler applies the journaled delete of the same key.
/// Catches a delete entry that is applied to the cold tier without telling an object made
/// after it from the object it deleted, so a key the client wrote and was told succeeded is
/// lost.
#[tokio::test(start_paused = true)]
async fn a_delete_then_a_cold_path_reput_keeps_the_new_object_after_the_reconciler_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("old")).await.unwrap();
    proxy.delete("k").await.unwrap();
    rig.clock.advance(10);

    set_usage(&rig, 1000, rig.clock.now()).await;
    proxy.put("k", body("new")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new"));

    let mut reconciler = rig.reconciler("r1", 1000);
    let report = reconciler.pass().await;
    assert!(report.held);
    assert_eq!(report.failed, Vec::<Error>::new());

    assert_eq!(proxy.get("k", None).await, Ok(body("new")));
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new"));
}

/// Asserts that an object created again after a delete by a multipart completion (which goes
/// to the cold tier and writes no journal entry) is still there, with its new content, after
/// the reconciler applies the journaled delete of the same key.
/// Catches a delete entry that is applied to the cold tier without telling an object made
/// after it from the object it deleted, so a completed multipart upload is lost.
#[tokio::test(start_paused = true)]
async fn a_delete_then_a_multipart_reput_keeps_the_new_object_after_the_reconciler_pass() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    proxy.put("k", body("old")).await.unwrap();
    proxy.delete("k").await.unwrap();
    rig.clock.advance(10);

    let id = proxy.create_upload("k").await.unwrap();
    let tag = proxy
        .upload_part("k", &id, 1, chunks(&["new"]))
        .await
        .unwrap();
    proxy.complete_upload("k", &id, &[(1, tag)]).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new"));

    let mut reconciler = rig.reconciler("r1", 1 << 20);
    let report = reconciler.pass().await;
    assert!(report.held);
    assert_eq!(report.failed, Vec::<Error>::new());

    assert_eq!(proxy.get("k", None).await, Ok(body("new")));
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new"));
}

/// Asserts that a key deleted and put again, whose delete and new put reach one journal object
/// while the cold tier tags content by another function than the hot tier, keeps its new object:
/// after the reconciler applies the journal and evicts the key from the hot tier, a get returns
/// the new content.
///
/// Catches a delete entry that compares content tags across tiers and deletes the cold copy of
/// the live object.
#[tokio::test(start_paused = true)]
async fn a_delete_and_a_reput_in_one_journal_object_keeps_the_new_object_when_the_tiers_tag_differently()
 {
    let rig = Rig::new();
    let mut config = rig.config("p1", 10);
    config.batch_entries = 2;
    let proxy = rig.open(config).await.unwrap();
    let cold: Arc<dyn Backend> = Arc::new(Retagged {
        inner: rig.cold.clone(),
    });
    let mut reconciler_over_retagged =
        Reconciler::new(rig.hot.clone(), cold, rig.reconciler_config("r1", 10));

    proxy.put("k", body("old")).await.unwrap();
    let report = reconciler_over_retagged.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(rig.cold_mem.get("k", None).await, Ok(body("old")));

    rig.clock.advance(10);
    let newer = "new-bytes";
    let (deleted, put) = tokio::join!(proxy.delete("k"), proxy.put("k", body(newer)));
    deleted.unwrap();
    put.unwrap();

    for _ in 0..2 {
        let report = reconciler_over_retagged.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new());
    }

    assert_eq!(rig.cold_mem.get("k", None).await, Ok(body(newer)));
    assert_eq!(proxy.get("k", None).await, Ok(body(newer)));
}

/// A backend over another whose tags are a different function of the content than the inner
/// backend's (the way an S3 ETag differs from a SHA-256): every tag it reports is prefixed.
struct Retagged {
    inner: Arc<dyn Backend>,
}

#[async_trait]
impl Backend for Retagged {
    fn needs_length(&self) -> bool {
        self.inner.needs_length()
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_stream(key, len, body).await
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_stream_hot_only(key, len, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let mut meta = self.inner.head(key).await?;
        meta.tag = format!("cold-etag:{}", meta.tag);
        Ok(meta)
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        self.inner.list_page(prefix, start_after, max_keys).await
    }
}
