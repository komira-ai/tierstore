//! The listings of a proxy where markers may be in force: the keys the cold tier lists that a
//! marker says are deleted, the page of marker names that covers the merged page, the rounds that
//! fill a page, the window of the reads after the cold page, and the failures.
//!
//! The tiers are memory backends behind fault injectors and gates (`mark_rig.rs`); a test holds
//! the cold tier's page to change the stores between the page and the reads that follow it, and
//! moves the monotonic clock by hand. Nothing here sleeps.

use std::sync::Arc;
use std::time::Duration;

use super::super::mark_rig::{ACTIVE_MARKS, ENTRIES, MarkRig};
use crate::{Backend, Error, Listed, MarkerState, MarkerWhy, Op, Phase, ProxyConfig};

fn names(listed: &[Listed]) -> Vec<&str> {
    listed.iter().map(|object| object.key.as_str()).collect()
}

async fn page(
    rig: &MarkRig,
    prefix: &str,
    after: Option<&str>,
    max_keys: usize,
) -> Result<Vec<Listed>, Error> {
    rig.proxy.list_page(prefix, after, max_keys).await
}

/// Puts the keys of `keys` in the cold tier.
async fn cold_keys(rig: &MarkRig, keys: &[&str]) {
    for key in keys {
        rig.put_cold(key, "cold").await;
    }
}

/// Asserts that a listing leaves out a key only the cold tier lists and a marker says is deleted,
/// reads the marker of each cold-only key the marker names list and of no other, and asks for one
/// page of marker names, by the page and by the whole listing.
#[tokio::test]
async fn a_listing_drops_a_cold_only_key_that_has_a_gone_marker() {
    for whole in [false, true] {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        cold_keys(&rig, &["a", "b", "c"]).await;
        rig.mark("b", MarkerState::Dirty, true).await;
        rig.mark("c", MarkerState::Want, false).await;
        let listed = if whole {
            rig.proxy.list("").await
        } else {
            page(&rig, "", None, 10).await
        };
        assert_eq!(names(&listed.unwrap()), ["a", "c"], "{whole}");
        let reads = ["a", "b", "c"].map(|key| rig.marks_faults.calls_on(Op::Get, key));
        assert_eq!(reads, [0, 1, 1], "{whole}");
        assert_eq!(rig.marks_faults.calls_on(Op::List, ""), 1, "{whole}");
    }
}

/// Asserts that a key the cold tier lists is kept when its marker is a lock that is not `gone`,
/// as an eviction takes while it deletes the hot object.
#[tokio::test]
async fn a_listing_keeps_a_cold_only_key_that_an_eviction_is_deleting() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["e"]).await;
    rig.mark_lock("e", MarkerWhy::Evict).await;
    assert_eq!(names(&page(&rig, "", None, 10).await.unwrap()), ["e"]);
}

/// Asserts that a key the hot page lists is listed from the hot tier and its marker is not read,
/// though the cold tier lists it too and the marker says `gone` (a recreate before the copier
/// replaced cold).
#[tokio::test]
async fn a_listing_reads_no_marker_of_a_key_the_hot_page_lists() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.put_hot("k", "hot!").await;
    rig.put_cold("k", "c").await;
    rig.mark("k", MarkerState::Dirty, true).await;
    let listed = page(&rig, "", None, 10).await.unwrap();
    assert_eq!(names(&listed), ["k"]);
    assert_eq!(listed[0].size, 4);
    assert_eq!(rig.marks_faults.calls(Op::Get), 0);
}

/// Asserts that a key the marker says is deleted is listed, from the hot tier, when a put landed in
/// the hot tier after the hot page was read.
#[tokio::test]
async fn a_cold_only_gone_key_is_listed_from_hot_when_a_put_landed_since() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.put_cold("k", "old").await;
    rig.mark("k", MarkerState::Dirty, true).await;
    let gate = rig.cold.gate(Op::List, "", Phase::Post);
    let proxy = Arc::clone(&rig.proxy);
    let task = tokio::spawn(async move { proxy.list_page("", None, 10).await });
    gate.parked().await;
    rig.put_hot("k", "new!").await;
    assert!(gate.release());
    let listed = task.await.unwrap().unwrap();
    let hot = rig.hot_mem.head("k").await.unwrap();
    assert_eq!(
        listed,
        [Listed {
            key: "k".to_owned(),
            size: 4,
            modified: hot.modified,
            tag: hot.tag,
        }]
    );
}

