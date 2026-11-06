//! The puts of a proxy whose format says `active-marks`: where each kind of put lands, what it
//! leaves in the marks bucket, and which proxies make them at all.
//!
//! No test waits or asserts a duration: the monotonic clock is moved by hand and the sleeper moves
//! it.

use std::time::Duration;

use super::marked_put_rig::{BUDGET, MARKS_FORMAT, MarkRig, body, chunks, secs};
use crate::journal::FORMAT_KEY;
use crate::{
    Backend, Error, MarkedPutConfig, MarkerState, MemoryBackend, Op, Phase, Proxy, ProxyConfig,
    TestClock,
};
use std::sync::Arc;

/// Asserts that a put of a new key asks the hot tier, the cold tier and the marks bucket once
/// each, then writes the object (to the hot tier alone) and the marker (`DIRTY`, `gone` false,
/// owing a copy, owned by nobody) once each, and makes no other call; and that a put that took
/// no longer than `reassert_after` reads the marker no more.
#[tokio::test]
async fn a_put_of_a_new_key_asks_each_store_once_then_writes_the_object_and_the_marker() {
    let rig = MarkRig::new().await;
    rig.proxy.put("k", body("v")).await.unwrap();

    assert_eq!(rig.hot_ops("k"), [Op::Head, Op::Put]);
    assert_eq!(rig.cold_ops("k"), [Op::Head]);
    assert_eq!(rig.marks_ops("k"), [Op::Get, Op::Put]);
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("v"));
    assert!(!rig.cold_has("k").await);
    let marker = rig.marker("k").await.unwrap();
    assert_eq!(marker.st, MarkerState::Dirty);
    assert_eq!((marker.gone, marker.dirty), (false, true));
    assert_eq!((marker.why, marker.owner), (None, None));
}

/// Asserts that the object and the marker of a new key are written in parallel: with the object's
/// write held at the hot tier the marker is stored, and with the marker's write held the object is
/// stored. Neither waits for the other.
#[tokio::test]
async fn the_object_and_the_marker_of_a_new_key_are_written_in_parallel() {
    let rig = MarkRig::new().await;
    let held = rig.hot.gate(Op::Put, "k", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("k", body("v")).await });
    held.parked().await;
    assert!(rig.marker("k").await.is_some());
    assert!(!rig.hot_has("k").await);
    assert!(held.release());
    put.await.unwrap().unwrap();

    let held = rig.marks.gate(Op::Put, "j", Phase::Pre);
    let proxy = rig.proxy.clone();
    let put = tokio::spawn(async move { proxy.put("j", body("w")).await });
    held.parked().await;
    assert!(rig.hot_has("j").await);
    assert!(rig.marker("j").await.is_none());
    assert!(held.release());
    put.await.unwrap().unwrap();
    assert!(rig.marker("j").await.is_some());
}

/// Asserts that a streamed put lands in the hot tier whole with a marker, and that a presigned put
/// (a hot-only streamed put) does too: the cold tier is asked whether the key is there and is
/// never written.
#[tokio::test]
async fn a_streamed_put_and_a_presigned_put_land_in_hot_with_a_marker() {
    let rig = MarkRig::new().await;
    let streamed = rig.proxy.put_stream("s", Some(3), chunks(&["a", "bc"]));
    streamed.await.unwrap();
    let presigned = rig.proxy.put_stream_hot_only("p", Some(1), chunks(&["v"]));
    presigned.await.unwrap();
    let answered = rig
        .proxy
        .put_stream_answered("a", Some(1), chunks(&["w"]), false);
    answered.await.unwrap();

    assert_eq!(rig.hot_mem.get("s", None).await.unwrap(), body("abc"));
    assert_eq!(rig.hot_mem.get("p", None).await.unwrap(), body("v"));
    for key in ["s", "p", "a"] {
        assert_eq!(rig.marker(key).await.unwrap().st, MarkerState::Dirty);
        assert!(!rig.cold_has(key).await);
        assert_eq!(rig.cold_ops(key), [Op::Head]);
    }
}

/// Asserts that the level of the hot tier does not matter: a put at level `Full` is written to the
/// hot tier, with its marker, and nothing is written to the cold tier.
#[tokio::test]
async fn a_put_at_level_full_is_written_to_hot_by_the_fast_path() {
    let rig = MarkRig::new().await;
    rig.fill_hot().await;
    rig.proxy.put("held", body("v")).await.unwrap();
    let streamed = rig.proxy.put_stream("streamed", Some(1), chunks(&["w"]));
    streamed.await.unwrap();
    for key in ["held", "streamed"] {
        assert!(rig.hot_has(key).await);
        assert!(rig.marker(key).await.is_some());
        assert!(!rig.cold_has(key).await);
    }
}

