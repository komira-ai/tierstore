//! The count of the usage figure: each object counts once whether a listing read it or the leader
//! copied it, a copy tried again adds nothing, and an eviction during a listing is not undone by
//! the end of the listing. The monotonic clock is moved by hand; no test reads a clock or waits.

use super::evict::published;
use super::*;
use crate::entry_reconciler::usage::Tally;

/// Asserts the rules of the count on the tally alone: before the listing reaches a key, or when
/// the listing read it, or when it was written before the listing started, a copy adds nothing;
/// otherwise it adds its bytes once, however often it is tried, and a larger body adds the
/// difference; an eviction behind the listing takes off what the listing or the copies counted,
/// and one ahead of it, or of an object neither counted, takes off nothing.
#[test]
fn the_tally_counts_each_object_once() {
    let tally = Tally::default();
    tally.start(100);
    tally.copied("ahead", 10, 150);
    assert_eq!(tally.finish(), (0, 0));

    tally.start(100);
    tally.reach(Some("m"));
    tally.copied("a", 10, 150);
    tally.copied("a", 10, 150);
    tally.copied("a", 25, 160);
    tally.copied("b", 5, 99);
    tally.listed("c", 150);
    tally.listed("old", 50);
    tally.copied("c", 7, 150);
    tally.copied("z", 9, 150);
    assert_eq!(tally.finish(), (25, 0));

    tally.start(100);
    tally.reach(Some("m"));
    tally.copied("a", 10, 150);
    tally.evicted("a", 10, 150);
    tally.listed("c", 150);
    tally.evicted("c", 30, 150);
    tally.evicted("d", 20, 50);
    tally.evicted("e", 5, 200);
    tally.evicted("z", 9, 50);
    assert_eq!(tally.finish(), (0, 50));

    tally.evicted("old", 20, 50);
    assert_eq!(tally.take_landed(), 0);
    tally.clear();
}

/// Asserts that a copy tried again adds its bytes once: after a listing of an empty hot tier an
/// object of thirty bytes is written, its cold write fails and is made at the next step, and the
/// published figure is thirty after the failure and thirty after the copy, not sixty.
#[tokio::test]
async fn a_copy_tried_again_adds_its_bytes_to_the_figure_once() {
    let rig = Rig::new();
    rig.activate().await;
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let mut r = rig.over(rig.hot.clone(), cold.clone(), "r1", |_| {});
    r.step().await;
    assert_eq!(published(&rig).await.hot_bytes, 0);

    rig.clock.advance(1);
    rig.hot.put("a", Bytes::from("a".repeat(30))).await.unwrap();
    land(&rig, "p1", "a").await;
    let error = Error::Unavailable("put".to_owned());
    cold.fail_next_on(Op::Put, "a", error.clone());
    rig.mono.advance(Duration::from_secs(10));
    assert_eq!(r.step().await.failed, [error]);
    assert_eq!(published(&rig).await.hot_bytes, 30);
    rig.mono.advance(Duration::from_secs(10));
    assert!(r.step().await.failed.is_empty());
    assert!(rig.cold_has("a").await);
    assert_eq!(published(&rig).await.hot_bytes, 30);
}

/// Asserts that an object the listing read is not added again when it is copied after the listing
/// has passed its key, though it was written in the millisecond the listing started: four objects
/// are listed two to a page and the copy of the second lands behind the listing, so the figure is
/// forty, not fifty.
#[tokio::test]
async fn an_object_the_listing_read_is_not_added_again_when_it_is_copied() {
    let rig = Rig::new();
    rig.activate().await;
    for key in ["k0", "k1", "k2", "k3"] {
        rig.hot.put(key, bytes("0123456789")).await.unwrap();
    }
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |config| {
        config.base.page_size = 2;
        config.scan_pages = 1;
    });
    assert!(r.step().await.scanning);
    land(&rig, "p1", "k1").await;
    let mut last = r.step().await;
    while last.scanning {
        last = r.step().await;
    }
    assert!(rig.cold_has("k1").await);
    assert_eq!(published(&rig).await.hot_bytes, 40);
}

/// Asserts that an eviction during a listing is not undone when the listing ends: objects `A` to
/// `D` of 30, 40, 30 and 15 bytes (115, over a budget of 100) are listed, `B` goes and the head of
/// `A` fails, so 75 bytes are left and the cycle stays open; a second listing, two to a page, has
/// read `A` and `C` when the next step evicts both, and the listing ends without them: the figure is
/// 15, not 75.
#[tokio::test]
async fn an_object_evicted_during_a_listing_is_not_counted_when_the_listing_ends() {
    let rig = Rig::new();
    rig.activate().await;
    for (key, size) in [("A", 30), ("B", 40), ("C", 30), ("D", 15)] {
        let body = Bytes::from(key.repeat(size));
        rig.hot.put(key, body.clone()).await.unwrap();
        rig.cold.put(key, body).await.unwrap();
        rig.clock.advance(1000);
    }
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    hot.fail_next_on(Op::Head, "A", Error::Unavailable("store".to_owned()));
    let mut r = rig.over(hot, rig.cold.clone(), "r1", |config| {
        config.base.hot_budget = 100;
        config.base.parallel = 2;
        config.base.page_size = 2;
        config.scan_pages = 1;
        config.usage_scan_interval = Duration::from_secs(10);
    });
    let mut first = r.step().await;
    while first.scanning {
        first = r.step().await;
    }
    assert_eq!((first.evicted, first.evicting), (1, true));
    assert!(!rig.hot_has("B").await);

    rig.mono.advance(Duration::from_secs(10));
    let second = r.step().await;
    assert!(second.scanning);
    assert_eq!(second.evicted, 2);
    assert!(!rig.hot_has("A").await);
    assert!(!rig.hot_has("C").await);

    let mut last = second;
    while last.scanning {
        rig.mono.advance(Duration::from_secs(10));
        last = r.step().await;
    }
    assert_eq!(published(&rig).await.hot_bytes, 15);
}
