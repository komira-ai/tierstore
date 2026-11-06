//! The stray sweep over markers: which hot objects get a marker, how the two listings are joined,
//! how often it runs, and how it ends when a call fails. Time is the wall clock and the monotonic
//! clock of the rig; the pause between lookups is a recording sleeper.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::format::FormatPhase;
use crate::marks_leader_tests::{Rig, Shape, bytes, dirty, secs, shape_of};
use crate::{
    Backend, ByteRange, CopyRecords, Error, EvictConfig, Evicted, Listed, MarkEvictor, MarkerState,
    MemoryBackend, Meta, Op, StraySweep, SweepConfig, SweepTime,
};

/// How a test changes a content tag.
type Retagger = fn(String) -> String;

/// A view of a memory bucket whose content tags are changed by `retag`, so that a test gives the
/// tags the shapes a store has: a plain MD5 (32 hex digits), a multipart tag, a SHA-256.
struct Retag {
    inner: Arc<MemoryBackend>,
    retag: Retagger,
}

#[async_trait]
impl Backend for Retag {
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
        let mut meta = self.inner.head(key).await?;
        meta.tag = (self.retag)(meta.tag);
        Ok(meta)
    }
    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let mut listed = self.inner.list(prefix).await?;
        for object in &mut listed {
            object.tag = (self.retag)(std::mem::take(&mut object.tag));
        }
        Ok(listed)
    }
}

fn md5ish(tag: String) -> String {
    tag[..32].to_owned()
}

fn sha256(tag: String) -> String {
    tag
}

fn multipart(tag: String) -> String {
    format!("{}-2", &tag[..32])
}

fn retagged(memory: &Arc<MemoryBackend>, retag: Retagger) -> Arc<dyn Backend> {
    Arc::new(Retag {
        inner: memory.clone(),
        retag,
    })
}

fn left_dirty() -> Shape {
    (MarkerState::Dirty, false, true, None, None)
}

fn time(rig: &Rig) -> SweepTime {
    SweepTime {
        clock: rig.clock.clone(),
        mono: rig.mono.clone(),
        waiter: rig.waits.clone(),
    }
}

fn sweep_over(
    rig: &Rig,
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    records: &Arc<CopyRecords>,
    tweak: impl FnOnce(&mut SweepConfig),
) -> StraySweep {
    let mut config = SweepConfig::default();
    tweak(&mut config);
    StraySweep::new(
        hot,
        cold,
        rig.marks.clone(),
        rig.markers("S"),
        records.clone(),
        time(rig),
        config,
    )
}

fn sweeper(rig: &Rig) -> StraySweep {
    let records = Arc::new(CopyRecords::default());
    sweep_over(rig, rig.hot.clone(), rig.cold.clone(), &records, |_| {})
}

/// Makes the objects written so far older than the stray grace.
fn age(rig: &Rig) {
    rig.clock.advance(600_001);
}

/// Asserts that a hot object old enough, with no marker, is adopted with a `DIRTY(gone = false)`
/// marker when the cold tier has no object, one of another size, or (both tags being plain MD5s)
/// another tag; and that adoption writes nothing to either tier.
#[tokio::test]
async fn the_sweep_adopts_a_hot_object_with_no_marker_and_no_cold_or_a_different_cold() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot_mem.put("no-cold", bytes("aaaa")).await.unwrap();
    rig.hot_mem.put("other-size", bytes("aaaa")).await.unwrap();
    rig.cold_mem.put("other-size", bytes("aa")).await.unwrap();
    rig.hot_mem.put("other-tag", bytes("aaaa")).await.unwrap();
    rig.cold_mem.put("other-tag", bytes("bbbb")).await.unwrap();
    age(&rig);
    let records = Arc::new(CopyRecords::default());
    let mut sweep = sweep_over(
        &rig,
        retagged(&rig.hot_mem, md5ish),
        retagged(&rig.cold_mem, md5ish),
        &records,
        |_| {},
    );
    let report = sweep.run().await;
    assert!(report.active);
    assert_eq!(report.adopted, 3);
    assert_eq!(report.failed, []);
    for key in ["no-cold", "other-size", "other-tag"] {
        assert_eq!(shape_of(&rig, key).await, Some(left_dirty()), "{key}");
    }
    assert_eq!(
        rig.hot_mem.get("other-tag", None).await.unwrap(),
        bytes("aaaa")
    );
    assert_eq!(
        rig.cold_mem.get("other-tag", None).await.unwrap(),
        bytes("bbbb")
    );
}