/// Asserts that a marker that is in the page of names and gone by the time it is read is decided
/// by a HEAD of the cold tier: kept if the cold tier still has the object, dropped if it does not.
#[tokio::test]
async fn a_listed_marker_that_vanishes_is_decided_by_the_cold_head() {
    for cold_keeps_it in [true, false] {
        let rig = MarkRig::new(ACTIVE_MARKS).await;
        rig.put_cold("k", "cold").await;
        rig.mark("k", MarkerState::Dirty, false).await;
        let gate = rig.cold.gate(Op::List, "", Phase::Post);
        let proxy = Arc::clone(&rig.proxy);
        let task = tokio::spawn(async move { proxy.list_page("", None, 10).await });
        gate.parked().await;
        rig.marks_mem.delete("k").await.unwrap();
        if !cold_keeps_it {
            rig.cold_mem.delete("k").await.unwrap();
        }
        assert!(gate.release());
        let listed = task.await.unwrap().unwrap();
        let kept: &[&str] = if cold_keeps_it { &["k"] } else { &[] };
        assert_eq!(names(&listed), kept, "{cold_keeps_it}");
        assert_eq!(
            rig.cold_faults.calls_on(Op::Head, "k"),
            1,
            "{cold_keeps_it}"
        );
    }
}

/// Asserts that the page of marker names is asked for with the client's own prefix and `after`:
/// names that sort before `after`, or outside the prefix, would stop short of the merged page and
/// ask for more, and here one request is enough.
#[tokio::test]
async fn a_listing_prefixes_the_after_key_for_the_marker_page() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    let keys = ["k0", "k1", "k2", "k3", "k4", "k5"];
    cold_keys(&rig, &keys).await;
    for key in keys {
        rig.mark(key, MarkerState::Want, false).await;
    }
    let listed = page(&rig, "", Some("k1"), 2).await.unwrap();
    assert_eq!(names(&listed), ["k2", "k3"]);
    assert_eq!(rig.marks_faults.calls_on(Op::List, ""), 1);

    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["p/1", "p/2"]).await;
    for key in ["a/0", "a/1", "p/1", "p/2"] {
        rig.mark(key, MarkerState::Want, false).await;
    }
    let listed = page(&rig, "p/", None, 2).await.unwrap();
    assert_eq!(names(&listed), ["p/1", "p/2"]);
    assert_eq!(rig.marks_faults.calls_on(Op::List, "p/"), 1);
}

/// Asserts that the marker names are read on, one page at a time, at most `list_marker_pages`
/// times in a round, until they reach the last merged key; that a round whose names stop short of
/// it keeps only the keys they reach and the listing goes on from there; and that the page is full
/// at the end.
#[tokio::test]
async fn a_listing_extends_the_marker_page_at_most_list_marker_pages_times_and_goes_on() {
    let rig = MarkRig::tuned(ACTIVE_MARKS, |config| {
        config.mark_reads.list_marker_pages = 2
    })
    .await;
    cold_keys(&rig, &["z1", "z2"]).await;
    for key in [
        "m01", "m02", "m03", "m04", "m05", "m06", "m07", "m08", "m09", "m10",
    ] {
        rig.mark(key, MarkerState::Want, false).await;
    }
    assert_eq!(names(&page(&rig, "", None, 2).await.unwrap()), ["z1", "z2"]);
    // Round 1: the page and two more reach m06 and no key. Round 2: the page, one more and one
    // that finds the names ended.
    assert_eq!(rig.marks_faults.calls_on(Op::List, ""), 6);
}

