//! The reads of a proxy that miss the hot tier where markers may be in force: the pair of the
//! marker and the cold read, the answer a `gone` marker gives, the phases that read markers, the
//! window of a pair, and the ask for a copy-back.
//!
//! The tiers are memory backends behind fault injectors and gates (`mark_rig.rs`). A test holds a
//! call with a gate to put a change between two calls of a read, and moves the monotonic clock by
//! hand: nothing here sleeps and no assertion is about elapsed time.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use super::super::entry_put_tests::body;
use super::super::mark_rig::{ACTIVE_MARKS, DRAINED, ENTRIES, MarkRig};
use super::{MarkReads, Wants};
use crate::entry::{Entries, EntryOp};
use crate::journal::FORMAT_KEY;
use crate::{Backend, ByteRange, Error, MarkerState, Op, Phase, Proxy, ReadBody, Span, TestClock};

/// The ways to read a key: `open_read`, `get`, `get_stream` and `head`.
const HOW: std::ops::Range<usize> = 0..4;

fn kind(how: usize) -> Op {
    if how < 3 { Op::Get } else { Op::Head }
}

/// What a read of `how` shows of an object whose bytes are `text`: the bytes, or for a head its
/// size.
fn shown(how: usize, text: &str) -> Vec<u8> {
    if how < 3 {
        text.as_bytes().to_vec()
    } else {
        format!("{} bytes", text.len()).into_bytes()
    }
}

async fn collect(mut stream: ReadBody) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk?);
    }
    Ok(bytes)
}

async fn read_from(proxy: &Proxy, key: &str, how: usize) -> Result<Vec<u8>, Error> {
    match how {
        0 => collect(proxy.open_read(key, None).await?.body).await,
        1 => Ok(proxy.get(key, None).await?.to_vec()),
        2 => collect(proxy.get_stream(key, None).await?).await,
        _ => Ok(format!("{} bytes", proxy.head(key).await?.size).into_bytes()),
    }
}

async fn read(rig: &MarkRig, key: &str, how: usize) -> Result<Vec<u8>, Error> {
    read_from(&rig.proxy, key, how).await
}

/// Lets the tasks that are ready run.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn not_found(key: &str) -> Result<Vec<u8>, Error> {
    Err(Error::NotFound(key.to_owned()))
}

/// Asserts that a read the hot tier answers is one request to it, and reads no marker, no cold
/// object and no format object, though the marks bucket holds a `gone` marker of the key.
#[tokio::test]
async fn a_hot_hit_is_one_request_and_reads_no_marker() {
    for how in HOW {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        rig.put_hot("k", "hot").await;
        rig.put_cold("k", "cold-copy").await;
        rig.mark("k", MarkerState::Dirty, true).await;
        assert_eq!(read(&rig, "k", how).await, Ok(shown(how, "hot")), "{how}");
        assert_eq!(rig.hot_faults.calls_on(kind(how), "k"), 1, "{how}");
        assert_eq!(rig.hot_faults.calls_on(Op::Get, FORMAT_KEY), 0, "{how}");
        assert_eq!(rig.marks_faults.calls(Op::Get), 0, "{how}");
        assert_eq!(rig.cold_faults.calls(kind(how)), 0, "{how}");
    }
}

/// Asserts that a read the hot tier misses reads the marker of the key and the cold object, one
/// request each, and that with no marker the cold object is the answer, by every kind of read.
#[tokio::test]
async fn a_hot_miss_reads_the_marker_beside_the_cold_object_and_serves_cold_when_there_is_none() {
    for how in HOW {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        rig.put_cold("k", "cold").await;
        assert_eq!(read(&rig, "k", how).await, Ok(shown(how, "cold")), "{how}");
        assert_eq!(rig.hot_faults.calls_on(kind(how), "k"), 1, "{how}");
        assert_eq!(rig.marks_faults.calls_on(Op::Get, "k"), 1, "{how}");
        assert_eq!(rig.cold_faults.calls_on(kind(how), "k"), 1, "{how}");
        assert_eq!(rig.marker("k").await, None, "{how}");
    }
}

/// Asserts that a key neither tier has, with no marker, is not found after the two reads.
#[tokio::test]
async fn a_key_neither_tier_has_is_not_found_after_the_pair() {
    for how in HOW {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        assert_eq!(read(&rig, "k", how).await, not_found("k"), "{how}");
        assert_eq!(rig.marks_faults.calls_on(Op::Get, "k"), 1, "{how}");
    }
}

