//! The first part alone of a silo-origin object (design section 9; section 14, row "S3-front
//! object"): an object whose location names no generation has its first cells fetched by one GET
//! with no `If-Match`, whose `ETag` is the generation every later part carries. No test here
//! waits for a time: a held upstream and the engine's own progress are the only waits.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt as _;
use tierstore_fetch_api::entry::EntryStore as _;
use tierstore_fetch_api::fake::PartScript;
use tierstore_fetch_api::ids::PackKey;
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, GetRequest, Located, Location, UpstreamError,
};
use tierstore_format::ids::Generation;

use super::testing::{LIMIT, LargeRig, MIB, collect, generation, legacy, object, span};
use crate::{ByteRange, Error, ReadBody, Span, Window};

/// Six cells and a short seventh.
const SIZE: u64 = 6 * MIB + 1000;

/// The `ETag` text of a generation made of the byte `b`.
fn text(b: u8) -> String {
    format!("{b:02x}").repeat(16)
}

/// The generation of `request`, if it carried one.
fn if_match(request: &GetRequest) -> Option<Generation> {
    request.generation
}

/// The bytes of `body` until it ends, and the error that ended it, if one did.
async fn drain(mut body: ReadBody) -> (Vec<u8>, Option<Error>) {
    let mut bytes = Vec::new();
    while let Some(piece) = body.next().await {
        match piece {
            Ok(piece) => bytes.extend_from_slice(&piece),
            Err(error) => return (bytes, Some(error)),
        }
    }
    (bytes, None)
}

/// A rig with a silo-origin object of `size` bytes under `key` whose `ETag` is `etag`.
fn silo_rig(name: &str, key: &str, size: u64, etag: u8) -> (LargeRig, Bytes) {
    let rig = LargeRig::new(name);
    let bytes = object(size, 3);
    rig.silo(key, &bytes, generation(etag));
    (rig, bytes)
}

/// Asserts that a whole read of a silo-origin object of several cells sends its first part alone
/// with no `If-Match` and the cells of the first four, then every later part with the `ETag` of
/// that first answer; that the bytes served are the object's; that the client is told the `ETag`
/// as 32 hex digits; and that exactly one first part alone is counted.
#[tokio::test]
async fn a_whole_read_of_a_silo_object_sends_the_first_part_alone_and_pins_the_rest() {
    let key = "cas/alone/whole";
    let (rig, bytes) = silo_rig("alone-whole", key, SIZE, 0xaa);
    let (meta, window, served) = rig.read(key, None).await.expect("a read");
    assert_eq!(window, Window::Whole);
    assert_eq!(meta.size, SIZE);
    assert_eq!(meta.tag, text(0xaa));
    assert_eq!(served, bytes.to_vec());
    let gets = rig.gets();
    assert_eq!(if_match(&gets[0]), None);
    assert_eq!(
        (gets[0].range.offset, gets[0].range.len),
        (0, 4 * MIB),
        "the first part alone is the first four cells"
    );
    assert!(gets.len() >= 2, "the engine fetched the rest");
    for later in &gets[1..] {
        assert_eq!(if_match(later), Some(generation(0xaa)));
        assert!(later.range.offset >= 4 * MIB, "no part refetches the head");
    }
    let counts = rig.large.counters().counts();
    assert_eq!((counts.alone, counts.routed, counts.locates), (1, 1, 1));
    assert_eq!((counts.known, counts.misses, counts.unavailable), (0, 0, 0));
    assert_eq!(counts.fill_failures, 0);
}

/// Asserts that while the first part alone is held at the upstream, the engine has been asked for
/// nothing (no fill, no entry in the store) and only one GET has reached the upstream, and that
/// the read completes with the object's bytes once the part is let go.
#[tokio::test]
async fn the_engine_is_not_asked_while_the_first_part_is_held() {
    let key = "cas/alone/held";
    let (rig, bytes) = silo_rig("alone-held", key, SIZE, 0xab);
    let rig = Arc::new(rig);
    let gate = rig.upstream.hold();
    let reading = tokio::spawn({
        let rig = rig.clone();
        async move { rig.read(key, None).await }
    });
    rig.upstream.entered(1).await;
    assert_eq!(rig.upstream.count(), 1);
    assert_eq!(rig.engine.stats().fills, 0);
    assert!(rig.store.packs().is_empty(), "no entry is open yet");
    assert_eq!(rig.large.counters().counts().alone, 1);
    gate.add_permits(1000);
    let (_, _, served) = reading.await.expect("joined").expect("a read");
    assert_eq!(served, bytes.to_vec());
    assert!(
        rig.engine.stats().fills >= 1,
        "the rest went through the engine"
    );
    assert_eq!(if_match(&rig.gets()[0]), None);
}

