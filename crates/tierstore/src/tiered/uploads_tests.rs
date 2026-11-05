//! The multipart uploads of the proxy: the tier an upload is made in and how its id names it, the
//! parts, the completion on the entry, the abort and the listing across both tiers, and what the
//! reconciler does with an upload's object and with an upload that was left open.
//!
//! No test reads the wall clock or asserts a time: the clocks are moved by hand, and each
//! assertion is about the calls the tiers saw or the objects and entries they hold.

use std::sync::Arc;

use super::entry_put_tests::{Rig, body, chunks, held_by, pending};
use super::uploads::HOT_PREFIX;
use crate::journal::{is_reserved, proxy_key};
use crate::{Backend, EntryReconciler, EntryReconcilerConfig, Error, Op, Phase, ReconcilerConfig};

/// A reconciler `r1` over the rig's tiers, behind no fault injector.
fn reconciler(rig: &Rig) -> EntryReconciler {
    let mut base = ReconcilerConfig::new("r1", 1 << 20, rig.clock.clone());
    base.mono = rig.mono.clone();
    EntryReconciler::new(
        rig.hot_mem.clone(),
        rig.cold_mem.clone(),
        EntryReconcilerConfig::new(base),
    )
}

/// Steps `reconciler` until a step changes nothing.
async fn settle(reconciler: &mut EntryReconciler) {
    loop {
        let step = reconciler.step().await;
        assert!(step.failed.is_empty(), "{:?}", step.failed);
        if !step.progress() {
            return;
        }
    }
}

/// Uploads one part of `text` to the upload `id` of `key` and gives its tag.
async fn part(rig: &Rig, key: &str, id: &str, number: u32, text: &'static str) -> String {
    let stored = rig.proxy.upload_part(key, id, number, chunks(&[text]));
    stored.await.unwrap()
}

/// The calls of a kind that the cold tier saw, for the kinds of a multipart upload.
fn cold_upload_calls(rig: &Rig) -> [u64; 4] {
    [
        Op::CreateUpload,
        Op::UploadPart,
        Op::CompleteUpload,
        Op::AbortUpload,
    ]
    .map(|op| rig.cold.calls(op))
}

/// Asserts that with room in the hot tier an upload's creation, parts and completion are calls of
/// the hot tier and none of the cold tier, that the id the client holds is not the hot tier's own,
/// that the completion leaves the object in the hot tier and a `(PUT, PENDING)` entry, and that the
/// reconciler then copies the object to the cold tier and removes the entry.
#[tokio::test]
async fn an_upload_with_room_in_hot_lands_in_hot_and_the_reconciler_copies_it() {
    let rig = Rig::active().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    assert!(id.starts_with(HOT_PREFIX), "{id}");
    let one = part(&rig, "k", &id, 1, "hello ").await;
    let two = part(&rig, "k", &id, 2, "world").await;
    let parts = [(1, one), (2, two)];
    rig.proxy.complete_upload("k", &id, &parts).await.unwrap();

    assert_eq!(rig.hot_faults.calls(Op::CreateUpload), 1);
    assert_eq!(rig.hot_faults.calls(Op::UploadPart), 2);
    assert_eq!(rig.hot_faults.calls(Op::CompleteUpload), 1);
    assert_eq!(cold_upload_calls(&rig), [0; 4]);
    assert_eq!(
        rig.hot_mem.get("k", None).await.unwrap(),
        body("hello world")
    );
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, pending());

    let mut reconciler = reconciler(&rig);
    settle(&mut reconciler).await;
    assert_eq!(
        rig.cold_mem.get("k", None).await.unwrap(),
        body("hello world")
    );
    assert_eq!(
        rig.hot_mem.get("k", None).await.unwrap(),
        body("hello world")
    );
    assert!(rig.entry("k").await.is_none());
    assert_eq!(cold_upload_calls(&rig), [0; 4]);
}

