//! The fill of several parts of a pack in one read: what it reads, what it keeps, what it refuses,
//! and how it shares and waits with other fills.

use super::super::layout::name_of;
use super::super::metrics::read;
use super::super::pack_testing::Pack;
use super::super::testing::{Rig, digest, until};
use super::files::{ExtentSidecar, meta_of};
use super::fill_tests::{data, pack_in};
use super::{PartsWant, RangeError, RangePart, RangeWant};
use crate::Op;

/// A part as the tests give it: the first byte, the end, and the digest it must have.
type Part = (u64, u64, String);

/// The parts `[from, to)` of `pack`, each under the digest of its true bytes.
fn true_parts(pack: &Pack, spans: &[(u64, u64)]) -> Vec<Part> {
    spans
        .iter()
        .map(|(from, to)| {
            let bytes = &pack.bytes()[*from as usize..*to as usize];
            (*from, *to, digest(bytes))
        })
        .collect()
}

/// Fills `parts` of the pack `key` of `rig` as `fill_parts` is asked.
async fn fill_parts_of(
    cache: &super::super::LocalCache,
    key: &str,
    parts: &[Part],
) -> Result<(), RangeError> {
    let given: Vec<RangePart<'_>> = parts
        .iter()
        .map(|(from, to, sha)| RangePart {
            offset: *from,
            len: to - from,
            sha256: sha,
        })
        .collect();
    let want = PartsWant {
        pack: key,
        known: None,
        parts: &given,
    };
    cache.fill_parts(&want).await
}

/// The sidecar of the entry of `pack` by extents.
fn sidecar(rig: &Rig, pack: &Pack) -> ExtentSidecar {
    let text = std::fs::read(meta_of(&rig.config.path, &name_of(&pack.key).hex)).unwrap();
    serde_json::from_slice(&text).unwrap()
}

/// Asserts that parts with gaps between them are read in one GET from the first start to the last
/// end and only the parts are kept: the sidecar lists the two parts, the cache holds their bytes,
/// each part has a span and a gap between them has none, and the bytes of the parts are in the file
/// at their own offsets.
#[tokio::test]
async fn parts_with_gaps_are_read_in_one_pass_and_only_the_parts_are_kept() {
    let rig = Rig::new("tspart-gaps");
    let pack = pack_in(&rig).await;
    let parts = true_parts(&pack, &[(500, 1000), (2000, 2500), (3000, 3600)]);
    fill_parts_of(&rig.cache, &pack.key, &parts).await.unwrap();
    assert_eq!(rig.upstream.get_ranges(), [Some((500, 3100))]);
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (1, 1));
    assert_eq!(
        sidecar(&rig, &pack).extents,
        [[500, 500], [2000, 500], [3000, 600]]
    );
    assert_eq!(rig.cache.extents_held(), (1, 1600));
    for (from, to, _) in &parts {
        assert!(rig.cache.span_of(&pack.key, *from, to - from).is_some());
    }
    assert!(rig.cache.span_of(&pack.key, 1000, 1000).is_none());
    assert!(rig.cache.span_of(&pack.key, 2500, 500).is_none());
    let file = std::fs::read(data(&rig, &pack)).unwrap();
    for (from, to, _) in &parts {
        let (from, to) = (*from as usize, *to as usize);
        assert_eq!(file[from..to], pack.bytes()[from..to]);
    }
    let counters = &rig.cache.inner.extents.counters;
    assert_eq!(
        (read(&counters.fills), read(&counters.fill_bytes)),
        (1, 1600)
    );
}