/// Asserts that the sweep leaves a hot object that has a marker (of any state), one whose cold
/// copy has the same plain-MD5 tag, one under a reserved prefix, and one younger than the grace;
/// and that a marker the listing could not have shown when it was taken (made after it) is not
/// overwritten: the create is create-only.
#[tokio::test]
async fn the_sweep_leaves_an_object_with_a_marker_or_a_matching_cold() {
    let rig = Rig::new();
    rig.activate().await;
    for key in ["marked", "copy", "_tiering/usage", "_journal/x"] {
        rig.hot_mem.put(key, bytes("aaaa")).await.unwrap();
    }
    rig.seed(&crate::marks_leader_tests::lock(
        "marked",
        crate::MarkerWhy::Overwrite,
        "p1",
    ))
    .await;
    rig.cold_mem.put("copy", bytes("aaaa")).await.unwrap();
    age(&rig);
    rig.hot_mem.put("young", bytes("aaaa")).await.unwrap();
    let records = Arc::new(CopyRecords::default());
    let mut sweep = sweep_over(
        &rig,
        retagged(&rig.hot_mem, md5ish),
        retagged(&rig.cold_mem, md5ish),
        &records,
        |_| {},
    );
    let report = sweep.run().await;
    assert_eq!(report.adopted, 0);
    assert_eq!(rig.marks_mem.list("").await.unwrap().len(), 1);
    assert_eq!(rig.cold.calls_on(Op::Head, "marked"), 0);
    assert_eq!(rig.cold.calls_on(Op::Head, "young"), 0);
}

/// Asserts that no time of either store is compared: a cold copy of the same bytes that is
/// written an hour before the hot object is left, and one of other bytes written an hour after it
/// is adopted; only the sizes and the tags decide.
#[tokio::test]
async fn the_sweep_compares_no_modified_time() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold_mem.put("older-cold", bytes("aaaa")).await.unwrap();
    rig.clock.advance(3_600_000);
    rig.hot_mem.put("older-cold", bytes("aaaa")).await.unwrap();
    rig.hot_mem.put("newer-cold", bytes("aaaa")).await.unwrap();
    rig.clock.advance(3_600_000);
    rig.cold_mem.put("newer-cold", bytes("bbbb")).await.unwrap();
    rig.clock.advance(3_600_000);
    let records = Arc::new(CopyRecords::default());
    let mut sweep = sweep_over(
        &rig,
        retagged(&rig.hot_mem, md5ish),
        retagged(&rig.cold_mem, md5ish),
        &records,
        |_| {},
    );
    assert_eq!(sweep.run().await.adopted, 1);
    assert_eq!(shape_of(&rig, "older-cold").await, None);
    assert_eq!(shape_of(&rig, "newer-cold").await, Some(left_dirty()));
}

/// Asserts the cases whose tags cannot be compared (a SHA-256 of a `dir:` hot tier, a multipart
/// tag): equal sizes with no record of the pair are left, however the bytes differ; with a record
/// whose hot tag is no longer the hot object's, the object is adopted (it changed since the leader
/// copied it); with a record that has the current tag it is left.
#[tokio::test]
async fn tags_that_cannot_be_compared_are_left_unless_the_recorded_pair_shows_a_change() {
    let cases: [(Retagger, Retagger); 3] = [
        (sha256, sha256),
        (multipart, multipart),
        (md5ish, multipart),
    ];
    for (hot_retag, cold_retag) in cases {
        let rig = Rig::new();
        rig.activate().await;
        for key in ["same", "differs", "changed", "current"] {
            rig.hot_mem.put(key, bytes("aaaa")).await.unwrap();
        }
        rig.cold_mem.put("same", bytes("aaaa")).await.unwrap();
        for key in ["differs", "changed", "current"] {
            rig.cold_mem.put(key, bytes("bbbb")).await.unwrap();
        }
        age(&rig);
        let records = Arc::new(CopyRecords::default());
        let (hot_tag, cold_tag) = rig.tags("current").await;
        records.record("current", &hot_retag(hot_tag), &cold_retag(cold_tag), 0, 10);
        records.record("changed", "an older hot tag", "a cold tag", 0, 10);
        let hot = retagged(&rig.hot_mem, hot_retag);
        let cold = retagged(&rig.cold_mem, cold_retag);
        let mut sweep = sweep_over(&rig, hot, cold, &records, |_| {});
        assert_eq!(sweep.run().await.adopted, 1);
        assert_eq!(shape_of(&rig, "changed").await, Some(left_dirty()));
        for key in ["same", "differs", "current"] {
            assert_eq!(shape_of(&rig, key).await, None, "{key}");
        }
    }
}

