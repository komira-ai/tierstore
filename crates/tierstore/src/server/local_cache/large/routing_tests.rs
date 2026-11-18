//! Which reads the large-object path takes and which it leaves to the path of `obj/`: what
//! `LOCATE` says of a key decides it, and what the front remembers of the answer (the generation
//! and size of a large object, a definitive miss for [`MISS_TTL`]) decides whether it asks again.
//! Every count is compared with the calls the fake upstreams saw. Time is the cache's clock,
//! moved by hand.

use std::sync::Arc;

use tierstore_fetch::EngineConfig;
use tierstore_fetch_api::ids::PackKey;
use tierstore_fetch_api::upstream::{DeviceRef, Located, Location, NotDefinitive, UpstreamError};
use tierstore_format::ids::NodeId;

use super::testing::{HOLDER, LIMIT, LargeRig, MIB, generation, hot, object, span};
use super::{LargeCounts, LargeObjects, LargeParts, MISS_TTL, S3_JOB};
use crate::server::local_cache::RangeWant;
use crate::server::local_cache::testing::{Rig, digest, pattern};
use crate::{Backend as _, Error, Op};

/// The key of a large object.
const BIG: &str = "cas/big";

/// The milliseconds of [`MISS_TTL`].
fn ttl() -> u64 {
    u64::try_from(MISS_TTL.as_millis()).unwrap()
}

/// The counts the path has made now.
fn counts(rig: &LargeRig) -> LargeCounts {
    rig.large.counters().counts()
}

/// The `LOCATE`s the front asked (the engine asks its own when it fills, and the fake upstream
/// records both).
fn asked(rig: &LargeRig) -> u64 {
    counts(rig).locates
}

/// An upstream error of the kind a `LOCATE` can fail with.
fn failure() -> UpstreamError {
    UpstreamError::NotFound {
        key: PackKey(BIG.to_owned()),
        device: DeviceRef::Hot(NodeId(1)),
    }
}

