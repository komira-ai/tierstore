//! Deletes: what a delete asks of the stores, that the key never returns, and that the leader
//! removes the cold copy and then the marker.

use std::time::Duration;

use super::*;

/// The key of a test.
const K: &str = "builds/k";

/// Asserts what a delete of a key whose marker is `DIRTY` (a delete soon after a put) asks of the
/// stores, in order: the marker GET, the compare-and-swap to the lock, the hot HEAD, the
/// compare-and-swap that writes `gone`, the hot DELETE, and, after the answer, the release. Not a
/// request goes to the cold tier. The marker ends `DIRTY` with `gone` set.
#[tokio::test]
async fn a_delete_of_a_key_with_a_dirty_marker_asks_the_marker_the_head_and_the_delete_and_not_cold()
 {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    let mut seen = world.cursor();
    proxy.delete(K).await.unwrap();
    released(&world, K).await;
    assert_eq!(
        world.since(&mut seen),
        [
            req(Store::Hot, Op::Head, K),
            req(Store::Hot, Op::Delete, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Marks, Op::PutIfMatch, K),
            req(Store::Marks, Op::PutIfMatch, K),
            req(Store::Marks, Op::PutIfMatch, K),
        ]
    );
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, true)));
}

/// Asserts what a delete of a copied key (no marker) asks of the stores: the marker GET, the
/// create-only lock, the hot HEAD, the compare-and-swap that writes `gone`, the hot DELETE, and
/// the release; nothing of the cold tier, whose copy is for the leader to delete.
#[tokio::test]
async fn a_delete_of_a_copied_key_creates_the_lock_and_leaves_the_cold_copy_to_the_leader() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    assert_eq!(world.leader("r1").step().await.changed, 1);
    let mut seen = world.cursor();
    proxy.delete(K).await.unwrap();
    released(&world, K).await;
    let requests = world.since(&mut seen);
    let marks: Vec<_> = requests
        .iter()
        .filter(|(store, ..)| *store == Store::Marks)
        .map(|(_, op, _)| *op)
        .collect();
    assert_eq!(marks, [Op::Get, Op::Put, Op::PutIfMatch, Op::PutIfMatch]);
    let hot: Vec<_> = requests
        .iter()
        .filter(|(store, ..)| *store == Store::Hot)
        .map(|(_, op, _)| *op)
        .collect();
    assert_eq!(hot, [Op::Head, Op::Delete]);
    assert_eq!(count(&requests, Store::Cold, Op::Delete), 0);
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, true)));
}

/// Asserts that a key a client deleted never returns: while the cold copy still stands and after
/// the leader deleted it, a GET, a HEAD and a listing find nothing, whatever the marker; and the
/// leader deletes the cold object, holds the marker for `tombstone_hold`, and then removes it.
#[tokio::test]
async fn a_deleted_key_never_returns_while_cold_holds_it_nor_after_the_leader_removes_it() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    let other = world.proxy("p2").await;
    proxy.put(K, body("v1")).await.unwrap();
    proxy.put("builds/keep", body("k")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 2);
    proxy.delete(K).await.unwrap();
    released(&world, K).await;

    for proxy in [&proxy, &other] {
        assert_eq!(served(proxy, K).await, Err(Error::NotFound(K.to_owned())));
        assert_eq!(proxy.head(K).await, Err(Error::NotFound(K.to_owned())));
        let listed = proxy.list("builds/").await.unwrap();
        let keys: Vec<_> = listed.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, ["builds/keep"]);
    }
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));

    // The leader deletes the cold object; the marker stays for the hold, so that a read that
    // began before cannot be served the old copy.
    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.cold_text(K).await, None);
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, true)));
    assert_eq!(served(&other, K).await, Err(Error::NotFound(K.to_owned())));

    world.pass(Duration::from_secs(3600));
    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.shape(K).await, None);
    assert_eq!(served(&other, K).await, Err(Error::NotFound(K.to_owned())));
    assert_eq!(other.head(K).await, Err(Error::NotFound(K.to_owned())));
}

/// Asserts that a delete of a key that exists nowhere is answered like one of a key that does,
/// asks the hot tier for the HEAD only (no hot DELETE), and leaves a `gone` marker.
#[tokio::test]
async fn a_delete_of_a_key_that_exists_nowhere_is_answered_ok_with_a_head_and_no_hot_delete() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    let mut seen = world.cursor();
    proxy.delete("nothing/here").await.unwrap();
    released(&world, "nothing/here").await;
    let requests = world.since(&mut seen);
    assert_eq!(count(&requests, Store::Hot, Op::Head), 1);
    assert_eq!(count(&requests, Store::Hot, Op::Delete), 0);
    assert_eq!(count(&requests, Store::Cold, Op::Delete), 0);
    assert_eq!(
        world.shape("nothing/here").await,
        Some((MarkerState::Dirty, true))
    );
}

/// Asserts that a key deleted and put again is the new object, read from the hot tier, while the
/// old cold copy stands, and that the leader then replaces the cold copy by the new object and
/// ends the tombstone: the old bytes are never served after the put was acknowledged.
#[tokio::test]
async fn a_key_put_again_after_its_delete_serves_the_new_bytes_and_the_leader_replaces_cold() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("old")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);
    proxy.delete(K).await.unwrap();
    released(&world, K).await;
    proxy.put(K, body("new")).await.unwrap();
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, true)));
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("new"));
    assert_eq!(world.cold_text(K).await.as_deref(), Some("old"));

    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.cold_text(K).await.as_deref(), Some("new"));
    world.pass(Duration::from_secs(3600));
    let step = leader.step().await;
    assert!(step.failed.is_empty(), "{step:?}");
    assert_eq!(world.shape(K).await, None);
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("new"));
}