/// Asserts that a hot object that landed without its marker, over a cold object of the same size
/// and other bytes (7.1), is adopted by the sweep, and that the eviction never evicts it unequal:
/// before the sweep, the eviction's compare finds other bytes and marks it `DIRTY` and keeps hot.
#[tokio::test]
async fn an_object_landed_without_its_marker_is_adopted_by_the_sweep_and_never_evicted_unequal() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold_mem.put("k", bytes("old!")).await.unwrap();
    rig.hot_mem.put("k", bytes("new!")).await.unwrap();
    age(&rig);
    let records = Arc::new(CopyRecords::default());
    let mut sweep = sweep_over(
        &rig,
        retagged(&rig.hot_mem, md5ish),
        retagged(&rig.cold_mem, md5ish),
        &records,
        |_| {},
    );
    assert_eq!(sweep.run().await.adopted, 1);
    assert_eq!(shape_of(&rig, "k").await, Some(left_dirty()));

    // The same state, met by the eviction first.
    rig.marks_mem.delete("k").await.unwrap();
    let lease = rig.lease("L").await;
    let evictor = MarkEvictor::new(
        rig.hot.clone(),
        rig.cold.clone(),
        rig.markers("L"),
        lease,
        records,
        rig.mono.clone(),
        EvictConfig::default(),
    );
    rig.mono.advance(secs(600));
    assert_eq!(evictor.evict_one("k").await, Ok(Evicted::Dirtied));
    assert_eq!(shape_of(&rig, "k").await, Some(left_dirty()));
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), bytes("new!"));
}

/// Asserts the merge-join over pages of two objects: hot objects and markers interleaved, a marker
/// before the first object, markers between, a marker of a key hot does not have, and markers
/// after the last object, so that each unmarked object is found and each marked one is not.
#[tokio::test]
async fn the_join_of_the_two_listings_finds_each_unmarked_object_over_pages() {
    let rig = Rig::new();
    rig.activate().await;
    let keys = ["a", "b", "c", "d", "e", "f", "g"];
    for key in keys {
        rig.hot_mem.put(key, bytes("v")).await.unwrap();
    }
    for key in ["0", "b", "d", "d2", "e", "zz"] {
        rig.seed(&dirty(key)).await;
    }
    age(&rig);
    let records = Arc::new(CopyRecords::default());
    let mut sweep = sweep_over(
        &rig,
        rig.hot.clone(),
        rig.cold.clone(),
        &records,
        |config| {
            config.page_size = 2;
        },
    );
    let report = sweep.run().await;
    assert_eq!(report.adopted, 4);
    for key in ["a", "c", "f", "g"] {
        assert_eq!(shape_of(&rig, key).await, Some(left_dirty()), "{key}");
    }
    assert_eq!(rig.cold.calls(Op::Head), 4);
}

/// Asserts the rate: the sweep waits `1 / stray_rate` seconds (rounded up to a microsecond) before
/// each cold lookup, and before no other request; at the default of 100 a second, 10 ms.
#[tokio::test]
async fn the_cold_tier_is_asked_about_one_object_per_stray_rate() {
    let rig = Rig::new();
    rig.activate().await;
    for key in ["a", "b"] {
        rig.hot_mem.put(key, bytes("v")).await.unwrap();
    }
    rig.hot_mem.put("_tiering/x", bytes("v")).await.unwrap();
    age(&rig);
    let mut sweep = sweeper(&rig);
    sweep.run().await;
    assert_eq!(rig.waits.asked(), [Duration::from_millis(10); 2]);

    let rig = Rig::new();
    rig.activate().await;
    rig.hot_mem.put("a", bytes("v")).await.unwrap();
    age(&rig);
    let records = Arc::new(CopyRecords::default());
    let mut sweep = sweep_over(
        &rig,
        rig.hot.clone(),
        rig.cold.clone(),
        &records,
        |config| {
            config.stray_rate = 3;
        },
    );
    sweep.run().await;
    assert_eq!(rig.waits.asked(), [Duration::from_micros(333_334)]);
}

/// Asserts the schedule: the sweep is due at takeover, runs, is not due again until
/// `stray_interval` has passed on the monotonic clock (to the millisecond), is due again at once
/// after a takeover, and a sweep that failed is due at the next call.
#[tokio::test]
async fn the_sweep_runs_at_takeover_and_then_only_after_its_interval() {
    let rig = Rig::new();
    rig.activate().await;
    let mut sweep = sweeper(&rig);
    assert!(sweep.due());
    rig.hot
        .fail_next_with(Op::List, Error::Unavailable("down".into()));
    let failed = sweep.run().await;
    assert_eq!(failed.failed.len(), 1);
    assert!(sweep.due());
    assert!(sweep.run().await.failed.is_empty());
    assert!(!sweep.due());
    rig.mono
        .advance(Duration::from_millis(24 * 3600 * 1000 - 1));
    assert!(!sweep.due());
    rig.mono.advance(Duration::from_millis(1));
    assert!(sweep.due());
    sweep.run().await;
    assert!(!sweep.due());
    sweep.took_over();
    assert!(sweep.due());
}