/// Asserts that a chunk that lies wholly in a gap between parts is read and given to no part: with
/// chunks of 700 bytes and a gap of 2900 bytes the upstream is asked chunk by chunk over the whole
/// span, and both parts are still kept, each with the digest of its own bytes.
#[tokio::test]
async fn a_chunk_in_a_gap_belongs_to_no_part() {
    let rig = Rig::with("tspart-chunk-gap", |config| config.fill_chunk = 700);
    let pack = pack_in(&rig).await;
    let parts = true_parts(&pack, &[(3000, 3100), (0, 100)]);
    fill_parts_of(&rig.cache, &pack.key, &parts).await.unwrap();
    assert_eq!(
        rig.upstream.get_ranges(),
        [
            Some((0, 700)),
            Some((700, 700)),
            Some((1400, 700)),
            Some((2100, 700)),
            Some((2800, 300))
        ]
    );
    assert_eq!(sidecar(&rig, &pack).extents, [[0, 100], [3000, 100]]);
    assert!(rig.cache.span_of(&pack.key, 0, 100).is_some());
    assert!(rig.cache.span_of(&pack.key, 3000, 100).is_some());
    assert!(rig.cache.span_of(&pack.key, 1000, 100).is_none());
}

/// Asserts that a part that spans several chunks is hashed across them: a part of 1500 bytes read in
/// chunks of 700 is kept under the digest of its whole bytes beside a part that starts inside a
/// chunk.
#[tokio::test]
async fn a_part_that_spans_chunks_is_hashed_whole() {
    let rig = Rig::with("tspart-spans-chunks", |config| config.fill_chunk = 700);
    let pack = pack_in(&rig).await;
    let parts = true_parts(&pack, &[(100, 1600), (1800, 2000)]);
    fill_parts_of(&rig.cache, &pack.key, &parts).await.unwrap();
    assert_eq!(sidecar(&rig, &pack).extents, [[100, 1500], [1800, 200]]);
    assert_eq!(rig.cache.extents_held(), (1, 1700));
}

/// Asserts that one part whose digest is wrong is a mismatch and the other parts are kept: the
/// sidecar lists the two good parts, the bad part has no span, the failure is counted once, and a
/// second fill of the good parts and the bad part's true digest keeps the bad part too.
#[tokio::test]
async fn a_part_with_a_wrong_digest_is_a_mismatch_and_the_others_are_kept() {
    let rig = Rig::new("tspart-one-wrong");
    let pack = pack_in(&rig).await;
    let mut parts = true_parts(&pack, &[(0, 500), (1000, 1500), (2000, 2500)]);
    let right = parts[1].2.clone();
    parts[1].2 = digest(b"other bytes");
    assert_eq!(
        fill_parts_of(&rig.cache, &pack.key, &parts).await,
        Err(RangeError::Mismatch)
    );
    assert_eq!(sidecar(&rig, &pack).extents, [[0, 500], [2000, 500]]);
    assert!(rig.cache.span_of(&pack.key, 0, 500).is_some());
    assert!(rig.cache.span_of(&pack.key, 1000, 500).is_none());
    assert!(rig.cache.span_of(&pack.key, 2000, 500).is_some());
    assert_eq!(rig.cache.extents_held(), (1, 1000));
    assert_eq!(rig.cache.inner.extents.counters.failures("mismatch"), 1);
    parts[1].2 = right;
    fill_parts_of(&rig.cache, &pack.key, &parts).await.unwrap();
    assert_eq!(
        sidecar(&rig, &pack).extents,
        [[0, 500], [1000, 500], [2000, 500]]
    );
    assert_eq!(rig.upstream.get_ranges().len(), 2);
}

/// Asserts that parts that all have a wrong digest are a mismatch and leave nothing: no entry, no
/// data file, no bytes held.
#[tokio::test]
async fn parts_that_all_have_wrong_digests_leave_nothing() {
    let rig = Rig::new("tspart-all-wrong");
    let pack = pack_in(&rig).await;
    let mut parts = true_parts(&pack, &[(0, 500), (1000, 1500)]);
    parts[0].2 = digest(b"one");
    parts[1].2 = digest(b"two");
    assert_eq!(
        fill_parts_of(&rig.cache, &pack.key, &parts).await,
        Err(RangeError::Mismatch)
    );
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
    assert!(!meta_of(&rig.config.path, &name_of(&pack.key).hex).exists());
    assert_eq!(rig.cache.inner.extents.counters.failures("mismatch"), 1);
}