/// Asserts that ListParts follows the id: the parts of an upload made in the hot tier are listed
/// through the proxy with the numbers and tags they were given, and so are those of an upload made
/// in the cold tier, and an upload of the other tier's id is `NoSuchUpload`. Catches ListParts
/// that asks the cold tier whatever the id says, which answers `NoSuchUpload` for every upload
/// made in the hot tier (`ts-hot~` ids).
#[tokio::test]
async fn the_parts_of_an_upload_are_listed_from_the_tier_its_id_names() {
    let rig = Rig::active().await;
    let hot = rig.proxy.create_upload("k").await.unwrap();
    assert!(hot.starts_with(HOT_PREFIX), "{hot}");
    let tag = part(&rig, "k", &hot, 1, "hello").await;
    let listed = rig.proxy.list_parts("k", &hot, None, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].number, &listed[0].tag), (1, &tag));
    rig.fill_hot().await;
    let cold = rig.proxy.create_upload("c").await.unwrap();
    assert!(!cold.starts_with(HOT_PREFIX), "{cold}");
    let tag = part(&rig, "c", &cold, 2, "world").await;
    let listed = rig.proxy.list_parts("c", &cold, None, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].number, &listed[0].tag), (2, &tag));
    assert!(rig.proxy.list_parts("c", &hot, None, 10).await.is_err());
}

/// Asserts that with the hot tier full an upload goes to the cold tier only, with the cold tier's
/// own id, and its completion leaves the object in the cold tier and a `(PUT, PENDING)` entry that
/// the reconciler only removes. The hot tier is asked for no upload call.
#[tokio::test]
async fn an_upload_with_the_hot_tier_full_lands_in_cold() {
    let rig = Rig::active().await;
    rig.fill_hot().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    assert!(!id.starts_with(HOT_PREFIX), "{id}");
    let one = part(&rig, "k", &id, 1, "ab").await;
    rig.proxy
        .complete_upload("k", &id, &[(1, one)])
        .await
        .unwrap();
    assert_eq!(rig.cold_mem.get("k", None).await.unwrap(), body("ab"));
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(cold_upload_calls(&rig), [1, 1, 1, 0]);
    for op in [
        Op::CreateUpload,
        Op::UploadPart,
        Op::CompleteUpload,
        Op::AbortUpload,
    ] {
        assert_eq!(rig.hot_faults.calls(op), 0, "{op:?}");
    }
    let mut reconciler = reconciler(&rig);
    settle(&mut reconciler).await;
    assert!(rig.entry("k").await.is_none());
    assert!(!rig.hot_has("k").await);
}

/// Asserts that a hot tier that answers `Full` to the creation sends the upload to the cold tier,
/// and that any other answer of the hot tier (it is down, it does not do uploads) is the proxy's
/// answer and the cold tier is not tried.
#[tokio::test]
async fn a_hot_tier_that_answers_full_to_the_creation_sends_the_upload_to_cold() {
    let rig = Rig::active().await;
    rig.hot_faults
        .fail_next_with(Op::CreateUpload, Error::Full("k".to_owned()));
    let id = rig.proxy.create_upload("k").await.unwrap();
    assert!(!id.starts_with(HOT_PREFIX), "{id}");
    assert_eq!(rig.cold.calls(Op::CreateUpload), 1);
    assert_eq!(
        rig.cold_mem.list_uploads("", None, 10).await.unwrap().len(),
        1
    );

    let down = Error::Unavailable("down".to_owned());
    rig.hot_faults
        .fail_next_with(Op::CreateUpload, down.clone());
    assert_eq!(rig.proxy.create_upload("k").await, Err(down));
    let none = Error::Unsupported("no uploads".to_owned());
    rig.hot_faults
        .fail_next_with(Op::CreateUpload, none.clone());
    assert_eq!(rig.proxy.create_upload("k").await, Err(none));
    assert_eq!(rig.cold.calls(Op::CreateUpload), 1);
}