/// Asserts that a page is full when keys were dropped from it, by rounds that go on from the last
/// key they looked at, and that the last page of a listing is short.
#[tokio::test]
async fn a_page_is_made_full_after_gone_keys_are_dropped_and_the_last_is_short() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["k1", "k2", "k3", "k4", "k5", "k6"]).await;
    for key in ["k1", "k2", "k3"] {
        rig.mark(key, MarkerState::Dirty, true).await;
    }
    assert_eq!(names(&page(&rig, "", None, 2).await.unwrap()), ["k4", "k5"]);
    assert_eq!(names(&page(&rig, "", Some("k5"), 2).await.unwrap()), ["k6"]);
    assert_eq!(page(&rig, "", Some("k6"), 2).await, Ok(vec![]));
}

/// Asserts that a listing of no keys is empty and asks nothing.
#[tokio::test]
async fn a_page_of_no_keys_is_empty_and_asks_nothing() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["a"]).await;
    assert_eq!(page(&rig, "", None, 0).await, Ok(vec![]));
    assert_eq!(rig.marks_faults.call_log(), vec![]);
    assert_eq!(rig.cold_faults.call_log(), vec![]);
}

/// Asserts that the keys under the reserved prefixes are left out in the marker path, whichever
/// tier holds them, as they are without it.
#[tokio::test]
async fn the_reserved_keys_are_left_out_of_a_listing_that_reads_markers() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.put_hot("k", "v").await;
    rig.put_cold("_tiering/x", "v").await;
    rig.put_cold("_journal/j", "v").await;
    let listed = rig.proxy.list("").await.unwrap();
    assert_eq!(names(&listed), ["k"]);
}

/// Asserts that a listing in the entry protocol's phase asks the marks bucket nothing, though the
/// bucket holds a `gone` marker of a key the cold tier lists.
#[tokio::test]
async fn in_the_entry_protocol_a_listing_reads_no_marker() {
    let rig = MarkRig::new(ENTRIES).await;
    cold_keys(&rig, &["k"]).await;
    rig.mark("k", MarkerState::Dirty, true).await;
    assert_eq!(names(&page(&rig, "", None, 10).await.unwrap()), ["k"]);
    assert_eq!(names(&rig.proxy.list("").await.unwrap()), ["k"]);
    assert_eq!(rig.marks_faults.call_log(), vec![]);
}

/// Holds the cold tier's page of a listing of `prefix`, moves the monotonic clock by `by` while it
/// is held, and gives what the listing came to. The tier is asked `calls` times in all.
async fn late_listing(rig: &MarkRig, gates: usize, by: Duration) -> Result<Vec<Listed>, Error> {
    let held: Vec<_> = (0..gates)
        .map(|_| rig.cold.gate(Op::List, "", Phase::Post))
        .collect();
    let proxy = Arc::clone(&rig.proxy);
    let task = tokio::spawn(async move { proxy.list_page("", None, 10).await });
    for gate in &held {
        gate.parked().await;
        rig.mono.advance(by);
        assert!(gate.release());
    }
    task.await.unwrap()
}

/// Asserts that the reads after the cold page (the marker of a cold-only key, here) that ended more
/// than `read_window` after the page began are dropped and the page is read again, and that ones
/// that took exactly the window are taken.
#[tokio::test]
async fn a_listing_whose_markers_are_read_too_long_after_the_cold_page_is_read_again() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["k"]).await;
    rig.mark("k", MarkerState::Want, false).await;
    let listed = late_listing(&rig, 1, Duration::from_millis(5001)).await;
    assert_eq!(names(&listed.unwrap()), ["k"]);
    assert_eq!(rig.cold_faults.calls_on(Op::List, ""), 2);

    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["k"]).await;
    rig.mark("k", MarkerState::Want, false).await;
    let listed = late_listing(&rig, 1, Duration::from_millis(5000)).await;
    assert_eq!(names(&listed.unwrap()), ["k"]);
    assert_eq!(rig.cold_faults.calls_on(Op::List, ""), 1);
}