/// Asserts that a `gone` marker makes the key not found though the cold tier holds the object,
/// and that the hot tier is read once more first (two reads of it in all), by every kind of read.
#[tokio::test]
async fn a_gone_marker_hides_the_cold_object_after_one_more_hot_read() {
    for how in HOW {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        rig.put_cold("k", "cold").await;
        rig.mark("k", MarkerState::Dirty, true).await;
        assert_eq!(read(&rig, "k", how).await, not_found("k"), "{how}");
        assert_eq!(rig.hot_faults.calls_on(kind(how), "k"), 2, "{how}");
        assert_eq!(rig.cold_faults.calls_on(kind(how), "k"), 1, "{how}");
    }
}

/// Asserts that a read whose marker said `gone` serves a put that landed in the hot tier after the
/// first miss and before the marker was read, not absent.
#[tokio::test]
async fn a_read_that_finds_gone_heads_hot_once_more_and_serves_a_put_that_landed_since() {
    for how in HOW {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        rig.put_cold("k", "old").await;
        rig.mark("k", MarkerState::Dirty, true).await;
        let gate = rig.marks.gate(Op::Get, "k", Phase::Post);
        let proxy = Arc::clone(&rig.proxy);
        let task = tokio::spawn(async move { read_from(&proxy, "k", how).await });
        gate.parked().await;
        rig.put_hot("k", "new!").await;
        assert!(gate.release());
        assert_eq!(task.await.unwrap(), Ok(shown(how, "new!")), "{how}");
        assert_eq!(rig.hot_faults.calls_on(kind(how), "k"), 2, "{how}");
    }
}

/// Asserts that a marker that is not `gone` leaves the cold object as the answer whatever its
/// state, a lock and a clearing included: the read polls nothing, sleeps nothing and writes
/// nothing, so it never waits (I12).
#[tokio::test]
async fn a_read_never_waits_on_a_lock_and_serves_cold_for_any_marker_that_is_not_gone() {
    let states = [
        MarkerState::Dirty,
        MarkerState::Lock,
        MarkerState::Clearing,
        MarkerState::Want,
        MarkerState::Other("LATER".to_owned()),
    ];
    for st in states {
        for how in HOW {
            let rig = MarkRig::new(ACTIVE_MARKS).await;
            rig.put_cold("k", "cold").await;
            rig.mark("k", st.clone(), false).await;
            let before = rig.marker("k").await;
            assert_eq!(read(&rig, "k", how).await, Ok(shown(how, "cold")), "{st:?}");
            assert_eq!(rig.marks_faults.calls_on(Op::Get, "k"), 1, "{st:?}");
            assert_eq!(rig.marks_faults.calls(Op::Put), 0, "{st:?}");
            assert_eq!(rig.sleeper.asked(), Vec::<Duration>::new(), "{st:?}");
            assert_eq!(rig.marker("k").await, before, "{st:?}");
        }
    }
}

/// Asserts which phases of the format object read the marker on a hot miss: those from which a
/// marker can be in force or still be run to completion (`drain`, `drained`, `active-marks`, a
/// phase of a later release), and not the entry protocol's `active`, an `active` below epoch 2, or
/// a missing object, where the marks bucket is not asked at all.
#[tokio::test]
async fn the_marker_is_read_in_each_phase_from_which_markers_can_be_in_force_and_in_no_other() {
    let reads = [
        r#"{"phase":"drain"}"#,
        DRAINED,
        ACTIVE_MARKS,
        r#"{"phase":"someday"}"#,
    ];
    for format in reads {
        let rig = MarkRig::new(format).await;
        rig.put_cold("k", "cold").await;
        rig.mark("k", MarkerState::Dirty, true).await;
        assert_eq!(
            rig.proxy.get("k", None).await,
            Err(Error::NotFound("k".into()))
        );
        assert_eq!(rig.marks_faults.calls_on(Op::Get, "k"), 1, "{format}");
    }
    let quiet = [ENTRIES, r#"{"epoch":1,"phase":"active"}"#, ""];
    for format in quiet {
        let rig = MarkRig::new(format).await;
        rig.put_cold("k", "cold").await;
        rig.mark("k", MarkerState::Dirty, true).await;
        assert_eq!(rig.proxy.get("k", None).await, Ok(body("cold")), "{format}");
        assert_eq!(rig.marks_faults.calls(Op::Get), 0, "{format}");
    }
}

/// Asserts that a proxy that has never read the format object reads markers (a deleted key is not
/// served from cold because the format could not be seen), and that after a read that succeeded
/// the phase it saw is kept when a later read fails.
#[tokio::test]
async fn a_format_that_cannot_be_read_does_not_hide_a_deleted_key_and_the_last_phase_is_kept() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.put_cold("k", "cold").await;
    rig.mark("k", MarkerState::Dirty, true).await;
    let down = || Error::Unavailable("format down".to_owned());
    rig.hot_faults.fail_next_on(Op::Get, FORMAT_KEY, down());
    assert_eq!(
        rig.proxy.get("k", None).await,
        Err(Error::NotFound("k".into()))
    );
    assert_eq!(rig.marks_faults.calls_on(Op::Get, "k"), 1);

    rig.set_format(ENTRIES).await;
    assert_eq!(rig.proxy.get("k", None).await, Ok(body("cold")));
    rig.hot_faults.fail_next_on(Op::Get, FORMAT_KEY, down());
    assert_eq!(rig.proxy.get("k", None).await, Ok(body("cold")));
    assert_eq!(rig.marks_faults.calls_on(Op::Get, "k"), 1);
}