/// Asserts that an id that has no prefix, as every id of the release before this one, names a cold
/// upload, though the hot tier has an upload of the very same id: its parts, its completion and
/// its abort are the cold tier's, and the hot tier's upload is left open.
#[tokio::test]
async fn an_id_without_the_prefix_is_a_cold_upload() {
    let rig = Rig::active().await;
    let hot_id = rig.proxy.create_upload("k").await.unwrap();
    let old = rig.cold_mem.create_upload("k").await.unwrap();
    assert_eq!(hot_id, format!("{HOT_PREFIX}{old}"));
    let one = part(&rig, "k", &old, 1, "old").await;
    rig.proxy
        .complete_upload("k", &old, &[(1, one)])
        .await
        .unwrap();
    assert_eq!(rig.cold_mem.get("k", None).await.unwrap(), body("old"));
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(rig.hot_faults.calls(Op::UploadPart), 0);
    assert_eq!(rig.hot_faults.calls(Op::CompleteUpload), 0);
    assert_eq!(
        rig.hot_mem.list_uploads("", None, 10).await.unwrap().len(),
        1
    );

    let other = rig.cold_mem.create_upload("j").await.unwrap();
    rig.proxy.abort_upload("j", &other).await.unwrap();
    assert!(
        rig.cold_mem
            .list_uploads("j", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(rig.hot_faults.calls(Op::AbortUpload), 0);
}

/// Asserts that an abort goes to the tier the id names (the hot tier's upload is gone and the cold
/// tier is not asked), that a part or a completion of an aborted upload is not found, and that a
/// reserved key is refused.
#[tokio::test]
async fn an_abort_follows_the_id_and_the_upload_is_gone() {
    let rig = Rig::active().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    part(&rig, "k", &id, 1, "x").await;
    rig.proxy.abort_upload("k", &id).await.unwrap();
    assert_eq!(rig.hot_faults.calls(Op::AbortUpload), 1);
    assert_eq!(cold_upload_calls(&rig), [0; 4]);
    assert!(
        rig.hot_mem
            .list_uploads("", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let late = rig.proxy.upload_part("k", &id, 2, chunks(&["y"])).await;
    assert!(matches!(late, Err(Error::NotFound(_))), "{late:?}");
    let done = rig.proxy.complete_upload("k", &id, &[]).await;
    assert!(matches!(done, Err(Error::NotFound(_))), "{done:?}");
    assert_eq!(rig.shape("k").await, None);
    let key = "_tiering/lease";
    let refused = rig.proxy.abort_upload(key, &id).await;
    assert_eq!(refused, Err(Error::Reserved(key.to_owned())));
}

/// Asserts that a hot tier with no room for a part answers `Full` (the S3 front makes it a 503) and
/// keeps the upload, so that the same part is accepted when there is room, and the upload then
/// completes in the hot tier: no part is sent to the cold tier.
#[tokio::test]
async fn a_part_the_hot_tier_has_no_room_for_is_refused_and_the_upload_goes_on() {
    let rig = Rig::active().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    let one = part(&rig, "k", &id, 1, "a").await;
    let full = Error::Full("k".to_owned());
    rig.hot_faults.fail_next_with(Op::UploadPart, full.clone());
    let refused = rig.proxy.upload_part("k", &id, 2, chunks(&["b"])).await;
    assert_eq!(refused, Err(full));
    let two = part(&rig, "k", &id, 2, "b").await;
    let parts = [(1, one), (2, two)];
    rig.proxy.complete_upload("k", &id, &parts).await.unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("ab"));
    assert_eq!(cold_upload_calls(&rig), [0; 4]);
}

/// Asserts that an upload made while the hot tier had room goes on when the usage says the hot tier
/// is full: its parts and its completion are accepted in the hot tier (the tier refuses a part
/// itself when it has no room), and the object lands in the hot tier.
#[tokio::test]
async fn an_upload_that_began_with_room_goes_on_when_the_usage_reaches_the_budget() {
    let rig = Rig::active().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    rig.fill_hot().await;
    let one = part(&rig, "k", &id, 1, "a").await;
    rig.proxy
        .complete_upload("k", &id, &[(1, one)])
        .await
        .unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("a"));
    assert_eq!(cold_upload_calls(&rig), [0; 4]);
}

/// Asserts that a hot tier with no room to make the object at completion answers `Full`, removes
/// the entry the completion took and leaves the upload open, that the key is free for any other
/// put meanwhile, and that the same completion is accepted when there is room.
#[tokio::test]
async fn a_completion_the_hot_tier_has_no_room_for_is_refused_and_can_be_tried_again() {
    let rig = Rig::active().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    let one = part(&rig, "k", &id, 1, "a").await;
    let full = Error::Full("k".to_owned());
    rig.hot_faults
        .fail_next_with(Op::CompleteUpload, full.clone());
    let parts = [(1, one)];
    let refused = rig.proxy.complete_upload("k", &id, &parts).await;
    assert_eq!(refused, Err(full));
    assert_eq!(rig.shape("k").await, None);
    assert!(!rig.hot_has("k").await && !rig.cold_has("k").await);
    assert_eq!(
        rig.hot_mem.list_uploads("", None, 10).await.unwrap().len(),
        1
    );
    rig.proxy.complete_upload("k", &id, &parts).await.unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("a"));
}

/// Asserts that a completion that meets a key either tier holds is answered `AlreadyExists`, as a
/// put is, and changes neither object, and that the upload stays open: a key the cold tier holds
/// (it was evicted from the hot tier), and a key the hot tier holds.
#[tokio::test]
async fn a_completion_of_a_key_either_tier_holds_is_refused_as_a_put_is() {
    let rig = Rig::active().await;
    rig.cold_mem.put("cold", body("in cold")).await.unwrap();
    rig.hot_mem.put("hot", body("in hot")).await.unwrap();
    for key in ["cold", "hot"] {
        let id = rig.proxy.create_upload(key).await.unwrap();
        let one = part(&rig, key, &id, 1, "new").await;
        let refused = rig.proxy.complete_upload(key, &id, &[(1, one)]).await;
        assert_eq!(refused, Err(Error::AlreadyExists(key.to_owned())));
        assert_eq!(rig.shape(key).await, None);
    }
    assert_eq!(
        rig.cold_mem.get("cold", None).await.unwrap(),
        body("in cold")
    );
    assert!(!rig.hot_has("cold").await);
    assert_eq!(rig.hot_mem.get("hot", None).await.unwrap(), body("in hot"));
    assert_eq!(
        rig.hot_mem.list_uploads("", None, 10).await.unwrap().len(),
        2
    );
}

/// Asserts that a completion and a put of one key that meet keep write-once, in both orders: while
/// the completion holds the entry the put is answered `Unavailable` and the completion lands with
/// its own bytes; and a put that landed first makes the completion `AlreadyExists`, the put's bytes
/// stay and the upload is left open.
#[tokio::test]
async fn a_completion_racing_a_put_of_the_same_key_keeps_write_once() {
    let rig = Rig::active().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    let one = part(&rig, "k", &id, 1, "upload").await;
    let gate = rig.hot.gate(Op::CompleteUpload, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let parts = [(1, one)];
    let owned = (id.clone(), parts.clone());
    let completion =
        tokio::spawn(async move { proxy.complete_upload("k", &owned.0, &owned.1).await });
    gate.parked().await;
    assert_eq!(rig.shape("k").await, held_by("p1"));
    let put = rig.proxy.put("k", body("put")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert!(gate.release());
    completion.await.unwrap().unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("upload"));
    let again = rig.proxy.put("k", body("put")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));

    let id = rig.proxy.create_upload("j").await.unwrap();
    let one = part(&rig, "j", &id, 1, "upload").await;
    rig.proxy.put("j", body("put")).await.unwrap();
    let done = rig.proxy.complete_upload("j", &id, &[(1, one)]).await;
    assert_eq!(done, Err(Error::AlreadyExists("j".to_owned())));
    assert_eq!(rig.hot_mem.get("j", None).await.unwrap(), body("put"));
    assert!(rig.hot_mem.list_uploads("j", None, 10).await.unwrap().len() == 1);
}

/// Asserts that a node that stops after its completion landed the object in the hot tier and before
/// it moved the entry to pending leaves a held entry that the reconciler reaps once the owner has
/// departed, aborting the uploads of the key on the way, and then copies to the cold tier and
/// removes: the object is in both tiers, no entry and no upload is left.
#[tokio::test]
async fn a_node_that_stops_between_the_completion_and_the_entry_is_recovered_by_the_reconciler() {
    let rig = Rig::active().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    let one = part(&rig, "k", &id, 1, "landed").await;
    let _other = rig.proxy.create_upload("k").await.unwrap();
    let gate = rig
        .hot
        .gate(Op::PutIfMatch, &crate::entry::entry_key("k"), Phase::Pre);
    let proxy = rig.proxy.clone();
    let completion = tokio::spawn(async move {
        let parts = [(1, one)];
        proxy.complete_upload("k", &id, &parts).await
    });
    gate.parked().await;
    completion.abort();
    assert!(completion.await.unwrap_err().is_cancelled());
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("landed"));
    assert_eq!(rig.shape("k").await, held_by("p1"));
    rig.hot_mem.delete(&proxy_key("p1")).await.unwrap();

    let mut reconciler = reconciler(&rig);
    settle(&mut reconciler).await;
    assert_eq!(rig.cold_mem.get("k", None).await.unwrap(), body("landed"));
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("landed"));
    assert!(rig.entry("k").await.is_none());
    assert!(
        rig.hot_mem
            .list_uploads("", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Asserts that an object the completion landed in the hot tier whose entry is gone, and that the
/// cold tier does not hold, is adopted by the stray sweep once it is old enough and is copied.
#[tokio::test(start_paused = true)]
async fn an_object_a_completion_landed_with_no_entry_is_adopted_by_the_stray_sweep() {
    let rig = Rig::active().await;
    rig.clock.advance(1000);
    let id = rig.proxy.create_upload("k").await.unwrap();
    let one = part(&rig, "k", &id, 1, "stray").await;
    rig.proxy
        .complete_upload("k", &id, &[(1, one)])
        .await
        .unwrap();
    rig.hot_mem
        .delete(&crate::entry::entry_key("k"))
        .await
        .unwrap();
    assert!(rig.entry("k").await.is_none());
    rig.clock.advance(600_001);
    let mut reconciler = reconciler(&rig);
    let first = reconciler.step().await;
    assert_eq!(first.strays_adopted, 1);
    settle(&mut reconciler).await;
    assert_eq!(rig.cold_mem.get("k", None).await.unwrap(), body("stray"));
}

/// A reconciler over the rig's backends (the hot one behind its fault injector) with `tweak`ed
/// settings.
fn sweeper(rig: &Rig, tweak: impl FnOnce(&mut EntryReconcilerConfig)) -> EntryReconciler {
    let mut base = ReconcilerConfig::new("r1", 1 << 20, rig.clock.clone());
    base.mono = rig.mono.clone();
    let mut config = EntryReconcilerConfig::new(base);
    tweak(&mut config);
    EntryReconciler::new(rig.hot.clone(), rig.cold_mem.clone(), config)
}

/// Asserts that the sweep aborts the uploads of the hot tier that have stood open longer than
/// `abandoned_upload_after` and no others: a young upload, an upload of the cold tier and an
/// upload of a reserved key are left, and the default setting, zero, aborts none (as S3 keeps an
/// incomplete upload until its client or a lifecycle rule aborts it).
#[tokio::test(start_paused = true)]
async fn the_sweep_aborts_hot_uploads_left_open_past_the_limit_only() {
    let rig = Rig::active().await;
    rig.clock.advance(1000);
    rig.proxy.create_upload("a").await.unwrap();
    rig.hot_mem.create_upload("_tiering/x").await.unwrap();
    rig.clock.advance(7 * 24 * 3600 * 1000);
    let young = rig.proxy.create_upload("young").await.unwrap();
    let cold = rig.cold_mem.create_upload("cold").await.unwrap();
    rig.clock.advance(1);

    let mut off = sweeper(&rig, |_| {});
    assert_eq!(off.step().await.uploads_aborted, 0);
    assert_eq!(
        rig.hot_mem.list_uploads("", None, 10).await.unwrap().len(),
        3
    );
    off.stop().await;
    let mut paged = sweeper(&rig, |config| {
        config.base.page_size = 1;
        config.abandoned_upload_after = std::time::Duration::from_secs(7 * 24 * 3600);
    });
    let report = paged.step().await;
    assert_eq!(report.uploads_aborted, 1);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert!(report.progress());
    let left = rig.hot_mem.list_uploads("", None, 10).await.unwrap();
    let keys: Vec<&str> = left.iter().map(|up| up.key.as_str()).collect();
    assert_eq!(keys, ["_tiering/x", "young"]);
    assert!(young.ends_with(&left[1].id));
    assert!(is_reserved(&left[0].key));
    let cold_left = rig.cold_mem.list_uploads("", None, 10).await.unwrap();
    assert_eq!(cold_left[0].id, cold);
}

/// Asserts that a failed abort or listing is a failure of the sweep, which is then due again at the
/// next step; that an upload that its client completed or aborted while the sweep ran (the abort is
/// answered not found) is not a failure; and that an upload whose start the hot tier does not give
/// is left.
#[tokio::test(start_paused = true)]
async fn the_sweep_reports_a_failed_call_and_ignores_a_vanished_upload() {
    let rig = Rig::active().await;
    // The memory backend dates an upload at its clock: zero is "unknown".
    rig.hot_mem.create_upload("unknown").await.unwrap();
    rig.clock.advance(1000);
    rig.hot_mem.create_upload("old").await.unwrap();
    rig.clock.advance(7 * 24 * 3600 * 1000 + 1);
    let mut sweeper = sweeper(&rig, |config| {
        config.abandoned_upload_after = std::time::Duration::from_secs(7 * 24 * 3600);
    });

    let down = Error::Unavailable("abort".to_owned());
    rig.hot_faults.fail_next_with(Op::AbortUpload, down.clone());
    assert_eq!(sweeper.step().await.failed, [down]);
    let down = Error::Unavailable("list".to_owned());
    rig.hot_faults.fail_next_with(Op::ListUploads, down.clone());
    assert_eq!(sweeper.step().await.failed, [down]);
    rig.hot_faults
        .fail_next_with(Op::AbortUpload, Error::NotFound("old".to_owned()));
    let report = sweeper.step().await;
    assert_eq!(report.uploads_aborted, 0);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        rig.hot_mem.list_uploads("", None, 10).await.unwrap().len(),
        2
    );
}

/// Asserts that a hot tier that does no multipart uploads has none to abort: the sweep is not a
/// failure.
#[tokio::test(start_paused = true)]
async fn the_sweep_of_a_hot_tier_without_uploads_is_not_a_failure() {
    let rig = Rig::active().await;
    let none = Error::Unsupported("no uploads".to_owned());
    rig.hot_faults.fail_next_with(Op::ListUploads, none);
    let report = sweeper(&rig, |config| {
        config.abandoned_upload_after = std::time::Duration::from_secs(7 * 24 * 3600);
    })
    .step()
    .await;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.uploads_aborted, 0);
}

/// Asserts that the listing of uploads leaves a marker alone whose key is outside the prefix, that
/// it lists no more than asked, and that a listing of no uploads asks no tier.
#[tokio::test]
async fn a_listing_after_a_cold_marker_outside_the_prefix_lists_the_prefix() {
    let rig = Rig::active().await;
    let hot = rig.proxy.create_upload("p/hot").await.unwrap();
    let cold = rig.cold_mem.create_upload("a").await.unwrap();
    let listed = rig
        .proxy
        .list_uploads("p/", Some(("a", Some(&cold))), 5)
        .await;
    let listed = listed.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, hot);
    let none = rig.proxy.list_uploads("", None, 0).await.unwrap();
    assert!(none.is_empty());
    let _ = Arc::clone(&rig.proxy);
}
