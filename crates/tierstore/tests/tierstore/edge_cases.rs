//! These tests assert the intended behaviour in edge cases of the proxy and the reconciler:
//! interleavings of a delete, a put, a copy-back, an eviction and a lease takeover that the
//! documentation lists as known limits or does not mention. Each holds the interleaving at a
//! point it names with a gate, not with a delay. Tests that fail on the current code say what
//! they assert; they land with the change that makes them pass.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{Backend, ByteRange, Error, JournalOp, Listed, Meta, PartBody, Proxy, Reconciler};
use tokio::sync::Notify;

use crate::hold::Stall;
use crate::support::{
    Rig, assert_absent, body, chunks, journal, journal_keys, read, set_usage, text,
};

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
    // The old copy landed in the cold tier after the delete.
    assert_eq!(read(&*rig.cold_mem, "k").await, body("old"));
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

/// Asserts that a key deleted while a get of it is copying it back from the cold tier into the
/// hot tier stays deleted: once the copy-back has written the key to the hot tier and journaled
/// it, and the reconciler has run, a get of the key is NotFound and neither tier holds it.
///
/// Catches a copy-back that brings a deleted object back: its put entry is journaled after the
/// delete entry, so the reconciler copies the stale hot object to the cold tier.
#[tokio::test(start_paused = true)]
async fn a_delete_during_a_copy_back_keeps_the_key_deleted() {
    let rig = Rig::new();
    rig.cold_mem.put("k", body("value")).await.unwrap();

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let stalled_hot = Arc::new(Stall::new(
        rig.hot.clone(),
        "k",
        entered.clone(),
        Some(release.clone()),
    ));
    let proxy = Arc::new(
        Proxy::open(stalled_hot, rig.cold.clone(), rig.config("p1", 1000))
            .await
            .unwrap(),
    );

    // The get misses hot, reads cold, and stops at the copy-back's hot put.
    let getter = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.get("k", None).await })
    };
    entered.notified().await;

    // The delete completes in full: both tiers, and its journal entry.
    proxy.delete("k").await.unwrap();
    rig.clock.advance(10);
    assert_absent(&*rig.cold_mem, "k").await;

    // The copy-back goes on: the hot tier takes the object, and the put is journaled.
    release.notify_one();
    let served = getter.await.unwrap();
    assert_eq!(served.unwrap(), body("value"));
    // The journal write of the copy-back is a background task that waits a linger.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(journal_keys(&*rig.hot_mem, "p1").await.len(), 2);

    let mut reconciler = rig.reconciler("r1", 1000);
    let first = reconciler.pass().await;
    assert_eq!(first.failed, Vec::<Error>::new());

    assert_eq!(
        proxy.get("k", None).await,
        Err(Error::NotFound("k".to_owned()))
    );
    assert_absent(&*rig.hot_mem, "k").await;
    assert_absent(&*rig.cold_mem, "k").await;
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

/// Asserts that an object that is deleted and put again with new content while the reconciler
/// is about to evict the old one from the hot tier is not lost: after the eviction and further
/// passes, a GET of the key through the proxy gives the new content.
/// Catches an eviction that deletes the hot tier's key without checking it is still the object
/// it chose, so the put that came after its last read of the journals is deleted before it was
/// copied, and the later passes, finding the hot tier empty, apply the delete entry to the cold
/// tier and leave no object at all.
#[tokio::test(start_paused = true)]
async fn an_eviction_racing_a_delete_and_a_reput_keeps_the_new_object() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let old = "o".repeat(21);
    for key in ["k", "f1", "f2", "f3", "f4"] {
        rig.clock.advance(10);
        proxy.put(key, body(&old)).await.unwrap();
    }
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let held = Arc::new(HoldDelete {
        inner: rig.hot.clone(),
        key: "k",
        entered: entered.clone(),
        release: release.clone(),
        spent: AtomicBool::new(false),
    });
    let mut reconciler = Reconciler::new(held, rig.cold.clone(), rig.reconciler_config("r1", 100));

    let client = async {
        entered.notified().await;
        proxy.delete("k").await.unwrap();
        proxy.put("k", body("new content")).await.unwrap();
        release.notify_one();
    };
    let (report, ()) = tokio::join!(reconciler.pass(), client);
    assert!(report.held);
    assert!(report.evicted >= 1);

    for _ in 0..3 {
        let report = reconciler.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new());
    }
    assert_eq!(proxy.get("k", None).await.unwrap(), body("new content"));
}