/// Asserts that a put the hot store refuses with `Full` is `Full` (a 503 at the S3 front), not
/// written to the cold tier, and leaves no object and no marker: the marker it made is removed.
#[tokio::test]
async fn a_put_the_hot_store_refuses_with_full_is_full_and_leaves_nothing() {
    let rig = MarkRig::new().await;
    let full = || Error::Full("k".to_owned());
    rig.hot_faults.fail_next_on(Op::Put, "k", full());
    assert_eq!(rig.proxy.put("k", body("v")).await, Err(full()));
    assert!(!rig.hot_has("k").await);
    assert!(!rig.cold_has("k").await);
    assert!(rig.marker("k").await.is_none());

    rig.hot_faults
        .fail_next_on(Op::Put, "p", Error::Full("p".to_owned()));
    let presigned = rig.proxy.put_stream_hot_only("p", Some(1), chunks(&["v"]));
    assert_eq!(presigned.await, Err(Error::Full("p".to_owned())));
    assert!(!rig.hot_has("p").await);
    assert!(!rig.cold_has("p").await);
    assert!(rig.marker("p").await.is_none());
}

/// Asserts that the completion of a multipart upload of a new key makes the object in the hot tier
/// by the tier's create-only completion, with a marker, and writes nothing to the cold tier.
#[tokio::test]
async fn a_completion_of_a_new_key_takes_the_fast_path() {
    let rig = MarkRig::new().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    let tag = rig.proxy.upload_part("k", &id, 1, chunks(&["ab"])).await;
    let done = rig
        .proxy
        .complete_upload("k", &id, &[(1, tag.unwrap())])
        .await;
    done.unwrap();

    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("ab"));
    assert_eq!(rig.marker("k").await.unwrap().st, MarkerState::Dirty);
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.cold_ops("k"), [Op::Head]);
}

/// Asserts that a completion the hot tier refuses for good (a part it does not know) is answered
/// with the tier's error and removes the marker it made, by a change to `CLEARING` and a delete.
#[tokio::test]
async fn a_failed_completion_removes_its_marker() {
    let rig = MarkRig::new().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    let tag = rig.proxy.upload_part("k", &id, 1, chunks(&["ab"])).await;
    let invalid = Error::InvalidPart("1".to_owned());
    rig.hot_faults
        .fail_next_with(Op::CompleteUpload, invalid.clone());
    let done = rig
        .proxy
        .complete_upload("k", &id, &[(1, tag.unwrap())])
        .await;

    assert_eq!(done, Err(invalid));
    assert!(rig.marker("k").await.is_none());
    assert!(!rig.hot_has("k").await);
    assert_eq!(
        rig.marks_ops("k"),
        [Op::Get, Op::Put, Op::PutIfMatch, Op::Delete]
    );
}

/// Asserts that while the format says `active-marks` an upload is begun in the hot tier whatever
/// the level, and that an upload begun in the cold tier, by the proxy of a release that had no
/// room in the hot tier, is not completed (nor given parts) on markers: 503, nothing written, and
/// the upload is left open to be aborted.
#[tokio::test]
async fn an_upload_begun_in_cold_is_not_completed_on_markers() {
    let rig = MarkRig::new().await;
    rig.fill_hot().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    assert!(id.starts_with("ts-hot~"), "{id}");

    let cold_id = rig.cold_mem.create_upload("c").await.unwrap();
    let part = rig
        .proxy
        .upload_part("c", &cold_id, 1, chunks(&["x"]))
        .await;
    assert!(matches!(part, Err(Error::Unavailable(_))), "{part:?}");
    let done = rig.proxy.complete_upload("c", &cold_id, &[]).await;
    assert!(matches!(done, Err(Error::Unavailable(_))), "{done:?}");
    assert!(!rig.cold_has("c").await);
    assert!(!rig.hot_has("c").await);
    assert!(rig.marker("c").await.is_none());
    assert_eq!(rig.marks_ops("c"), []);
    rig.proxy.abort_upload("c", &cold_id).await.unwrap();
}

/// Asserts that a put is refused with 503 and touches nothing, marker included, when the format
/// says `active-marks` with a marks bucket other than this proxy's, when it records none, when this
/// proxy has no marks bucket or no name for it, and when there is no format object.
#[tokio::test]
async fn a_put_is_refused_unless_the_format_records_the_marks_bucket_this_proxy_has() {
    let other = r#"{"epoch":3,"marks_bucket":"another","phase":"active-marks"}"#;
    let none = r#"{"epoch":3,"phase":"active-marks"}"#;
    for format in [other, none, ""] {
        let rig = MarkRig::with_format(format).await;
        let put = rig.proxy.put("k", body("v")).await;
        assert!(
            matches!(put, Err(Error::Unavailable(_))),
            "{format}: {put:?}"
        );
        assert!(!rig.hot_has("k").await);
        assert!(!rig.cold_has("k").await);
        assert_eq!(rig.marks_ops("k"), []);
        assert_eq!(rig.hot_ops("k"), []);
    }

    let nameless = MarkRig::tuned(MARKS_FORMAT, |config| config.marks_name = None).await;
    let put = nameless.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    let marksless = MarkRig::tuned(MARKS_FORMAT, |config| config.marks = None).await;
    let put = marksless.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    let upload = marksless.proxy.create_upload("k").await;
    assert!(matches!(upload, Err(Error::Unavailable(_))), "{upload:?}");
    assert!(!marksless.hot_has("k").await);
}