/// Asserts that a marker that cannot be read fails the read closed, though the cold tier answered:
/// a `gone` marker may be behind it.
#[tokio::test]
async fn a_hot_miss_whose_marker_cannot_be_read_fails_closed() {
    for how in HOW {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        rig.put_cold("k", "cold").await;
        let down = Error::Unavailable("marks down".to_owned());
        rig.marks_faults.fail_next_on(Op::Get, "k", down.clone());
        assert_eq!(read(&rig, "k", how).await, Err(down), "{how}");
    }
}

/// Asserts that the cold tier's error is the answer when the marker is not `gone`, and that a
/// `gone` marker answers not found whatever the cold tier did: the key is deleted, cold or not.
#[tokio::test]
async fn a_cold_error_is_the_answer_unless_the_marker_says_the_key_is_gone() {
    for how in HOW {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        let down = Error::Unavailable("cold down".to_owned());
        rig.cold_faults.fail_next_on(kind(how), "k", down.clone());
        assert_eq!(read(&rig, "k", how).await, Err(down.clone()), "{how}");

        rig.mark("k", MarkerState::Dirty, true).await;
        rig.cold_faults.fail_next_on(kind(how), "k", down);
        assert_eq!(read(&rig, "k", how).await, not_found("k"), "{how}");
    }
}

/// Asserts that a read in the entry protocol's phase makes no request to the marks bucket, though
/// the proxy has one and the bucket holds a `gone` marker, and still asks for its copy-back on an
/// entry, as before.
#[tokio::test]
async fn in_the_entry_protocol_a_hot_miss_reads_no_marker_and_asks_for_its_copy_back_on_an_entry() {
    let rig = MarkRig::new(ENTRIES).await;
    rig.put_cold("k", "cold").await;
    rig.mark("k", MarkerState::Dirty, true).await;
    assert_eq!(rig.proxy.get("k", None).await, Ok(body("cold")));
    settle().await;
    let entries = Entries::new(rig.hot_mem.clone(), "other", Arc::new(TestClock::new()));
    let entry = entries.read("k").await.unwrap().unwrap().entry;
    assert_eq!(entry.op, EntryOp::Cb);
    assert_eq!(rig.marks_faults.call_log(), vec![]);
}

/// Asserts that a pair that took longer than `read_window` is dropped and read again, so that a
/// delete that came while the cold read was held is seen by the second marker read, and the
/// cold object the first pair read is not served.
#[tokio::test]
async fn a_read_pair_slower_than_the_window_is_read_again_and_sees_a_delete_that_came_between() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.put_cold("k", "cold").await;
    let gate = rig.cold.gate(Op::Get, "k", Phase::Post);
    let proxy = Arc::clone(&rig.proxy);
    let task = tokio::spawn(async move { proxy.get("k", None).await });
    gate.parked().await;
    rig.mark("k", MarkerState::Dirty, true).await;
    rig.mono.advance(Duration::from_millis(5001));
    assert!(gate.release());
    assert_eq!(task.await.unwrap(), Err(Error::NotFound("k".into())));
    assert_eq!(rig.marks_faults.calls_on(Op::Get, "k"), 2);
    assert_eq!(rig.cold_faults.calls_on(Op::Get, "k"), 2);
}