/// Asserts that the sweep does nothing under a format that is not `active-marks` (it reads the
/// format object only), and that a format object that cannot be read is the error and ends it.
#[tokio::test]
async fn the_sweep_does_nothing_unless_the_format_says_active_marks() {
    for phase in [
        None,
        Some(FormatPhase::Drain),
        Some(FormatPhase::Drained),
        Some(FormatPhase::Active),
        Some(FormatPhase::Unknown),
    ] {
        let rig = Rig::new();
        if let Some(phase) = phase {
            rig.format(phase).await;
        }
        rig.hot_mem.put("a", bytes("v")).await.unwrap();
        age(&rig);
        let mut sweep = sweeper(&rig);
        let report = sweep.run().await;
        assert!(!report.active, "{phase:?}");
        assert_eq!((report.adopted, report.uploads_aborted), (0, 0));
        assert_eq!(rig.marks.calls(Op::Put), 0);
        assert_eq!(rig.hot.calls(Op::List), 0);
        assert!(sweep.due());
    }
    let rig = Rig::new();
    let mut sweep = sweeper(&rig);
    let down = Error::Unavailable("hot down".into());
    rig.hot.fail_next_with(Op::Get, down.clone());
    let report = sweep.run().await;
    assert!(!report.active);
    assert_eq!(report.failed, [down]);
}

/// Asserts how a failure ends the sweep: a failed listing of the hot tier, of the markers, a failed
/// cold lookup, and a failed listing of uploads each end it with the error and leave it due; a
/// failed create of a marker is noted and the sweep goes on with the next object.
#[tokio::test]
async fn a_failed_call_ends_the_sweep_and_a_failed_marker_create_does_not() {
    for failing in ["hot list", "marks list", "cold head", "uploads"] {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot_mem.put("a", bytes("v")).await.unwrap();
        age(&rig);
        let down = Error::Unavailable(failing.to_owned());
        match failing {
            "hot list" => rig.hot.fail_next_with(Op::List, down.clone()),
            "marks list" => rig.marks.fail_next_with(Op::List, down.clone()),
            "cold head" => rig.cold.fail_next_with(Op::Head, down.clone()),
            _ => rig.hot.fail_next_with(Op::ListUploads, down.clone()),
        }
        let mut sweep = sweeper(&rig);
        let report = sweep.run().await;
        assert_eq!(report.failed, [down], "{failing}");
        assert!(sweep.due(), "{failing}");
    }

    let rig = Rig::new();
    rig.activate().await;
    rig.hot_mem.put("a", bytes("v")).await.unwrap();
    rig.hot_mem.put("b", bytes("v")).await.unwrap();
    age(&rig);
    let down = Error::Unavailable("marks down".into());
    rig.marks.fail_next_on(Op::Put, "a", down.clone());
    let mut sweep = sweeper(&rig);
    let report = sweep.run().await;
    assert_eq!(report.failed, [down]);
    assert_eq!(report.adopted, 1);
    assert_eq!(shape_of(&rig, "a").await, None);
    assert_eq!(shape_of(&rig, "b").await, Some(left_dirty()));
    assert!(!sweep.due());
}

/// Asserts that a marker made between the listing and the create (so that the create is refused)
/// is not an adoption and not an error.
#[tokio::test]
async fn a_marker_made_since_the_listing_is_not_overwritten() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot_mem.put("a", bytes("v")).await.unwrap();
    age(&rig);
    let gated = Arc::new(crate::GateBackend::new(rig.cold.clone()));
    let lookup = gated.gate(Op::Head, "a", crate::Phase::Post);
    let records = Arc::new(CopyRecords::default());
    let mut sweep = sweep_over(&rig, rig.hot.clone(), gated, &records, |_| {});
    let mut running = Box::pin(sweep.run());
    crate::marks_leader_tests::until_parked(&lookup, &mut running).await;
    rig.seed(&dirty("a")).await;
    let before = rig.marker("a").await;
    lookup.release();
    let report = running.await;
    assert_eq!((report.adopted, report.failed.len()), (0, 0));
    assert_eq!(rig.marker("a").await, before);
}

