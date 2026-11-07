//! Puts: a new key, an overwrite, the same bytes, a create-only put that finds the key, and one
//! that meets a delete.

use tierstore::{Gate, GateBackend, Phase, PutBody, PutClaim};

use super::*;

/// The key of a test.
const K: &str = "builds/k";

/// A held plain put of `text`.
async fn plain(proxy: &Proxy, key: &str, text: &str) -> tierstore::PlainPut {
    let claim = PutClaim::default();
    proxy
        .put_plain(key, PutBody::Held(body(text)), &claim)
        .await
        .unwrap()
}

/// Asserts what a create-only put of a new key asks of the stores: the hot HEAD, the marker GET and
/// the cold HEAD together, then the object and the marker together, and nothing else. No request
/// names an entry, none goes to the cold tier but the HEAD, the marker is `DIRTY` and does not
/// say `gone`, and the object is in the hot tier alone.
#[tokio::test]
async fn a_create_only_put_of_a_new_key_asks_for_one_head_and_one_marker_then_writes_both() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    let mut seen = world.cursor();
    proxy.put(K, body("v1")).await.unwrap();
    assert_eq!(
        world.since(&mut seen),
        [
            req(Store::Hot, Op::Head, K),
            req(Store::Hot, Op::Put, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Marks, Op::Put, K),
            req(Store::Cold, Op::Head, K),
        ]
    );
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.cold_text(K).await, None);
    assert!(world.hot_mem.list("_entries/").await.unwrap().is_empty());
}

/// Asserts what a plain put of a new key asks of the stores: the hot HEAD together with the
/// marker GET (the look an overwrite needs), then the object and the marker, and no request to
/// the cold tier (a plain put never asks it). The marker GET is the one request more than the two
/// of the design's table 6.1, which counts a plain put's first wave as the HEAD alone.
#[tokio::test]
async fn a_plain_put_of_a_new_key_asks_for_the_head_and_the_marker_then_writes_both_and_never_asks_cold()
 {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    let mut seen = world.cursor();
    let put = plain(&proxy, K, "v1").await;
    assert!(put.wrote);
    assert_eq!(
        world.since(&mut seen),
        [
            req(Store::Hot, Op::Head, K),
            req(Store::Hot, Op::Put, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Marks, Op::Put, K),
        ]
    );
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v1"));
}

/// Asserts that a put of a streamed body takes the same path: the object lands in the hot tier
/// with its marker, and the answer's tag is the tag a HEAD gives.
#[tokio::test]
async fn a_streamed_plain_put_of_a_new_key_lands_the_object_and_a_dirty_marker() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    let chunks: Vec<Result<bytes::Bytes, Error>> = vec![Ok(body("ab")), Ok(body("cd"))];
    let stream = futures::StreamExt::boxed(futures::stream::iter(chunks));
    let put = proxy
        .put_plain(
            K,
            PutBody::Stream {
                len: Some(4),
                body: stream,
            },
            &PutClaim::default(),
        )
        .await
        .unwrap();
    assert_eq!(put.tag, Some(proxy.head(K).await.unwrap().tag));
    assert_eq!(world.hot_text(K).await.as_deref(), Some("abcd"));
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
}

/// Asserts what an overwrite of a key whose marker is `DIRTY` (a re-put soon after a put) asks of
/// the stores: the hot HEAD with the marker GET, the compare-and-swap of the marker to a lock, the
/// replace by the tag of the HEAD, and, after the answer, the release of the lock to `DIRTY`. The
/// replace names the tag (one `If-Match` write), nothing is asked of the cold tier, and the
/// marker does not say `gone`.
#[tokio::test]
async fn an_overwrite_of_a_key_with_a_dirty_marker_locks_it_by_compare_and_swap_and_replaces_by_tag()
 {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    plain(&proxy, K, "v1").await;
    let mut seen = world.cursor();
    let put = plain(&proxy, K, "v2").await;
    assert!(put.wrote);
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v2"));
    released(&world, K).await;
    let requests = world.since(&mut seen);
    assert_eq!(
        requests,
        [
            req(Store::Hot, Op::Head, K),
            req(Store::Hot, Op::PutIfMatch, K),
            req(Store::Hot, Op::Get, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Marks, Op::PutIfMatch, K),
            req(Store::Marks, Op::PutIfMatch, K),
        ]
    );
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v2"));
}