/// Asserts that a pair that took exactly `read_window` is taken: only a longer one is read again.
#[tokio::test]
async fn a_read_pair_that_took_exactly_the_window_is_taken() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.put_cold("k", "cold").await;
    let gate = rig.cold.gate(Op::Get, "k", Phase::Post);
    let proxy = Arc::clone(&rig.proxy);
    let task = tokio::spawn(async move { proxy.get("k", None).await });
    gate.parked().await;
    rig.mono.advance(Duration::from_millis(5000));
    assert!(gate.release());
    assert_eq!(task.await.unwrap(), Ok(body("cold")));
    assert_eq!(rig.marks_faults.calls_on(Op::Get, "k"), 1);
}

/// Asserts that a read whose every pair is slower than `read_window` is answered unavailable (503)
/// after `read_retries` more pairs, naming the key and the numbers, and does not loop.
#[tokio::test]
async fn a_read_pair_slower_than_the_window_is_503_after_read_retries() {
    let rig = MarkRig::tuned(ACTIVE_MARKS, |config| config.mark_reads.read_retries = 1).await;
    rig.put_cold("k", "cold").await;
    let gates = [
        rig.cold.gate(Op::Get, "k", Phase::Post),
        rig.cold.gate(Op::Get, "k", Phase::Post),
    ];
    let proxy = Arc::clone(&rig.proxy);
    let task = tokio::spawn(async move { proxy.get("k", None).await });
    for gate in &gates {
        gate.parked().await;
        rig.mono.advance(Duration::from_secs(6));
        assert!(gate.release());
    }
    let why = "the hot and cold tiers answered a read of k further apart than read_window (5000 \
               ms), 2 times";
    assert_eq!(task.await.unwrap(), Err(Error::Unavailable(why.to_owned())));
    assert_eq!(rig.marks_faults.calls_on(Op::Get, "k"), 2);
    assert_eq!(rig.cold_faults.calls_on(Op::Get, "k"), 2);
}

/// Reads `key` whole and lets the ask for its copy-back run, which is held at the write of its
/// marker so that the test knows the write was made.
async fn read_and_settle(rig: &MarkRig, key: &str, how: usize) {
    assert!(read(rig, key, how).await.is_ok());
    settle().await;
}

/// Asserts that a whole read of a key only the cold tier has makes the `WANT` marker of the key,
/// create-only, from the phase `active-marks`, and that a proxy asks for a key again only after
/// `want_retry`: reads within it make no second create though the marker was cleared meanwhile.
#[tokio::test]
async fn a_whole_read_of_a_cold_only_key_creates_want_once_in_each_want_retry() {
    for how in 0..3 {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        rig.put_cold("k", "cold").await;
        read_and_settle(&rig, "k", how).await;
        let marker = rig.marker("k").await.unwrap();
        assert_eq!(
            (
                marker.st,
                marker.gone,
                marker.dirty,
                marker.why,
                marker.owner
            ),
            (MarkerState::Want, false, false, None, None),
            "{how}"
        );
        assert_eq!(rig.marks_faults.calls_on(Op::Put, "k"), 1, "{how}");

        rig.marks_mem.delete("k").await.unwrap();
        rig.mono.advance(Duration::from_millis(59_999));
        read_and_settle(&rig, "k", how).await;
        assert_eq!(rig.marker("k").await, None, "{how}");
        assert_eq!(rig.marks_faults.calls_on(Op::Put, "k"), 1, "{how}");

        rig.mono.advance(Duration::from_millis(1));
        read_and_settle(&rig, "k", how).await;
        assert_eq!(
            rig.marker("k").await.unwrap().st,
            MarkerState::Want,
            "{how}"
        );
        assert_eq!(rig.marks_faults.calls_on(Op::Put, "k"), 2, "{how}");
    }
}

/// Asserts that a read asks for no copy-back of an object above `copy_back_max_bytes`, and asks
/// for one at that size.
#[tokio::test]
async fn an_object_above_copy_back_max_bytes_is_not_asked_for() {
    let tune = |config: &mut crate::ProxyConfig| config.mark_reads.copy_back_max_bytes = 4;
    let rig = MarkRig::tuned(ACTIVE_MARKS, tune).await;
    rig.put_cold("four", "1234").await;
    rig.put_cold("five", "12345").await;
    read_and_settle(&rig, "five", 1).await;
    read_and_settle(&rig, "four", 1).await;
    assert_eq!(rig.marker("five").await, None);
    assert_eq!(rig.marker("four").await.unwrap().st, MarkerState::Want);
}