/// Asserts that an overwrite on silo between the first part and the engine's parts ends the read
/// with an error after only the old object's first four cells (the engine's parts carry the old
/// `ETag` and are refused with `412`), counts the failed fill, makes the book forget the key, and
/// that the next read sends a first part alone again and serves the whole new object with the new
/// `ETag`.
#[tokio::test]
async fn an_overwrite_between_the_first_part_and_the_rest_is_a_failed_read_not_a_mix() {
    let key = "cas/alone/overwrite";
    let (rig, old) = silo_rig("alone-overwrite", key, SIZE, 0xac);
    let rig = Arc::new(rig);
    let gate = rig.upstream.hold();
    let reading = tokio::spawn({
        let rig = rig.clone();
        async move {
            let opened = rig.open(key, None).await.expect("the head opens");
            drain(opened.body).await
        }
    });
    rig.upstream.entered(1).await;
    gate.add_permits(1);
    rig.upstream.entered(2).await;
    let new = object(SIZE, 9);
    rig.silo(key, &new, generation(0xad));
    gate.add_permits(1000);
    let (served, error) = reading.await.expect("joined");
    assert!(matches!(error, Some(Error::Unavailable(_))), "{error:?}");
    assert_eq!(served.len() as u64, 4 * MIB, "only the first part alone");
    assert_eq!(served, old[..served.len()].to_vec());
    let gets = rig.gets();
    assert_eq!(if_match(&gets[0]), None);
    assert!(gets.len() >= 2);
    for later in &gets[1..] {
        assert_eq!(if_match(later), Some(generation(0xac)));
    }
    assert_eq!(rig.large.counters().counts().fill_failures, 1);

    let before = gets.len();
    let (meta, _, served) = rig.read(key, None).await.expect("the next read");
    assert_eq!(meta.tag, text(0xad));
    assert_eq!(served, new.to_vec());
    let counts = rig.large.counters().counts();
    assert_eq!((counts.alone, counts.known, counts.locates), (2, 0, 2));
    let fresh: Vec<GetRequest> = rig.gets()[before..]
        .iter()
        .filter(|get| if_match(get) != Some(generation(0xac)))
        .cloned()
        .collect();
    assert_eq!(if_match(&fresh[0]), None, "the book forgot: alone again");
    for later in &fresh[1..] {
        assert_eq!(if_match(later), Some(generation(0xad)));
    }
}

/// Asserts that a read whose range lies inside the first part costs exactly one GET (the part
/// alone, with no `If-Match`, of the first cell), never asks the engine, and serves the bytes of
/// the range.
#[tokio::test]
async fn a_range_inside_the_first_part_costs_one_get_and_no_fill() {
    let key = "cas/alone/inside";
    let (rig, bytes) = silo_rig("alone-inside", key, SIZE, 0xae);
    let (meta, window, served) = rig.read(key, span(100, 2000)).await.expect("a read");
    assert_eq!(window, Window::Part(ByteRange::new(100, 1901)));
    assert_eq!(meta.tag, text(0xae));
    assert_eq!(served, bytes[100..=2000].to_vec());
    let gets = rig.gets();
    assert_eq!(gets.len(), 1);
    assert_eq!(if_match(&gets[0]), None);
    assert_eq!((gets[0].range.offset, gets[0].range.len), (0, MIB));
    assert_eq!(rig.engine.stats().fills, 0);
    assert_eq!(rig.engine.stats().gets, [0; 4]);
}