/// Asserts that a listing with no read after the cold page (no marker names the cold-only key) is
/// not read again however long the page took: the marker page was read before the cold page.
#[tokio::test]
async fn a_slow_cold_page_with_no_marker_read_after_it_is_not_read_again() {
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["k"]).await;
    let listed = late_listing(&rig, 1, Duration::from_secs(60)).await;
    assert_eq!(names(&listed.unwrap()), ["k"]);
    assert_eq!(rig.cold_faults.calls_on(Op::List, ""), 1);
}

/// Asserts that a listing whose every round is late is answered unavailable after `read_retries`
/// more rounds, naming the prefix and the numbers.
#[tokio::test]
async fn a_listing_whose_every_round_is_late_is_503_after_read_retries() {
    let tune = |config: &mut ProxyConfig| config.mark_reads.read_retries = 1;
    let rig = MarkRig::tuned(ACTIVE_MARKS, tune).await;
    cold_keys(&rig, &["k"]).await;
    rig.mark("k", MarkerState::Want, false).await;
    let why = "the cold page of a listing of \"\" and the markers read after it were further apart \
               than read_window (5000 ms), 2 times";
    let listed = late_listing(&rig, 2, Duration::from_secs(6)).await;
    assert_eq!(listed, Err(Error::Unavailable(why.to_owned())));
    assert_eq!(rig.cold_faults.calls_on(Op::List, ""), 2);
}

/// Asserts that a listing fails closed when any read it makes fails: the hot page, the cold page,
/// the page of marker names, one more page of names, the marker of a cold-only key, the HEAD of
/// the hot tier after a `gone` marker, and the HEAD of the cold tier after a vanished one.
#[tokio::test]
async fn a_listing_fails_closed_when_a_read_it_makes_fails() {
    let down = || Error::Unavailable("down".to_owned());

    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.hot_faults.fail_next_on(Op::List, "", down());
    assert_eq!(page(&rig, "", None, 10).await, Err(down()));

    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.cold_faults.fail_next_on(Op::List, "", down());
    assert_eq!(page(&rig, "", None, 10).await, Err(down()));

    let rig = MarkRig::new(ACTIVE_MARKS).await;
    rig.marks_faults.fail_next_on(Op::List, "", down());
    assert_eq!(page(&rig, "", None, 10).await, Err(down()));

    // The second page of names: the first is held until the failure is planned.
    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["z"]).await;
    rig.mark("a", MarkerState::Want, false).await;
    rig.mark("b", MarkerState::Want, false).await;
    let gate = rig.marks.gate(Op::List, "", Phase::Post);
    let proxy = Arc::clone(&rig.proxy);
    let task = tokio::spawn(async move { proxy.list_page("", None, 1).await });
    gate.parked().await;
    rig.marks_faults.fail_next_with(Op::List, down());
    assert!(gate.release());
    assert_eq!(task.await.unwrap(), Err(down()));

    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["k"]).await;
    rig.mark("k", MarkerState::Want, false).await;
    rig.marks_faults.fail_next_on(Op::Get, "k", down());
    assert_eq!(page(&rig, "", None, 10).await, Err(down()));

    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["k"]).await;
    rig.mark("k", MarkerState::Dirty, true).await;
    rig.hot_faults.fail_next_on(Op::Head, "k", down());
    assert_eq!(page(&rig, "", None, 10).await, Err(down()));

    let rig = MarkRig::new(ACTIVE_MARKS).await;
    cold_keys(&rig, &["k"]).await;
    rig.mark("k", MarkerState::Want, false).await;
    let gate = rig.cold.gate(Op::List, "", Phase::Post);
    let proxy = Arc::clone(&rig.proxy);
    let task = tokio::spawn(async move { proxy.list_page("", None, 10).await });
    gate.parked().await;
    rig.marks_mem.delete("k").await.unwrap();
    rig.cold_faults.fail_next_on(Op::Head, "k", down());
    assert!(gate.release());
    assert_eq!(task.await.unwrap(), Err(down()));
}