/// Asserts what an overwrite of a key with no marker (the leader cleared it after the copy) asks
/// of the stores: the hot HEAD with the marker GET, the create-only lock, the replace, and the
/// release by compare-and-swap; and that the cold copy is not touched, so that it is now older
/// than hot, which the marker `DIRTY` says.
#[tokio::test]
async fn an_overwrite_of_a_copied_key_creates_the_lock_and_leaves_dirty_what_cold_lacks() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    plain(&proxy, K, "v1").await;
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);
    assert_eq!(world.shape(K).await, None);
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));

    let mut seen = world.cursor();
    plain(&proxy, K, "v2").await;
    released(&world, K).await;
    let requests = world.since(&mut seen);
    let marks: Vec<_> = requests
        .iter()
        .filter(|(store, ..)| *store == Store::Marks)
        .map(|(_, op, _)| *op)
        .collect();
    assert_eq!(marks, [Op::Get, Op::Put, Op::PutIfMatch]);
    assert_eq!(count(&requests, Store::Cold, Op::Put), 0);
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v2"));
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
}

/// Asserts that a put whose body the caller says has the tag and the size of the object the key
/// holds writes nothing: the hot HEAD and the marker GET are all it asks, the answer carries the
/// tag the key has, and `wrote` is false.
#[tokio::test]
async fn a_put_of_the_same_bytes_with_a_claim_asks_two_reads_and_writes_nothing() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    plain(&proxy, K, "v1").await;
    let tag = proxy.head(K).await.unwrap().tag;
    let mut seen = world.cursor();
    let claim = PutClaim {
        tags: vec![tag.clone()],
    };
    let put = proxy
        .put_plain(K, PutBody::Held(body("v1")), &claim)
        .await
        .unwrap();
    assert_eq!((put.wrote, put.tag), (false, Some(tag)));
    assert_eq!(
        world.since(&mut seen),
        [req(Store::Hot, Op::Head, K), req(Store::Marks, Op::Get, K)]
    );
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
}

/// Asserts that a create-only put of a key that exists is refused as `AlreadyExists` after the
/// one re-read of the marker (the re-check of 5.1: `412` only when no delete is in flight), with
/// no write to any store.
#[tokio::test]
async fn a_create_only_put_of_a_key_that_exists_is_refused_after_one_marker_re_read_and_writes_nothing()
 {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    let mut seen = world.cursor();
    let again = proxy.put(K, body("v2")).await;
    assert_eq!(again, Err(Error::AlreadyExists(K.to_owned())));
    assert_eq!(
        world.since(&mut seen),
        [
            req(Store::Hot, Op::Head, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Cold, Op::Head, K),
        ]
    );
    assert_eq!(world.hot_text(K).await.as_deref(), Some("v1"));
}

/// The gate that holds the hot DELETE of the deleter `p1` before the store has seen it, and the
/// two proxies: `p1` over a hot tier that can hold a call, `p2` over the hot tier itself.
async fn a_delete_in_flight(world: &World) -> (Arc<Proxy>, Arc<Proxy>, Gate) {
    let held = Arc::new(GateBackend::new(world.hot.clone()));
    let gate = held.gate(Op::Delete, K, Phase::Pre);
    let p1 = world.proxy_over(held, world.config("p1")).await;
    let p2 = world.proxy("p2").await;
    (p1, p2, gate)
}