/// Asserts that a checkpoint never moves backwards: reconciler `r1` has a pass that is longer
/// than the lease (a copy waits), `r2` takes the lease over, applies two more journal objects
/// than `r1` has read and stores its checkpoint, and then `r1`'s pass ends and stores its own
/// older one. The checkpoint stored for the proxy after that is still `r2`'s (journal objects
/// up to number 4 applied), not `r1`'s (up to number 2).
/// Catches a stale pass that stores its older checkpoint over the new holder's, so that the
/// next holder reads and copies journal objects again and the prune of journal objects follows
/// a number that is older than the work done.
#[tokio::test(start_paused = true)]
async fn a_stale_pass_does_not_move_the_stored_checkpoint_back() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    // One journal object for each put: numbers 1 and 2.
    for name in ["k1", "k2"] {
        proxy.put(name, body(name)).await.unwrap();
    }
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let slow: Arc<dyn Backend> = Arc::new(Stall::new(
        rig.cold.clone(),
        "k2",
        entered.clone(),
        Some(release.clone()),
    ));
    let mut one = Reconciler::new(rig.hot.clone(), slow, rig.reconciler_config("r1", 1 << 20));
    let mut two = rig.reconciler("r2", 1 << 20);
    let takeover = async {
        entered.notified().await;
        // r1 has read journal objects 1 and 2 and waits in the copy of k2. Two more come.
        for name in ["k3", "k4"] {
            proxy.put(name, body(name)).await.unwrap();
        }
        rig.clock.set(30_000);
        let report = two.pass().await;
        assert!(report.held, "{report:?}");
        let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
        assert!(stored.contains("\"seq\":4"), "r2 stored {stored}");
        release.notify_one();
    };

    let (first, ()) = tokio::join!(one.pass(), takeover);

    assert!(first.held, "{first:?}");
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert!(
        stored.contains("\"seq\":4"),
        "the stale pass put the checkpoint back: {stored}"
    );
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

/// Asserts that of two unsigned puts of one key with different bodies, one sent to the cold tier
/// by a node that sees the hot tier full and one made in the hot tier by a node that sees room,
/// interleaved so both pass their checks, at most one is acknowledged (here exactly one: the hot
/// put was acknowledged before the cold put reached the cold tier, so the cold put is refused),
/// and that after a reconciler pass evicts the hot copy a GET of the key returns the body of the
/// acknowledged put, not the body of the refused one.
///
/// Both puts refused is also a failure, so the assertion is exactly one acknowledged.
///
/// Catches a create-only promise that does not hold across the tiers: both puts acknowledged,
/// and the key reading as a body that depends on whether the hot tier has evicted it.
#[tokio::test(start_paused = true)]
async fn an_unsigned_cold_put_and_a_hot_put_of_one_key_do_not_both_get_acknowledged() {
    let rig = Rig::new();
    // The hot tier is at the budget of 1000 bytes for the node `p1` and far below the budget of
    // `p2`, which read the same usage object.
    rig.hot_mem
        .put(
            "_tiering/usage",
            body(r#"{"hot_bytes":1000,"copy_lag":0,"time":0}"#),
        )
        .await
        .unwrap();
    let (entered, go) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let stalled_cold: Arc<dyn Backend> = Arc::new(Stall::new(
        rig.cold.clone(),
        "contended",
        entered.clone(),
        Some(go.clone()),
    ));
    let sees_full = Arc::new(
        Proxy::open(rig.hot.clone(), stalled_cold, rig.config("p1", 1000))
            .await
            .unwrap(),
    );
    let sees_room = rig.proxy("p2", 1 << 30).await;

    // 1. p1: put_stream("contended", "cold bytes"): sees the hot tier full, checks the hot tier
    //    lacks the key, and stops at the cold tier's create.
    let late = {
        let node = sees_full.clone();
        tokio::spawn(async move {
            node.put_stream("contended", None, chunks(&["cold bytes"]))
                .await
        })
    };
    entered.notified().await;
    // 2. p2: put_stream("contended", "hot bytes"): creates it in the hot tier.
    let hot_answer = sees_room
        .put_stream("contended", None, chunks(&["hot bytes"]))
        .await;
    // 3. p1's cold create is released.
    go.notify_one();
    let cold_answer = late.await.unwrap();

    let acknowledged: Vec<&str> = [(&hot_answer, "hot bytes"), (&cold_answer, "cold bytes")]
        .into_iter()
        .filter(|(answer, _)| answer.is_ok())
        .map(|(_, text)| text)
        .collect();
    assert_eq!(
        acknowledged.len(),
        1,
        "hot put: {hot_answer:?}, cold put: {cold_answer:?}"
    );
    let want = body(acknowledged[0]);

    // 4. The reconciler (hot budget 5 bytes, so the 9-byte hot object is over the high-water
    //    mark) copies and evicts; the cold tier alone then holds the acknowledged body.
    let mut reconciler = rig.reconciler("r1", 5);
    let report = reconciler.pass().await;
    assert!(report.held, "{report:?}");
    assert_eq!(report.failed, Vec::<Error>::new());
    assert!(report.evicted >= 1, "{report:?}");
    assert_absent(&*rig.hot_mem, "contended").await;
    assert_eq!(read(&*rig.cold_mem, "contended").await, want);

    // 5. GET through a node: the acknowledged body.
    assert_eq!(sees_room.get("contended", None).await.unwrap(), want);
    assert_eq!(sees_full.get("contended", None).await.unwrap(), want);
}

/// A backend over another that holds the first delete of one key: the call says it has arrived
/// (`entered`) and goes on when `release` is notified. Every other call goes straight through.
struct HoldDelete {
    inner: Arc<dyn Backend>,
    key: &'static str,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    spent: AtomicBool,
}

#[async_trait]
impl Backend for HoldDelete {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
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
        if key == self.key && !self.spent.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
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
