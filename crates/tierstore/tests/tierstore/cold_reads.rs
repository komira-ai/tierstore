//! The proxy over a cold tier: reads and heads of the cold tier and what they count, and listings
//! of both tiers.

use tierstore::{Backend, ByteRange, Error, Op, Proxy};

use crate::support::{Rig, body, keys, page, set_usage};

/// The line of a metric of the proxy, as `Proxy::metrics` renders it, with its value.
fn metric(proxy: &Proxy, line: &str) -> String {
    let rendered = proxy.metrics();
    let found = rendered.lines().find(|l| l.starts_with(line));
    found.unwrap_or("").to_owned()
}

fn not_found(key: &str) -> Error {
    Error::NotFound(key.to_owned())
}

/// Asserts that a range past the end of an object, asked of the cold tier above the critical mark,
/// stays the error it is, counted as an error of the cold tier, and that the proxy makes no HEAD
/// of the cold tier for it.
#[tokio::test(start_paused = true)]
async fn a_range_past_the_end_of_an_object_is_an_error_without_a_head() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("0123456789")).await.unwrap();
    set_usage(&rig, 960, 0).await;
    let past = Some(ByteRange::new(11, 1));

    let error = proxy.get("k", past).await;
    assert!(matches!(error, Err(Error::Other(_))), "{error:?}");
    assert_eq!(rig.cold.calls(Op::Head), 0);
    let errors = "tierstore_read_errors_total{tier=\"cold\",op=\"get\"}";
    assert_eq!(metric(&proxy, errors), format!("{errors} 1"));
}

/// Asserts that a head that reaches the cold tier is counted as a hit of it when the object is
/// there, as a miss when it is not, and as an error when the call fails.
#[tokio::test(start_paused = true)]
async fn a_head_of_the_cold_tier_is_counted_as_a_hit_a_miss_or_an_error() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();

    assert_eq!(proxy.head("k").await.unwrap().size, 5);
    assert_eq!(proxy.head("absent").await, Err(not_found("absent")));
    rig.cold.fail_next(Op::Head, 1);
    assert!(proxy.head("k").await.unwrap_err().is_unavailable());
    for (line, count) in [
        ("tierstore_hits_total{tier=\"cold\",op=\"head\"}", 1),
        ("tierstore_misses_total{tier=\"cold\",op=\"head\"}", 1),
        ("tierstore_read_errors_total{tier=\"cold\",op=\"head\"}", 1),
    ] {
        assert_eq!(metric(&proxy, line), format!("{line} {count}"));
    }
}

/// Asserts that a listing shows the objects of both tiers once each, in key order, and pages
/// through them: a page is cut at `max_keys`, the next starts after the last key given, the hot
/// tier's entry is shown for a key both tiers hold, and a short page is the last.
#[tokio::test(start_paused = true)]
async fn a_listing_shows_the_objects_of_both_tiers_and_pages_through_them() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for key in ["a", "c", "e", "g"] {
        rig.cold_mem.put(key, body("v")).await.unwrap();
    }
    rig.cold_mem.put("d", body("cold")).await.unwrap();
    rig.hot_mem.put("d", body("hot")).await.unwrap();

    assert_eq!(keys(&proxy).await, ["a", "c", "d", "e", "g"]);
    assert_eq!(page(&proxy, None, 2).await, ["a", "c"]);
    assert_eq!(page(&proxy, Some("c"), 2).await, ["d", "e"]);
    assert_eq!(page(&proxy, Some("e"), 2).await, ["g"]);
    assert_eq!(page(&proxy, Some("g"), 2).await, Vec::<String>::new());
    assert_eq!(page(&proxy, None, 0).await, Vec::<String>::new());
    let listed = proxy.list("d").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].size, 3);
}

/// Asserts that a read error of the cold tier that is not "not found" is an error that is
/// counted, whether it is "unavailable" or another error, and a key the cold tier does not hold is
/// a miss.
#[tokio::test(start_paused = true)]
async fn a_failed_read_of_the_cold_tier_is_an_error_and_an_absent_key_is_a_miss() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();
    rig.cold.fail_next(Op::Get, 1);
    assert!(proxy.get("k", None).await.unwrap_err().is_unavailable());
    rig.cold
        .fail_next_with(Op::Get, Error::Other("odd".to_owned()));
    let odd = proxy.get("k", None).await;
    assert_eq!(odd, Err(Error::Other("odd".to_owned())));
    assert_eq!(proxy.get("absent", None).await, Err(not_found("absent")));
    let errors = "tierstore_read_errors_total{tier=\"cold\",op=\"get\"}";
    assert_eq!(metric(&proxy, errors), format!("{errors} 2"));
    let miss = "tierstore_misses_total{tier=\"cold\",op=\"get\"}";
    assert_eq!(metric(&proxy, miss), format!("{miss} 1"));
}