// ---- abandoned uploads ----

/// Asserts that the sweep aborts a hot-tier multipart upload started more than
/// `abandoned_upload_after` ago (7 days), at the boundary to the millisecond, and leaves a younger
/// one, one under a reserved prefix, one whose start the store does not know, and the uploads of
/// the cold tier, which are the client's.
#[tokio::test]
async fn the_sweep_aborts_a_hot_upload_older_than_abandoned_upload_after_and_leaves_a_cold_one() {
    let rig = Rig::new();
    rig.activate().await;
    rig.clock.set(0);
    let unknown = rig.hot_mem.create_upload("unknown-start").await.unwrap();
    rig.clock.set(1000);
    rig.hot_mem.create_upload("old").await.unwrap();
    let reserved = rig.hot_mem.create_upload("_tiering/part").await.unwrap();
    rig.cold_mem.create_upload("old").await.unwrap();
    rig.clock.set(2001);
    let young = rig.hot_mem.create_upload("young").await.unwrap();
    // Seven days and a second after the old upload, and a millisecond before the young one is
    // seven days old.
    rig.clock.set(1000 + 7 * 24 * 3600 * 1000 + 1000);
    let mut sweep = sweeper(&rig);
    let report = sweep.run().await;
    assert_eq!(report.uploads_aborted, 1);
    let open = rig.hot_mem.list_uploads("", None, 100).await.unwrap();
    let mut keys: Vec<(&str, &str)> = open
        .iter()
        .map(|u| (u.key.as_str(), u.id.as_str()))
        .collect();
    keys.sort_unstable();
    let mut want = vec![
        ("_tiering/part", reserved.as_str()),
        ("unknown-start", unknown.as_str()),
        ("young", young.as_str()),
    ];
    want.sort_unstable();
    assert_eq!(keys, want);
    assert_eq!(
        rig.cold_mem
            .list_uploads("", None, 100)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// Asserts that an upload exactly `abandoned_upload_after` old is aborted and one a millisecond
/// younger is not, that the abort is off when the setting is zero (the store is not asked), that
/// uploads are walked in pages, that an upload its client completed or aborted meanwhile is not
/// counted, and that other failures of an abort, and a store without uploads, end the walk as the
/// design says.
#[tokio::test]
async fn the_abort_of_abandoned_uploads_follows_its_setting_and_its_failures() {
    let week = 7 * 24 * 3600 * 1000;
    let rig = Rig::new();
    rig.activate().await;
    rig.clock.set(10_000);
    rig.hot_mem.create_upload("exact").await.unwrap();
    rig.clock.set(10_001);
    rig.hot_mem.create_upload("one-younger").await.unwrap();
    rig.clock.set(10_000 + week);
    let records = Arc::new(CopyRecords::default());
    let mut sweep = sweep_over(
        &rig,
        rig.hot.clone(),
        rig.cold.clone(),
        &records,
        |config| {
            config.page_size = 1;
        },
    );
    assert_eq!(sweep.run().await.uploads_aborted, 1);
    let open = rig.hot_mem.list_uploads("", None, 100).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].key, "one-younger");

    let mut off = sweep_over(
        &rig,
        rig.hot.clone(),
        rig.cold.clone(),
        &records,
        |config| {
            config.abandoned_upload_after = Duration::ZERO;
        },
    );
    rig.clock.advance(2 * week);
    let listed = rig.hot.calls(Op::ListUploads);
    assert_eq!(off.run().await.uploads_aborted, 0);
    assert_eq!(rig.hot.calls(Op::ListUploads), listed);

    rig.hot
        .fail_next_with(Op::AbortUpload, Error::NotFound("gone".into()));
    let mut on = sweeper(&rig);
    assert_eq!(on.run().await.uploads_aborted, 0);
    assert!(on.run().await.failed.is_empty());

    let rig2 = Rig::new();
    rig2.activate().await;
    rig2.hot_mem.create_upload("u").await.unwrap();
    rig2.clock.advance(2 * week);
    let refused = Error::Unavailable("abort refused".into());
    rig2.hot.fail_next_with(Op::AbortUpload, refused.clone());
    let mut sweep = sweeper(&rig2);
    assert_eq!(sweep.run().await.failed, [refused]);
    assert!(sweep.due());

    rig2.hot
        .fail_next_with(Op::ListUploads, Error::Unsupported("no uploads".into()));
    let mut sweep = sweeper(&rig2);
    let report = sweep.run().await;
    assert_eq!((report.failed.len(), report.uploads_aborted), (0, 0));
    assert!(!sweep.due());
}