/// Asserts that a read starting in the middle of an object has its part alone start at the cell
/// of the start (not at 0), with the first four cells from there, serves the bytes from the
/// start, and fetches what follows the part with `If-Match`.
#[tokio::test]
async fn a_read_from_mid_object_starts_the_part_alone_at_its_cell() {
    let key = "cas/alone/mid";
    let size = 10 * MIB + 500;
    let (rig, bytes) = silo_rig("alone-mid", key, size, 0xaf);
    let first = 3 * MIB + MIB / 2;
    let (_, window, served) = rig
        .read(key, Some(Span::From { first, last: None }))
        .await
        .expect("a read");
    assert_eq!(window, Window::Part(ByteRange::new(first, size - first)));
    assert_eq!(served, bytes[first as usize..].to_vec());
    let gets = rig.gets();
    assert_eq!(if_match(&gets[0]), None);
    assert_eq!(
        (gets[0].range.offset, gets[0].range.len),
        (3 * MIB, 4 * MIB)
    );
    assert!(gets.len() >= 2);
    for later in &gets[1..] {
        assert_eq!(if_match(later), Some(generation(0xaf)));
        assert!(later.range.offset >= 7 * MIB);
    }
}

/// Asserts that a second read after a first knows the object from the book: it counts as known,
/// asks no `LOCATE`, sends no first part alone (every GET it costs carries the `ETag`, and
/// fetches the head cells the first read did not keep), and serves the object's bytes.
#[tokio::test]
async fn a_second_read_knows_the_generation_and_sends_no_part_without_if_match() {
    let key = "cas/alone/second";
    let (rig, bytes) = silo_rig("alone-second", key, SIZE, 0xb0);
    rig.read(key, None).await.expect("the first read");
    let before = rig.gets().len();
    let (meta, _, served) = rig.read(key, None).await.expect("the second read");
    assert_eq!(meta.tag, text(0xb0));
    assert_eq!(served, bytes.to_vec());
    let gets = rig.gets();
    assert!(gets.len() > before, "the head cells were not kept");
    for get in &gets[before..] {
        assert_eq!(if_match(get), Some(generation(0xb0)));
    }
    assert!(gets[before..].iter().any(|get| get.range.offset == 0));
    let counts = rig.large.counters().counts();
    assert_eq!((counts.alone, counts.known, counts.locates), (1, 1, 1));
    assert_eq!(counts.routed, 2);
}

/// Asserts that a span starting past the end of a silo object opens as `Window::Unsatisfiable`
/// with an empty body and the object's size and `ETag`; that the first such read costs one GET
/// of one byte (alone, for the `ETag`) and no fill; and that the next one, the object being
/// known, costs no GET at all.
#[tokio::test]
async fn an_unsatisfiable_range_of_a_silo_object_costs_one_byte_alone_once() {
    let key = "cas/alone/unsatisfiable";
    let (rig, _) = silo_rig("alone-unsatisfiable", key, 3 * MIB, 0xb1);
    let past = Some(Span::From {
        first: 3 * MIB,
        last: None,
    });
    for round in 0..2 {
        let opened = rig.open(key, past).await.expect("opens");
        assert_eq!(opened.window, Window::Unsatisfiable);
        assert_eq!((opened.meta.size, opened.meta.tag), (3 * MIB, text(0xb1)));
        assert!(collect(opened.body).await.expect("a body").is_empty());
        let gets = rig.gets();
        assert_eq!(gets.len(), 1, "round {round}");
        assert_eq!(if_match(&gets[0]), None);
        assert_eq!((gets[0].range.offset, gets[0].range.len), (0, 1));
        assert_eq!(rig.engine.stats().fills, 0);
    }
    assert_eq!(rig.large.counters().counts().alone, 1);
}

/// A location of an object of `size` bytes with no generation and one legacy source.
fn silo_location(size: u64) -> Location {
    Location {
        generation: None,
        size: Some(size),
        sources: vec![legacy(size)],
        conflicts: Vec::new(),
    }
}

/// Asserts that a first part alone whose source does not hold the object, after `LOCATE` named
/// it, is `Error::Unavailable` (a `503`: a lost location is not a definitive miss, 10.6 step 3),
/// counted as unavailable and not as a miss, not remembered, and with the engine never asked.
#[tokio::test]
async fn a_first_part_that_the_source_does_not_hold_is_unavailable_not_a_miss() {
    let key = "cas/alone/gone";
    let rig = LargeRig::new("alone-gone");
    rig.upstream
        .script_locate(Ok(Located::Found(silo_location(3 * MIB))));
    let Err(error) = rig.read(key, None).await else {
        panic!("a read of an object the source lacks");
    };
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    let counts = rig.large.counters().counts();
    assert_eq!((counts.misses, counts.unavailable, counts.alone), (0, 1, 1));
    assert_eq!(rig.upstream.count(), 1);
    assert_eq!(rig.engine.stats().fills, 0);
    let (rig_bytes, etag) = (object(3 * MIB, 0xb4), generation(0xb4));
    rig.silo(key, &rig_bytes, etag);
    let (meta, _, served) = rig.read(key, None).await.expect("served once there");
    assert_eq!((meta.tag, served), (etag.to_string(), rig_bytes.to_vec()));
}