/// Asserts the refusals that need no upstream, each by its name: no parts, a part of no bytes, a
/// digest that is not sixty-four hex digits, a part whose end overflows (each `empty`, even beside
/// good parts), and a key the cache does not keep; none asks the upstream or leaves an entry.
#[tokio::test]
async fn parts_that_are_empty_or_malformed_are_refused_before_the_upstream() {
    let rig = Rig::new("tspart-refuse");
    let pack = pack_in(&rig).await;
    let good = true_parts(&pack, &[(0, 10)]);
    assert_eq!(
        fill_parts_of(&rig.cache, &pack.key, &[]).await,
        Err(RangeError::Empty)
    );
    let sha = good[0].2.clone();
    let given = [
        RangePart {
            offset: 20,
            len: 10,
            sha256: &sha,
        },
        RangePart {
            offset: 40,
            len: 0,
            sha256: &sha,
        },
    ];
    let want = PartsWant {
        pack: &pack.key,
        known: None,
        parts: &given,
    };
    assert_eq!(rig.cache.fill_parts(&want).await, Err(RangeError::Empty));
    let bad = [(0, 10, "xyz".to_owned())];
    assert_eq!(
        fill_parts_of(&rig.cache, &pack.key, &bad).await,
        Err(RangeError::Empty)
    );
    let given = [RangePart {
        offset: u64::MAX,
        len: 2,
        sha256: &sha,
    }];
    let want = PartsWant {
        pack: &pack.key,
        known: None,
        parts: &given,
    };
    assert_eq!(rig.cache.fill_parts(&want).await, Err(RangeError::Empty));
    assert_eq!(
        fill_parts_of(&rig.cache, "other/pack", &good).await,
        Err(RangeError::NotKept)
    );
    assert_eq!(rig.calls(Op::Head) + rig.calls(Op::Get), 0);
    assert_eq!(rig.cache.extents_held(), (0, 0));
}

/// Asserts that a part past the end of the pack is refused as past the end, beside a good part, and
/// that nothing is kept.
#[tokio::test]
async fn a_part_past_the_end_of_the_pack_is_refused() {
    let rig = Rig::new("tspart-past");
    let pack = pack_in(&rig).await;
    let mut parts = true_parts(&pack, &[(0, 100)]);
    parts.push((5900, 6100, digest(b"x")));
    assert_eq!(
        fill_parts_of(&rig.cache, &pack.key, &parts).await,
        Err(RangeError::Past)
    );
    assert_eq!(rig.cache.extents_held(), (0, 0));
}

/// Asserts that a cache that does not hold the lock of its directory fills no parts: the answer is
/// `read-only` and the holder's directory is as it was.
#[tokio::test]
async fn a_cache_without_the_lock_fills_no_parts() {
    let rig = Rig::new("tspart-readonly");
    let pack = pack_in(&rig).await;
    let _holder = rig.locked();
    let waiting = rig.locked();
    let parts = true_parts(&pack, &[(0, 10), (20, 30)]);
    assert_eq!(
        fill_parts_of(&waiting, &pack.key, &parts).await,
        Err(RangeError::ReadOnly)
    );
    assert_eq!(waiting.extents_held(), (0, 0));
    assert_eq!(rig.calls(Op::Get), 0);
}

/// Asserts that parts the cache holds are answered at once and ask the upstream nothing, that
/// parts of which only some are held are all read again in one pass, and that every held answer
/// is counted.
#[tokio::test]
async fn parts_that_are_held_ask_the_upstream_nothing() {
    let rig = Rig::new("tspart-held");
    let pack = pack_in(&rig).await;
    let parts = true_parts(&pack, &[(0, 500), (2000, 2500)]);
    fill_parts_of(&rig.cache, &pack.key, &parts).await.unwrap();
    fill_parts_of(&rig.cache, &pack.key, &parts).await.unwrap();
    assert_eq!(rig.calls(Op::Get), 1);
    assert_eq!(read(&rig.cache.inner.extents.counters.held), 1);

    let more = true_parts(&pack, &[(0, 500), (3000, 3500)]);
    fill_parts_of(&rig.cache, &pack.key, &more).await.unwrap();
    assert_eq!(
        rig.upstream.get_ranges(),
        [Some((0, 2500)), Some((0, 3500))]
    );
    assert_eq!(
        sidecar(&rig, &pack).extents,
        [[0, 500], [2000, 500], [3000, 500]]
    );
}