/// Asserts that a put whose proxy is not live (its registration has not been heard of for the
/// departure time) is 503 and asks no store.
#[tokio::test]
async fn a_put_by_a_proxy_that_is_not_live_is_refused_and_asks_no_store() {
    let rig = MarkRig::new().await;
    rig.mono.advance(secs(3600));
    let put = rig.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert_eq!(rig.hot_ops("k"), []);
    assert_eq!(rig.marks_ops("k"), []);
    assert_eq!(rig.cold_ops("k"), []);
}

/// Asserts that a reserved key is refused, and that writes stay paused under `drain` and are 503
/// under a phase that is not one of the protocols (`active` below epoch 2).
#[tokio::test]
async fn a_put_of_a_reserved_key_or_during_a_drain_is_refused() {
    let rig = MarkRig::new().await;
    let reserved = rig.proxy.put("_tiering/x", body("v")).await;
    assert_eq!(reserved, Err(Error::Reserved("_tiering/x".to_owned())));
    for format in [r#"{"phase":"drain"}"#, r#"{"epoch":1,"phase":"active"}"#] {
        let rig = MarkRig::with_format(format).await;
        let put = rig.proxy.put("k", body("v")).await;
        assert!(
            matches!(put, Err(Error::Unavailable(_))),
            "{format}: {put:?}"
        );
        assert_eq!(rig.marks_ops("k"), []);
    }
}

/// Asserts that under the format that runs entries a put is made on its entry as it was, and the
/// marks bucket is not called: with `active-marks` not in force nothing of a running farm changes.
#[tokio::test]
async fn a_put_under_the_active_format_is_made_on_its_entry_and_calls_no_marker() {
    let rig = MarkRig::with_format(r#"{"epoch":2,"phase":"active"}"#).await;
    rig.proxy.put("k", body("v")).await.unwrap();
    assert!(rig.hot_has("k").await);
    assert!(rig.hot_has(&crate::entry_key("k")).await);
    assert_eq!(rig.marks_ops("k"), []);
    assert!(rig.marks_mem.list("").await.unwrap().is_empty());
}

/// Asserts that a delete under `active-marks` is still refused with 503 and changes nothing: no
/// delete is made on markers yet.
#[tokio::test]
async fn a_delete_under_active_marks_is_refused_and_changes_nothing() {
    let rig = MarkRig::new().await;
    rig.hot_mem.put("k", body("v")).await.unwrap();
    let deleted = rig.proxy.delete("k").await;
    assert!(matches!(deleted, Err(Error::Unavailable(_))), "{deleted:?}");
    assert!(rig.hot_has("k").await);
    assert_eq!(rig.marks_ops("k"), []);
    assert_eq!(rig.cold_ops("k"), []);
}

/// Asserts that the format object that names the bucket is read the way any write reads it: a
/// format that changes from `active-marks` to the entries between two puts moves the second put to
/// its entry, and the first put's marker stays.
#[tokio::test]
async fn a_put_follows_the_format_it_reads_when_it_is_made() {
    let rig = MarkRig::new().await;
    rig.proxy.put("one", body("1")).await.unwrap();
    let format = r#"{"epoch":2,"phase":"active"}"#;
    rig.hot_mem.delete(FORMAT_KEY).await.unwrap();
    rig.hot_mem.put(FORMAT_KEY, body(format)).await.unwrap();
    rig.proxy.put("two", body("2")).await.unwrap();
    assert!(rig.marker("one").await.is_some());
    assert!(rig.marker("two").await.is_none());
    assert!(rig.hot_has(&crate::entry_key("two")).await);
}

/// Asserts the checks of the settings of the puts: the three times are more than zero, and the
/// waits of a lock are those the markers accept; a proxy with a setting that is refused is not
/// opened.
#[tokio::test]
async fn settings_that_do_not_hold_together_are_refused_when_the_proxy_is_opened() {
    let zero = Duration::ZERO;
    assert_eq!(MarkedPutConfig::default().check(), Ok(()));
    let names = ["marker_deadline", "reassert_after", "read_window"];
    for name in names {
        let mut settings = MarkedPutConfig::default();
        match name {
            "marker_deadline" => settings.marker_deadline = zero,
            "reassert_after" => settings.reassert_after = zero,
            _ => settings.read_window = zero,
        }
        let refused = settings.check().unwrap_err();
        assert_eq!(refused, format!("{name} must be more than zero"));
    }

    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let mut config = ProxyConfig::new("p1", BUDGET, clock);
    config.marks = Some(Arc::new(MemoryBackend::new()));
    config.marks_put.markers.lock_poll_factor = 0;
    let opened = Proxy::open(hot.clone(), cold.clone(), config.clone()).await;
    assert!(matches!(opened, Err(Error::Other(_))), "{:?}", opened.err());
    config.marks_put = MarkedPutConfig::default();
    config.marks_put.read_window = zero;
    let opened = Proxy::open(hot, cold, config).await;
    assert!(matches!(opened, Err(Error::Other(_))), "{:?}", opened.err());
}
