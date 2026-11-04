//! The proxy when a tier fails.

use tierstore::{Error, Op};

use crate::support::{Rig, assert_absent, body, read};
use tierstore::Backend;

/// Asserts that a read or a head the hot tier fails with an outage is that error, and is not
/// answered from the cold tier.
#[tokio::test(start_paused = true)]
async fn a_hot_outage_is_reported_by_reads_and_heads() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("v")).await.unwrap();
    rig.hot.set_down(true);

    let read = proxy.get("k", None).await.unwrap_err();
    assert!(read.is_unavailable());
    let head = proxy.head("k").await.unwrap_err();
    assert!(head.is_unavailable());
    assert_eq!(rig.cold.calls(Op::Get), 0);
    assert_eq!(rig.cold.calls(Op::Head), 0);
}

/// Asserts that a get and a head of an object only the cold tier holds fail with the cold
/// tier's error when it is down, and that the failed get copies nothing into the hot tier.
#[tokio::test(start_paused = true)]
async fn a_cold_outage_is_reported_by_reads_of_cold_objects() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold.set_down(true);

    let read = proxy.get("k", None).await.unwrap_err();
    assert!(read.is_unavailable());
    let head = proxy.head("k").await.unwrap_err();
    assert!(head.is_unavailable());
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts that a put is refused with the cold tier's error when the cold tier cannot say
/// whether it holds the key, that nothing is written, and that hot objects can
/// still be read.
#[tokio::test(start_paused = true)]
async fn a_put_fails_when_the_cold_tier_cannot_be_asked() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("h", body("hot")).await.unwrap();
    rig.cold.set_down(true);

    let failed = proxy.put("k", body("value")).await.unwrap_err();
    assert!(failed.is_unavailable());
    assert_absent(&*rig.hot_mem, "k").await;

    assert_eq!(proxy.get("h", None).await.unwrap(), body("hot"));
}

/// Asserts that a put the hot tier fails (other than for lack of room) is an error that a client
/// retries (`Unavailable`) and leaves no object.
#[tokio::test(start_paused = true)]
async fn a_put_the_hot_tier_fails_is_not_acknowledged() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let down = Error::Unavailable("hot".to_owned());
    rig.hot.fail_next_on(Op::Put, "k", down.clone());

    let failed = proxy.put("k", body("value")).await.unwrap_err();
    assert!(failed.is_unavailable(), "{failed:?} for {down:?}");
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts that a listing, whole or a page, fails if either tier fails to list, instead of
/// returning the other tier's keys alone.
#[tokio::test(start_paused = true)]
async fn a_listing_fails_if_either_tier_fails() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("v")).await.unwrap();

    rig.cold.fail_next(Op::List, 1);
    assert!(proxy.list("").await.unwrap_err().is_unavailable());
    rig.hot.fail_next(Op::List, 1);
    assert!(proxy.list("").await.unwrap_err().is_unavailable());

    rig.cold.fail_next(Op::List, 1);
    let paged = proxy.list_page("", None, 10).await;
    assert!(paged.unwrap_err().is_unavailable());
    rig.hot.fail_next(Op::List, 1);
    let paged = proxy.list_page("", None, 10).await;
    assert!(paged.unwrap_err().is_unavailable());

    assert_eq!(proxy.list("").await.unwrap().len(), 1);
}