/// Asserts that a second fill of the same parts while one runs joins it: one GET, both answered
/// `ok`, the second counted as a join, and no flight is left.
#[tokio::test]
async fn a_second_fill_of_the_same_parts_joins_the_flight() {
    let rig = Rig::new("tspart-join");
    let pack = pack_in(&rig).await;
    let parts = true_parts(&pack, &[(0, 500), (2000, 2500)]);
    let gate = rig.tamper.hold();
    let ask = || {
        let cache = rig.cache.clone();
        let (key, parts) = (pack.key.clone(), parts.clone());
        tokio::spawn(async move { fill_parts_of(&cache, &key, &parts).await })
    };
    let first = ask();
    until("the first fill is at the upstream", || {
        rig.calls(Op::Get) == 1
    })
    .await;
    let second = ask();
    let counters = &rig.cache.inner.extents.counters;
    until("the second joined", || read(&counters.joins) == 1).await;
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(rig.calls(Op::Get), 1);
    assert_eq!(rig.cache.extents_held(), (1, 1000));
    assert!(rig.cache.inner.extents.flights.lock().unwrap().is_empty());
}

/// Asserts that a fill of one range that lies inside a span a parts flight reads waits for the
/// flight and then finds its range held: one GET in all, and the range has a span.
#[tokio::test]
async fn a_range_fill_inside_a_parts_flight_waits_and_finds_it_held() {
    let rig = Rig::new("tspart-behind");
    let pack = pack_in(&rig).await;
    let parts = true_parts(&pack, &[(0, 500), (2000, 2500)]);
    let gate = rig.tamper.hold();
    let first = {
        let cache = rig.cache.clone();
        let (key, parts) = (pack.key.clone(), parts.clone());
        tokio::spawn(async move { fill_parts_of(&cache, &key, &parts).await })
    };
    until("the parts fill is at the upstream", || {
        rig.calls(Op::Get) == 1
    })
    .await;
    let second = {
        let cache = rig.cache.clone();
        let (key, sha) = (pack.key.clone(), digest(&pack.bytes()[..500]));
        tokio::spawn(async move {
            let want = RangeWant {
                pack: &key,
                known: None,
                offset: 0,
                len: 500,
                sha256: &sha,
            };
            cache.fill_range(&want).await
        })
    };
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(rig.calls(Op::Get), 1);
    assert_eq!(rig.cache.inner.extents.flights.lock().unwrap().len(), 1);
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(rig.calls(Op::Get), 1);
    assert!(rig.cache.span_of(&pack.key, 0, 500).is_some());
}

/// Asserts that parts that together cover the whole pack, given out of order, make it an ordinary
/// object of `obj/` with the pack's bytes, leave no entry by extents, and count one promotion.
#[tokio::test]
async fn parts_that_cover_the_pack_move_it_whole_into_obj() {
    let rig = Rig::new("tspart-whole");
    let pack = pack_in(&rig).await;
    let parts = true_parts(&pack, &[(3000, 6000), (0, 1000), (1000, 3000)]);
    fill_parts_of(&rig.cache, &pack.key, &parts).await.unwrap();
    assert!(rig.has_file(&pack.key));
    assert_eq!(std::fs::read(rig.file(&pack.key)).unwrap(), pack.bytes());
    assert_eq!(rig.cache.held(), (6000, 1));
    assert_eq!(rig.cache.extents_held(), (0, 0));
    assert!(!data(&rig, &pack).exists());
    assert_eq!(read(&rig.cache.inner.extents.counters.promotions), 1);
    assert_eq!(rig.upstream.get_ranges(), [Some((0, 6000))]);
}
