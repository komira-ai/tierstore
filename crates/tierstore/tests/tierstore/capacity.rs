//! The hot tier's capacity as the proxy sees it: the critical mark, a full hot tier, and the
//! usage the reconciler publishes.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{ByteRange, Error, Mono, Op};
use tokio::time::Instant;

use crate::support::{Rig, assert_absent, body, read, set_usage};
use tierstore::Backend;

/// Asserts that a put made while the published usage is at or above the critical share of the
/// budget and under the budget takes exactly as much of the paused clock as a put made under the
/// share (the journal's linger and nothing more), and the object is in the hot tier. (Before,
/// such a put slept a fixed back-pressure on top.)
#[tokio::test(start_paused = true)]
async fn a_put_above_the_critical_mark_goes_to_the_hot_tier_without_waiting() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.critical = 0.5;
    let proxy = rig.open(config).await.unwrap();

    set_usage(&rig, 100, 0).await;
    let start = Instant::now();
    proxy.put("a", body("1")).await.unwrap();
    let under = start.elapsed();

    set_usage(&rig, 600, 0).await;
    let start = Instant::now();
    proxy.put("b", body("2")).await.unwrap();
    assert_eq!(start.elapsed(), under);
    assert_eq!(read(&*rig.hot_mem, "b").await, body("2"));
}

/// Asserts that while the published usage is at the budget new objects go straight to the cold
/// tier, create-only and unjournaled; that reads work from both tiers and copy nothing back;
/// that a put is still refused where either tier holds the key; and that the hot tier is used
/// again once the usage falls.
#[tokio::test(start_paused = true)]
async fn when_the_usage_is_at_the_budget_puts_go_straight_to_the_cold_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("h", body("hot")).await.unwrap();
    set_usage(&rig, 1000, 0).await;

    proxy.put("k", body("direct")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k").await, body("direct"));
    assert_absent(&*rig.hot_mem, "k").await;

    assert_eq!(proxy.get("k", None).await.unwrap(), body("direct"));
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(proxy.get("h", None).await.unwrap(), body("hot"));

    let in_cold = proxy.put("k", body("again")).await;
    assert_eq!(in_cold, Err(Error::AlreadyExists("k".to_owned())));
    let in_hot = proxy.put("h", body("again")).await;
    assert_eq!(in_hot, Err(Error::AlreadyExists("h".to_owned())));

    set_usage(&rig, 10, 0).await;
    proxy.put("m", body("back")).await.unwrap();
    assert_eq!(read(&*rig.hot_mem, "m").await, body("back"));
}

/// Asserts that a put made while the hot tier is full is an error when the hot tier cannot
/// answer whether it holds the key, and not an object written to the cold tier.
#[tokio::test(start_paused = true)]
async fn a_put_to_the_cold_tier_needs_the_hot_tier_to_say_it_lacks_the_key() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    set_usage(&rig, 1000, 0).await;
    let down = Error::Unavailable("hot".to_owned());
    rig.hot.fail_next_on(Op::Head, "k", down.clone());

    let failed = proxy.put("k", body("value")).await;
    assert_eq!(failed, Err(down));
    assert_absent(&*rig.cold_mem, "k").await;
}

/// Asserts that a usage object that is missing, that is not readable, or that has stood unchanged
/// for longer than `usage_stale_after` on the proxy's own monotonic clock counts as not full (the
/// put goes to the hot tier), that a usage at the budget is believed while it is within the
/// limit (and at the limit itself), and that a rewritten object is believed again. The clock of
/// the stores, which stamps the `time` of the object, is never read.
#[tokio::test(start_paused = true)]
async fn a_usage_that_is_missing_unreadable_or_stale_counts_as_not_full() {
    let rig = Rig::new();
    let mono = Arc::new(Mono::new());
    let mut config = rig.config("p1", 1000);
    config.mono = mono.clone();
    let proxy = rig.open(config).await.unwrap();
    proxy.put("a", body("missing")).await.unwrap();

    let garbage = body("not json");
    rig.hot_mem.put("_tiering/usage", garbage).await.unwrap();
    proxy.put("b", body("unreadable")).await.unwrap();

    set_usage(&rig, 1000, 0).await;
    rig.clock.set(100_000);
    proxy.put("e", body("within")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "e").await, body("within"));
    mono.advance(Duration::from_secs(30));
    proxy.put("f", body("at the limit")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "f").await, body("at the limit"));
    mono.advance(Duration::from_millis(1));
    proxy.put("c", body("stale")).await.unwrap();
    for key in ["a", "b", "c"] {
        assert_absent(&*rig.cold_mem, key).await;
        assert!(rig.hot_mem.head(key).await.is_ok());
    }

    set_usage(&rig, 1000, 1).await;
    proxy.put("d", body("fresh")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "d").await, body("fresh"));
    assert_absent(&*rig.hot_mem, "d").await;
}

/// Asserts that a failed read of the usage object does not restart the age of the body that was
/// read before it: a usage that has stood unchanged past `usage_stale_after` stays stale when the
/// same body is read again after one read failed, while the failed read itself counts as not
/// full.
#[tokio::test(start_paused = true)]
async fn a_failed_usage_read_does_not_make_an_unchanged_stale_usage_fresh() {
    let rig = Rig::new();
    let mono = Arc::new(Mono::new());
    let mut config = rig.config("p1", 1000);
    config.mono = mono.clone();
    let proxy = rig.open(config).await.unwrap();
    set_usage(&rig, 1000, 0).await;
    rig.clock.set(100_000);
    proxy.put("a", body("fresh")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "a").await, body("fresh"));

    mono.advance(Duration::from_secs(31));
    rig.hot.fail_next_on(
        Op::Get,
        "_tiering/usage",
        Error::Unavailable("down".to_owned()),
    );
    proxy.put("b", body("failed read")).await.unwrap();
    assert!(rig.hot_mem.head("b").await.is_ok());

    proxy.put("c", body("same body again")).await.unwrap();
    assert!(rig.hot_mem.head("c").await.is_ok());
    assert_absent(&*rig.cold_mem, "c").await;
}