/// Asserts that a small object that `LOCATE` finds is read by the path of `obj/` from the cache's
/// own upstream, and that the engine is not asked for a byte or a fill.
#[tokio::test]
async fn a_small_object_is_read_by_the_path_of_obj_from_the_cache_upstream() {
    let rig = LargeRig::new("routing-small");
    let bytes = object(LIMIT, 1);
    rig.stored("cas/small", &bytes, generation(1));
    rig.rig.put("cas/small", &bytes).await;
    let before = rig.rig.calls(Op::Get);
    let (meta, _, got) = rig.read("cas/small", None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!(meta.size, LIMIT);
    assert_eq!(rig.rig.calls(Op::Get), before + 1);
    assert_eq!(rig.locates(), 1);
    assert_eq!(
        counts(&rig),
        LargeCounts {
            locates: 1,
            small: 1,
            ..LargeCounts::default()
        }
    );
    assert!(rig.gets().is_empty());
    let stats = rig.engine.stats();
    assert_eq!((stats.gets, stats.fills), ([0; 4], 0));
    assert!(rig.entry("cas/small", generation(1)).is_none());
}

/// Asserts that a key under no `cache=true` policy is never located: the cache's own path serves
/// it, and no count of the large-object path moves.
#[tokio::test]
async fn a_key_under_no_cache_policy_is_not_located_and_is_served_as_before() {
    let rig = LargeRig::new("routing-no-policy");
    let bytes = object(3 * MIB, 2);
    rig.stored("other/x", &bytes, generation(2));
    rig.rig.put("other/x", &bytes).await;
    let before = rig.rig.calls(Op::Get);
    let (_, _, got) = rig.read("other/x", None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!(rig.rig.calls(Op::Get), before + 1);
    assert_eq!(asked(&rig), 0);
    assert_eq!(counts(&rig), LargeCounts::default());
    assert!(rig.gets().is_empty());
}

/// Asserts that a key already held whole in `obj/` is not located again: the first read of a
/// small object fills `obj/` after one `LOCATE`, and the second asks nobody.
#[tokio::test]
async fn a_key_held_whole_in_obj_makes_no_locate() {
    let rig = LargeRig::new("routing-held");
    let bytes = object(MIB / 2, 3);
    rig.stored("cas/held", &bytes, generation(3));
    rig.rig.put("cas/held", &bytes).await;
    rig.read("cas/held", None).await.unwrap();
    assert!(rig.rig.has_file("cas/held"));
    assert_eq!(asked(&rig), 1);
    let before = rig.rig.calls(Op::Get);
    let (_, _, got) = rig.read("cas/held", None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!(asked(&rig), 1);
    assert_eq!(rig.rig.calls(Op::Get), before);
    assert_eq!(counts(&rig).locates, 1);
    assert_eq!(counts(&rig).small, 1);
}

/// Asserts that a location without a size is not routable: the read counts `unroutable`, the
/// engine is not asked, and the cache's own path serves the bytes.
#[tokio::test]
async fn a_location_without_a_size_is_unroutable_and_served_as_before() {
    let rig = LargeRig::new("routing-no-size");
    let bytes = object(1000, 4);
    let location = Location {
        generation: generation(4),
        size: None,
        sources: vec![hot(1000)],
        conflicts: Vec::new(),
    };
    rig.insert("cas/nosize", &bytes, generation(4), location);
    rig.rig.put("cas/nosize", &bytes).await;
    let (_, _, got) = rig.read("cas/nosize", None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!(
        counts(&rig),
        LargeCounts {
            locates: 1,
            unroutable: 1,
            ..LargeCounts::default()
        }
    );
    assert!(rig.gets().is_empty());
}

/// Asserts that a location with no source is not routable either, even when it names a
/// generation and a size over the limit.
#[tokio::test]
async fn a_location_with_no_source_is_unroutable() {
    let rig = LargeRig::new("routing-no-source");
    let bytes = object(1000, 5);
    let location = Location {
        generation: generation(5),
        size: Some(3 * MIB),
        sources: Vec::new(),
        conflicts: Vec::new(),
    };
    rig.insert("cas/nosource", &bytes, generation(5), location);
    rig.rig.put("cas/nosource", &bytes).await;
    let (_, _, got) = rig.read("cas/nosource", None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!(
        counts(&rig),
        LargeCounts {
            locates: 1,
            unroutable: 1,
            ..LargeCounts::default()
        }
    );
    assert!(rig.gets().is_empty());
}

/// Asserts that a definitive miss is a `NotFound` counted as a miss, and that it is remembered:
/// a read within [`MISS_TTL`] of the cache's clock does not ask again and is `NotFound`, and one
/// after it asks again and finds the object that is there by then.
#[tokio::test]
async fn a_definitive_miss_is_remembered_for_the_ttl_and_asked_again_after_it() {
    let rig = LargeRig::new("routing-miss");
    assert!(matches!(rig.open(BIG, None).await, Err(Error::NotFound(_))));
    assert_eq!((asked(&rig), counts(&rig).misses), (1, 1));
    let bytes = object(3 * MIB, 6);
    rig.stored(BIG, &bytes, generation(6));
    rig.rig.clock.advance(ttl() - 1);
    assert!(matches!(rig.open(BIG, None).await, Err(Error::NotFound(_))));
    assert_eq!((asked(&rig), counts(&rig).misses), (1, 2));
    rig.rig.clock.advance(2);
    let (meta, _, got) = rig.read(BIG, None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!(meta.size, 3 * MIB);
    assert_eq!(asked(&rig), 2);
    assert_eq!(counts(&rig).misses, 2);
}

/// Asserts that a miss which is not definitive (an index still being built) is `Unavailable`,
/// counted, and not remembered: the next read asks again and serves the object once it is there.
#[tokio::test]
async fn a_miss_that_is_not_definitive_is_unavailable_and_not_remembered() {
    let rig = LargeRig::new("routing-indexing");
    rig.upstream
        .script_locate(Ok(Located::NotDefinitive(NotDefinitive::Indexing)));
    assert!(matches!(
        rig.open(BIG, None).await,
        Err(Error::Unavailable(_))
    ));
    assert_eq!((asked(&rig), counts(&rig).unavailable), (1, 1));
    let bytes = object(3 * MIB, 7);
    rig.stored(BIG, &bytes, generation(7));
    let (_, _, got) = rig.read(BIG, None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!(asked(&rig), 2);
    assert_eq!(counts(&rig).unavailable, 1);
    assert_eq!(counts(&rig).misses, 0);
}

/// Asserts the same of an answer from too few nodes.
#[tokio::test]
async fn too_few_answers_is_unavailable_and_not_remembered() {
    let rig = LargeRig::new("routing-too-few");
    rig.upstream
        .script_locate(Ok(Located::NotDefinitive(NotDefinitive::TooFewAnswers)));
    assert!(matches!(
        rig.open(BIG, None).await,
        Err(Error::Unavailable(_))
    ));
    let bytes = object(3 * MIB, 8);
    rig.stored(BIG, &bytes, generation(8));
    let (_, _, got) = rig.read(BIG, None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!((asked(&rig), counts(&rig).unavailable), (2, 1));
}

/// Asserts that a failing `LOCATE` is `Unavailable` naming the key, counted, and not remembered.
#[tokio::test]
async fn a_failed_locate_is_unavailable_and_not_remembered() {
    let rig = LargeRig::new("routing-locate-error");
    rig.upstream.script_locate(Err(failure()));
    let error = rig.open(BIG, None).await.err().unwrap();
    assert!(
        matches!(&error, Error::Unavailable(text) if text.starts_with("cas/big: LOCATE failed"))
    );
    assert_eq!((asked(&rig), counts(&rig).unavailable), (1, 1));
    let bytes = object(3 * MIB, 9);
    rig.stored(BIG, &bytes, generation(9));
    let (_, _, got) = rig.read(BIG, None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!(asked(&rig), 2);
}

/// Asserts that a delete through the cache forgets a large object: the next read asks `LOCATE`
/// again, where without the delete it would not.
#[tokio::test]
async fn a_delete_through_the_cache_forgets_a_known_large_object() {
    let rig = LargeRig::build(
        "routing-delete",
        usize::MAX,
        EngineConfig::default(),
        |config| config.write_through = true,
        |store| store,
    );
    let bytes = object(3 * MIB, 10);
    rig.stored(BIG, &bytes, generation(10));
    rig.rig.put(BIG, &bytes).await;
    rig.read(BIG, None).await.unwrap();
    rig.read(BIG, None).await.unwrap();
    assert_eq!((asked(&rig), counts(&rig).known), (1, 1));
    rig.rig.cache.delete(BIG).await.unwrap();
    rig.read(BIG, None).await.unwrap();
    assert_eq!((asked(&rig), counts(&rig).known), (2, 1));
}

/// Asserts that a put through the cache forgets a remembered definitive miss: the next read asks
/// again within the TTL and finds the object, where without the put it would be `NotFound`.
#[tokio::test]
async fn a_put_through_the_cache_forgets_a_remembered_miss() {
    let rig = LargeRig::build(
        "routing-put",
        usize::MAX,
        EngineConfig::default(),
        |config| config.write_through = true,
        |store| store,
    );
    assert!(matches!(rig.open(BIG, None).await, Err(Error::NotFound(_))));
    assert!(matches!(rig.open(BIG, None).await, Err(Error::NotFound(_))));
    assert_eq!(asked(&rig), 1);
    let bytes = object(3 * MIB, 11);
    rig.stored(BIG, &bytes, generation(11));
    rig.rig.cache.put(BIG, bytes.clone()).await.unwrap();
    let (_, _, got) = rig.read(BIG, None).await.unwrap();
    assert_eq!(got, bytes.to_vec());
    assert_eq!(asked(&rig), 2);
}

/// Asserts that a second path is refused while one is attached, and that the attached one is the
/// first.
#[tokio::test]
async fn a_second_large_path_is_not_attached() {
    let rig = LargeRig::new("routing-attach");
    let other = Arc::new(LargeObjects::new(LargeParts {
        engine: rig.engine.clone(),
        upstream: rig.upstream.clone(),
        store: rig.store.clone(),
        holder: HOLDER,
    }));
    assert!(!rig.rig.cache.route_large(other.clone()));
    let attached = rig.rig.cache.large().unwrap();
    assert!(Arc::ptr_eq(attached, &rig.large));
    assert!(!Arc::ptr_eq(attached, &other));
}

/// Asserts that a book of one object asks `LOCATE` at every read of two objects read in turn,
/// and that the default book asks once for each.
#[tokio::test]
async fn a_book_of_one_forgets_the_other_object_and_a_larger_book_does_not() {
    let (a, b) = (object(3 * MIB, 12), object(2 * MIB, 13));
    let small = LargeRig::with("routing-book-one", 1, EngineConfig::default());
    let wide = LargeRig::new("routing-book-wide");
    for rig in [&small, &wide] {
        rig.stored("cas/a", &a, generation(12));
        rig.stored("cas/b", &b, generation(13));
        for key in ["cas/a", "cas/b", "cas/a", "cas/b"] {
            let (_, _, got) = rig.read(key, None).await.unwrap();
            let want = if key == "cas/a" { &a } else { &b };
            assert_eq!(got, want.to_vec());
        }
    }
    assert_eq!(asked(&small), 4);
    assert_eq!(counts(&small).known, 0);
    assert_eq!(asked(&wide), 2);
    assert_eq!(counts(&wide).known, 2);
}

/// Asserts that with no large-object path attached a ranged read is served as before, from the
/// cache's upstream, and the cache has none.
#[tokio::test]
async fn a_cache_with_no_large_path_serves_a_ranged_read_as_before() {
    let rig = Rig::new("routing-plain");
    assert!(rig.cache.large().is_none());
    let bytes = pattern(5000);
    rig.put("cas/plain", &bytes).await;
    let opened = rig
        .cache
        .open_read("cas/plain", span(100, 199))
        .await
        .unwrap();
    assert_eq!(opened.meta.size, 5000);
    let got = super::testing::collect(opened.body).await.unwrap();
    assert_eq!(got, bytes[100..200].to_vec());
}

/// Asserts that each read is a job of its own with the S3 bit set: the GETs of two large stored
/// objects, and of a third, carry three different jobs, each with bit 63.
#[tokio::test]
async fn every_read_is_its_own_job_with_the_s3_bit() {
    let rig = LargeRig::new("routing-jobs");
    for (key, seed) in [("cas/j1", 14), ("cas/j2", 15), ("cas/j3", 16)] {
        let bytes = object(2 * MIB, seed);
        rig.stored(key, &bytes, generation(seed));
        rig.read(key, None).await.unwrap();
    }
    let gets = rig.gets();
    assert!(gets.len() >= 3);
    assert!(gets.iter().all(|get| get.job.0 & S3_JOB == S3_JOB));
    let job_of = |key: &str| {
        let jobs: Vec<u64> = gets
            .iter()
            .filter(|get| get.key.0 == key)
            .map(|get| get.job.0)
            .collect();
        assert!(!jobs.is_empty(), "no GET of {key}");
        assert!(jobs.windows(2).all(|pair| pair[0] == pair[1]));
        jobs[0]
    };
    let jobs = [job_of("cas/j1"), job_of("cas/j2"), job_of("cas/j3")];
    assert!(jobs[0] != jobs[1] && jobs[1] != jobs[2] && jobs[0] != jobs[2]);
}

/// Asserts that a delete through the cache while a read's `LOCATE` is out keeps that read's
/// answer out of the book: the read is served, and the next read asks `LOCATE` again, where it
/// would otherwise take the generation learned before the delete.
#[tokio::test]
async fn a_delete_during_a_locate_keeps_its_answer_out_of_the_book() {
    let rig = LargeRig::build(
        "routing-delete-race",
        usize::MAX,
        EngineConfig::default(),
        |config| config.write_through = true,
        |store| store,
    );
    let bytes = object(3 * MIB, 14);
    rig.stored(BIG, &bytes, generation(14));
    rig.rig.put(BIG, &bytes).await;
    let gate = rig.upstream.hold_locates();
    let (read, ()) = tokio::join!(rig.read(BIG, None), async {
        rig.upstream.located(1).await;
        rig.rig.cache.delete(BIG).await.unwrap();
        gate.add_permits(64);
    });
    assert_eq!(read.unwrap().2, bytes.to_vec());
    assert_eq!((asked(&rig), counts(&rig).known), (1, 0));
    rig.read(BIG, None).await.unwrap();
    assert_eq!((asked(&rig), counts(&rig).known), (2, 0));
}

/// Asserts that a put through the cache while a read's `LOCATE` is out keeps that read's
/// definitive miss out of the book: the read is `NotFound`, and the next read, inside the TTL,
/// asks `LOCATE` again and finds the object the put wrote.
#[tokio::test]
async fn a_put_during_a_locate_keeps_its_miss_out_of_the_book() {
    let rig = LargeRig::build(
        "routing-put-race",
        usize::MAX,
        EngineConfig::default(),
        |config| config.write_through = true,
        |store| store,
    );
    let bytes = object(3 * MIB, 15);
    let gate = rig.upstream.hold_locates();
    // The waiter is polled first, so it waits for the `LOCATE` to arrive.
    let ((), read) = tokio::join!(
        async {
            rig.upstream.located(1).await;
            rig.rig.cache.put(BIG, bytes.clone()).await.unwrap();
            gate.add_permits(64);
        },
        rig.open(BIG, None)
    );
    assert!(matches!(read, Err(Error::NotFound(_))));
    rig.stored(BIG, &bytes, generation(15));
    let (_, _, served) = rig.read(BIG, None).await.unwrap();
    assert_eq!(served, bytes.to_vec());
    assert_eq!(asked(&rig), 2);
}

/// Asserts that a key held by an entry by extents (a range of a pack that a fill of ranges
/// kept) is not routed to the engine: a read inside that range asks no `LOCATE`, counts nothing
/// in the path, and is served from the extent with no GET of the cache's upstream.
#[tokio::test]
async fn a_key_held_by_an_entry_by_extents_is_not_located() {
    let rig = LargeRig::new("routing-extents");
    let bytes = object(3 * MIB, 16);
    rig.rig.put(BIG, &bytes).await;
    rig.stored(BIG, &bytes, generation(16));
    let sha256 = digest(&bytes[..1000]);
    let want = RangeWant {
        pack: BIG,
        known: None,
        offset: 0,
        len: 1000,
        sha256: &sha256,
    };
    rig.rig.cache.fill_range(&want).await.unwrap();
    let gets = rig.rig.calls(Op::Get);
    let (_, _, served) = rig.read(BIG, span(100, 199)).await.unwrap();
    assert_eq!(served, bytes[100..200]);
    assert_eq!(rig.rig.calls(Op::Get), gets);
    assert_eq!(counts(&rig), LargeCounts::default());
    assert_eq!(rig.locates(), 0);
}