/// Asserts that a read asks for no copy-back while the hot tier is at its critical level.
#[tokio::test]
async fn no_copy_back_is_asked_for_while_the_hot_tier_is_critical() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.fill_hot().await;
    rig.put_cold("k", "cold").await;
    read_and_settle(&rig, "k", 1).await;
    assert_eq!(rig.marks_faults.calls(Op::Put), 0);
}

/// Asserts that a head, a read of a part and a read in a phase where markers are not in force
/// (`drained`) ask for no copy-back.
#[tokio::test]
async fn a_head_a_ranged_read_and_a_read_before_activation_ask_for_no_copy_back() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.put_cold("k", "cold").await;
    read_and_settle(&rig, "k", 3).await;
    let part = rig.proxy.get("k", Some(ByteRange::new(1, 2))).await;
    assert_eq!(part, Ok(body("ol")));
    let span = Span::From {
        first: 1,
        last: Some(2),
    };
    let opened = rig.proxy.open_read("k", Some(span)).await.unwrap();
    assert_eq!(collect(opened.body).await, Ok(b"ol".to_vec()));
    settle().await;
    assert_eq!(rig.marks_faults.calls(Op::Put), 0);

    rig.set_format(DRAINED).await;
    read_and_settle(&rig, "k", 1).await;
    assert_eq!(rig.marks_faults.calls(Op::Put), 0);
}

/// Asserts that a key that has a marker already (a lock, here) is not asked for, and that a
/// create that fails is ignored: the read is served, and the proxy does not try again within
/// `want_retry`.
#[tokio::test]
async fn a_marked_key_is_not_asked_for_and_a_failed_ask_is_ignored() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.put_cold("held", "cold").await;
    rig.mark("held", MarkerState::Lock, false).await;
    read_and_settle(&rig, "held", 1).await;
    assert_eq!(rig.marks_faults.calls_on(Op::Put, "held"), 0);

    rig.put_cold("k", "cold").await;
    let down = Error::Unavailable("marks down".to_owned());
    rig.marks_faults.fail_next_on(Op::Put, "k", down);
    read_and_settle(&rig, "k", 1).await;
    read_and_settle(&rig, "k", 1).await;
    assert_eq!(rig.marker("k").await, None);
    assert_eq!(rig.marks_faults.calls_on(Op::Put, "k"), 1);
}

/// Asserts the bounds of the set of keys asked for: a key within `want_retry` is refused, a full
/// set of asks that are all within it refuses a new key, the set is cleaned of the others at most
/// once in `want_retry`, a key whose ask is older is asked again even when the set is full.
#[test]
fn the_set_of_asks_is_bounded_and_cleaned_at_most_once_in_want_retry() {
    let config = MarkReads {
        want_recent_limit: 2,
        want_retry: Duration::from_secs(10),
        ..MarkReads::default()
    };
    let wants = Wants::default();
    assert!(wants.allow("a", 0, &config));
    assert!(!wants.allow("a", 9_999, &config));
    assert!(wants.allow("b", 1, &config));
    // Full of asks within `want_retry`: refused, and the set was cleaned at 2.
    assert!(!wants.allow("c", 2, &config));
    // Cleaned at 2, so not cleaned again at 3.
    assert!(!wants.allow("d", 3, &config));
    // `a` was asked 10 s ago: asked again though the set is full.
    assert!(wants.allow("a", 10_000, &config));
    // `b` is 9.999 s old and the set was cleaned 9.998 s ago: refused.
    assert!(!wants.allow("e", 10_001, &config));
    // The last cleaning is 10 s old, and `b` is past `want_retry`: cleaned, and `e` fits.
    assert!(wants.allow("e", 10_002, &config));
}

/// Asserts that a proxy whose marker settings do not hold together (a first poll of zero, here) does
/// not open, and says which setting, rather than reading markers with a wait that cannot end.
#[tokio::test]
async fn a_proxy_whose_marker_settings_do_not_hold_together_does_not_open() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    let mut config = crate::ProxyConfig::new("p2", 1000, Arc::new(TestClock::new()));
    config.marks = Some(rig.marks.clone());
    config.markers.lock_poll_min = Duration::ZERO;
    let opened = Proxy::open(rig.hot_mem.clone(), rig.cold_mem.clone(), config).await;
    let why = "invalid marker settings: lock_poll_min must be more than zero";
    assert_eq!(opened.err(), Some(Error::Other(why.to_owned())));
}