/// Asserts that when the hot tier answers a put with a capacity error the object goes to the
/// cold tier instead, that the next puts go there without asking the hot tier, and that the
/// hot tier is tried again after `usage_refresh`.
#[tokio::test(start_paused = true)]
async fn a_capacity_error_from_the_hot_tier_sends_puts_to_cold_until_the_refresh() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.usage_refresh = Duration::from_secs(10);
    let proxy = rig.open(config).await.unwrap();
    let full = Error::Full("hot".to_owned());
    rig.hot.fail_next_on(Op::Put, "k1", full);

    proxy.put("k1", body("1")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k1").await, body("1"));
    assert_absent(&*rig.hot_mem, "k1").await;

    proxy.put("k2", body("2")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k2").await, body("2"));
    assert_absent(&*rig.hot_mem, "k2").await;

    tokio::time::sleep(Duration::from_secs(11)).await;
    proxy.put("k3", body("3")).await.unwrap();
    assert_eq!(read(&*rig.hot_mem, "k3").await, body("3"));
    assert_absent(&*rig.cold_mem, "k3").await;
}

/// Asserts that a get of an object only the cold tier holds asks for no copy-back while the usage
/// is above the critical mark, and asks for one (an entry of the key) once the usage is under it;
/// the read copies nothing itself.
#[tokio::test(start_paused = true)]
async fn a_get_does_not_copy_back_above_the_critical_mark() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.critical = 0.5;
    let proxy = rig.open(config).await.unwrap();
    rig.cold_mem.put("k", body("value")).await.unwrap();

    set_usage(&rig, 600, 0).await;
    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_absent(&*rig.hot_mem, "k").await;
    assert!(rig.hot_mem.head(&tierstore::entry_key("k")).await.is_err());

    set_usage(&rig, 100, 0).await;
    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    crate::support::copy_back_asked(&rig, "k").await;
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts that a ranged get of an object only the cold tier holds asks the cold tier for the
/// range alone, above the critical mark and under it, and copies nothing back: the memory of the
/// read is its range, whatever the size of the object.
#[tokio::test(start_paused = true)]
async fn a_ranged_get_reads_only_the_range_from_the_cold_tier_at_any_usage() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.critical = 0.5;
    let proxy = rig.open(config).await.unwrap();
    rig.cold_mem.put("k", body("0123456789")).await.unwrap();

    set_usage(&rig, 600, 0).await;
    let part = ByteRange::new(2, 3);
    assert_eq!(proxy.get("k", Some(part)).await.unwrap(), body("234"));
    assert_eq!(rig.cold.get_ranges(), vec![Some((2, 3))]);
    assert_absent(&*rig.hot_mem, "k").await;

    set_usage(&rig, 100, 0).await;
    assert_eq!(proxy.get("k", Some(part)).await.unwrap(), body("234"));
    assert_eq!(rig.cold.get_ranges(), vec![Some((2, 3)), Some((2, 3))]);
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts that a read whose copy back into the hot tier fails (the hot tier is full, or
/// another read got there first) still returns the object, and journals nothing.
#[tokio::test(start_paused = true)]
async fn a_failed_copy_back_does_not_fail_the_read() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k1", body("one")).await.unwrap();
    rig.cold_mem.put("k2", body("two")).await.unwrap();
    let full = Error::Full("hot".to_owned());
    rig.hot.fail_next_on(Op::Put, "k1", full);
    let raced = Error::AlreadyExists("k2".to_owned());
    rig.hot.fail_next_on(Op::Put, "k2", raced);

    assert_eq!(proxy.get("k1", None).await.unwrap(), body("one"));
    assert_eq!(proxy.get("k2", None).await.unwrap(), body("two"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_absent(&*rig.hot_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "k2").await;
}

/// Asserts that the usage object is read at most once for each `usage_refresh`: a change of
/// the usage is not seen by a put made before the interval has passed, and is seen by one
/// made after it.
#[tokio::test(start_paused = true)]
async fn the_usage_is_read_once_for_each_refresh_interval() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.usage_refresh = Duration::from_secs(10);
    // The format object is not read again either, so that the reads counted are the usage's.
    config.format_refresh = Duration::from_secs(3600);
    let proxy = rig.open(config).await.unwrap();

    proxy.put("a", body("1")).await.unwrap();
    let reads = rig.hot.calls(Op::Get);
    set_usage(&rig, 1000, 0).await;
    proxy.put("b", body("2")).await.unwrap();
    assert_eq!(rig.hot.calls(Op::Get), reads);
    assert_eq!(read(&*rig.hot_mem, "b").await, body("2"));

    tokio::time::sleep(Duration::from_secs(11)).await;
    proxy.put("c", body("3")).await.unwrap();
    assert_eq!(rig.hot.calls(Op::Get), reads + 1);
    assert_eq!(read(&*rig.cold_mem, "c").await, body("3"));
    assert_absent(&*rig.hot_mem, "c").await;
}