/// Asserts that a transport error and a `503` of the first part alone are `Error::Unavailable`,
/// counted as unavailable (not as misses), with the engine never asked.
#[tokio::test]
async fn an_error_of_the_first_part_is_unavailable() {
    let key = "cas/alone/error";
    let (rig, _) = silo_rig("alone-error", key, SIZE, 0xb2);
    let failures = [
        UpstreamError::Transport {
            key: PackKey(key.to_owned()),
            device: DeviceRef::Legacy,
            reason: "reset".to_owned(),
        },
        UpstreamError::Unavailable {
            key: PackKey(key.to_owned()),
            device: DeviceRef::Legacy,
            retry_after_ms: 10,
            signals: DriveSignals::default(),
        },
    ];
    for (round, failure) in failures.into_iter().enumerate() {
        rig.upstream.fake.script_part(PartScript::Fail(failure));
        let Err(error) = rig.read(key, None).await else {
            panic!("a read whose first part fails");
        };
        assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
        let counts = rig.large.counters().counts();
        assert_eq!(counts.unavailable, round as u64 + 1);
        assert_eq!((counts.misses, counts.alone), (0, round as u64 + 1));
    }
    assert_eq!(rig.engine.stats().fills, 0);
}

/// Asserts that a first part whose body ends in a transport error before the bytes the read wants
/// fails the read with an error, and that what was served before it is a prefix of the object.
#[tokio::test]
async fn a_first_part_that_ends_early_fails_the_read() {
    let key = "cas/alone/truncated";
    let (rig, bytes) = silo_rig("alone-truncated", key, SIZE, 0xb3);
    rig.upstream
        .fake
        .script_part(PartScript::Truncate { after: 1000 });
    let (served, error) = match rig.open(key, None).await {
        Ok(opened) => drain(opened.body).await,
        Err(error) => (Vec::new(), Some(error)),
    };
    assert!(matches!(error, Some(Error::Unavailable(_))), "{error:?}");
    assert!(served.len() <= 1000);
    assert_eq!(served, bytes[..served.len()].to_vec());
}

/// Asserts that a silo object of exactly `whole_fill_limit` bytes takes the path of `obj/` (its
/// bytes are the cache's upstream's, no first part alone is sent and nothing is routed), and one
/// byte more goes alone (its bytes are the fake's, one GET with no `If-Match`, no fill).
#[tokio::test]
async fn the_limit_is_the_last_size_of_obj_and_one_more_byte_goes_alone() {
    let rig = LargeRig::new("alone-limit");
    let (small, large) = ("cas/alone/limit", "cas/alone/limit-plus-one");
    let (small_obj, large_obj) = (object(LIMIT, 1), object(LIMIT + 1, 2));
    rig.silo(small, &small_obj, generation(0xb4));
    rig.silo(large, &large_obj, generation(0xb5));
    let (cache_small, cache_large) = (object(LIMIT, 5), object(LIMIT + 1, 6));
    rig.rig.put(small, &cache_small).await;
    rig.rig.put(large, &cache_large).await;

    let (meta, _, served) = rig.read(small, None).await.expect("the small read");
    assert_eq!(served, cache_small.to_vec(), "served from obj/");
    assert_eq!(meta.size, LIMIT);
    assert!(rig.gets().is_empty());
    let counts = rig.large.counters().counts();
    assert_eq!((counts.small, counts.routed, counts.alone), (1, 0, 0));

    let (meta, _, served) = rig.read(large, None).await.expect("the large read");
    assert_eq!(served, large_obj.to_vec(), "served through the front");
    assert_eq!(meta.tag, text(0xb5));
    let gets = rig.gets();
    assert_eq!(gets.len(), 1);
    assert_eq!(if_match(&gets[0]), None);
    assert_eq!((gets[0].range.offset, gets[0].range.len), (0, LIMIT + 1));
    assert_eq!(rig.engine.stats().fills, 0);
    let counts = rig.large.counters().counts();
    assert_eq!((counts.small, counts.routed, counts.alone), (1, 1, 1));
}