/// Asserts that a create-only put of a key that a delete holds, between its lock and its hot
/// DELETE, is answered `Conflict` (409) and not `AlreadyExists` (412), and writes nothing; that
/// the delete then finishes and the key is gone; and that the same put made after it creates the
/// key again, over the cold copy that still stands.
#[tokio::test]
async fn a_create_only_put_that_meets_a_delete_is_409_and_the_same_put_after_the_delete_is_free() {
    let world = World::new().await;
    let (p1, p2, gate) = a_delete_in_flight(&world).await;
    p1.put(K, body("old")).await.unwrap();
    assert_eq!(world.leader("r1").step().await.changed, 1);
    let deleter = tokio::spawn({
        let p1 = Arc::clone(&p1);
        async move { p1.delete(K).await }
    });
    gate.parked().await;
    assert_eq!(
        world.shape(K).await,
        Some((MarkerState::Lock, true)),
        "the delete holds the key and has written gone"
    );

    let writes_before = world.hot.calls(Op::Put);
    let refused = p2.put(K, body("new")).await;
    assert_eq!(refused, Err(Error::Conflict(K.to_owned())));
    assert_eq!(world.hot.calls(Op::Put), writes_before);

    assert!(gate.release());
    deleter.await.unwrap().unwrap();
    assert_eq!(served(&p2, K).await, Err(Error::NotFound(K.to_owned())));
    assert_eq!(world.cold_text(K).await.as_deref(), Some("old"));

    p2.put(K, body("new")).await.unwrap();
    assert_eq!(served(&p2, K).await.as_deref(), Ok("new"));
}

/// Asserts that a create-only put of a key only the cold tier holds, with no marker, is refused as
/// `AlreadyExists` (S3 sees a key wherever it lives), and that the same put of a key the cold tier
/// holds and a `gone` marker deleted is free: the cold copy is a ghost.
#[tokio::test]
async fn a_create_only_put_sees_a_key_only_cold_holds_unless_a_gone_marker_deleted_it() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    world.cold_mem.put(K, body("cold")).await.unwrap();
    assert_eq!(
        proxy.put(K, body("new")).await,
        Err(Error::AlreadyExists(K.to_owned()))
    );
    assert_eq!(world.hot_text(K).await, None);

    proxy.delete(K).await.unwrap();
    released(&world, K).await;
    proxy.put(K, body("new")).await.unwrap();
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("new"));
}

/// Asserts the cost the design accepts for a create-only put, and no other (I15): with the cold tier
/// down a create-only put of a new key is `Unavailable` and writes nothing, and a plain put of a new
/// key and an overwrite are made all the same, since a plain put never asks the cold tier.
#[tokio::test]
async fn a_create_only_put_is_unavailable_while_cold_is_down_and_a_plain_put_is_not() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    plain(&proxy, K, "v1").await;
    world.cold.set_down(true);
    let refused = proxy.put("builds/new", body("x")).await;
    assert!(matches!(refused, Err(Error::Unavailable(_))), "{refused:?}");
    assert_eq!(world.hot_text("builds/new").await, None);
    assert_eq!(world.marker("builds/new").await, None);

    plain(&proxy, "builds/other", "y").await;
    plain(&proxy, K, "v2").await;
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v2"));
    assert_eq!(world.cold.calls(Op::Put), 0);
}

/// Asserts that a multipart upload of a new key completes on the fast path: the parts go to the hot
/// tier, the completion creates the object and the `DIRTY` marker together, and the object is read
/// back whole.
#[tokio::test]
async fn a_multipart_upload_of_a_new_key_completes_with_a_dirty_marker() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    let id = proxy.create_upload(K).await.unwrap();
    let mut parts = Vec::new();
    for (number, text) in [(1, "hello "), (2, "world")] {
        let chunks: Vec<Result<bytes::Bytes, Error>> = vec![Ok(body(text))];
        let stream = futures::StreamExt::boxed(futures::stream::iter(chunks));
        let tag = proxy.upload_part(K, &id, number, stream).await.unwrap();
        parts.push((number, tag));
    }
    proxy.complete_upload(K, &id, &parts).await.unwrap();
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("hello world"));
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
    assert_eq!(world.cold_text(K).await, None);
}
