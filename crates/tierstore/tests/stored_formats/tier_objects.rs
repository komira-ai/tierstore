//! What the proxy and the reconciler keep in the hot tier under `_entries/` and `_tiering/`: the
//! proxy marker, the usage and the lease, and the keys they are kept under.

use std::time::Duration;

use tierstore::{Backend as _, entry_key};

use crate::support::Tiers;

const USAGE: &[u8] = include_bytes!("fixtures/usage.json");
const USAGE_NEWER: &[u8] = include_bytes!("fixtures/usage_newer.json");
const LEASE: &str = include_str!("fixtures/lease_body.txt");
const ACTIVE: &str = r#"{"epoch":2,"phase":"active"}"#;
const PROXY_MARKER: &[u8] = include_bytes!("fixtures/proxy_marker.bin");

/// Reads `usage` as a proxy with a hot tier of 63 bytes at time 80 and checks what the usage
/// fixture says: the proxy has not seen it change yet (its age is zero on the proxy's own clock,
/// whatever time the fixture carries), and a new object goes straight to the cold tier because
/// the hot tier is full.
async fn read_usage(usage: &[u8]) {
    let tiers = Tiers::new();
    tiers.seed("_tiering/usage", usage).await;
    tiers.clock.set(80);
    let proxy = tiers.proxy_on_clock("p1", 63).await.unwrap();

    assert_eq!(proxy.usage_age().await, Some(Duration::ZERO));
    proxy.put("k", "direct".into()).await.unwrap();
    assert_eq!(tiers.cold_keys().await, ["k"]);
    let in_hot = tiers.hot_keys("k").await;
    assert!(in_hot.is_empty(), "{in_hot:?}");
}

/// Asserts that a proxy that finds the usage of the fixture (63 bytes in a hot tier of 63,
/// published at time 50) reads it: it says the usage has not changed for no time yet, and
/// writes a new object straight to the cold tier because the hot tier is full. Catches a usage
/// field that is read as another, or ignored.
#[tokio::test(start_paused = true)]
async fn a_proxy_reads_the_usage_fixture() {
    read_usage(USAGE).await;
}

/// Asserts that a usage with a field this release does not know (the fixture of a newer
/// release) is read as if the field were not there: the proxy reads the same age and the same
/// full hot tier as from the usage fixture.
#[tokio::test(start_paused = true)]
async fn a_proxy_reads_a_usage_with_a_field_added_later() {
    read_usage(USAGE_NEWER).await;
}

/// Asserts that opening a proxy writes a registration under `_tiering/proxies/<proxy_id>` that is
/// JSON with `proto` 1, a `boot` and `hb` 0 (a proxy that does not beat yet), and that opening over
/// the empty marker of the fixture, which a release before the heartbeat wrote, leaves it as it
/// is.
#[tokio::test(start_paused = true)]
async fn opening_a_proxy_writes_a_registration_and_leaves_an_empty_marker_as_it_is() {
    let tiers = Tiers::new();
    tiers.proxy("p1", 1000).await.unwrap();
    let stored = tiers.stored("_tiering/proxies/p1").await;
    let found: serde_json::Value = serde_json::from_slice(&stored).unwrap();
    assert_eq!(found["proto"], 2);
    assert_eq!(found["hb"], 0);
    assert!(found["boot"].as_str().is_some_and(|boot| !boot.is_empty()));

    tiers.seed("_tiering/proxies/p2", PROXY_MARKER).await;
    tiers.proxy("p2", 1000).await.unwrap();
    assert_eq!(tiers.stored("_tiering/proxies/p2").await, PROXY_MARKER);
}

/// Asserts that the lease body is the holder, a nonce of 32 hex digits and the count of bodies
/// written, one to a line, as the fixture is: with the nonce of the fixture put in, the body of
/// a first lease is the fixture byte for byte, and a renewal changes only the count.
#[tokio::test(start_paused = true)]
async fn the_lease_body_is_the_holder_a_nonce_and_a_count_as_the_fixture_is() {
    let tiers = Tiers::new();
    tiers.seed("_tiering/format", ACTIVE.as_bytes()).await;
    let mut reconciler = tiers.reconciler("r1", 1000);
    let nonce = LEASE.split('\n').nth(1).unwrap();

    assert!(reconciler.step().await.held);
    let first = String::from_utf8(tiers.stored("_tiering/lease").await).unwrap();
    let lines: Vec<&str> = first.split('\n').collect();
    assert_eq!(lines.len(), 3, "{first:?}");
    assert!(lines[1].len() == 32 && lines[1].bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(first.replace(lines[1], nonce), LEASE);

    // Less than the margin of the lease is left, so the step renews it.
    tiers.clock.set(10_001);
    assert!(reconciler.step().await.held);
    let second = String::from_utf8(tiers.stored("_tiering/lease").await).unwrap();
    assert_eq!(second.replace(lines[1], nonce), LEASE.replace("\n1", "\n2"));
}

/// Asserts that a lease object of the fixture, held by `r1`, keeps another reconciler out until
/// the same tag has stood for the ttl (30 seconds) on the other's own clock, counted from its first
/// look at the object, and is taken over then, which writes the new holder's name on the first
/// line. Catches a lease that is no longer judged by how long its object has stood unchanged.
#[tokio::test(start_paused = true)]
async fn a_lease_object_of_the_fixture_is_held_until_it_expires() {
    let tiers = Tiers::new();
    tiers.seed("_tiering/format", ACTIVE.as_bytes()).await;
    tiers.seed("_tiering/lease", LEASE.as_bytes()).await;
    let mut other = tiers.reconciler("r2", 1000);

    assert!(!other.step().await.held);
    tiers.clock.set(29_999);
    assert!(!other.step().await.held);
    tiers.clock.set(30_000);
    assert!(other.step().await.held);
    let taken = String::from_utf8(tiers.stored("_tiering/lease").await).unwrap();
    assert_eq!(taken.split('\n').next(), Some("r2"));
}

/// Asserts the keys the objects are kept under, which a release that reads an older tier needs
/// to find: the entry of a key is under `_entries/` by the digest of the key, and the format
/// object and the registration of a proxy are under `_tiering/`.
#[tokio::test(start_paused = true)]
async fn the_objects_are_kept_under_these_keys() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy_on_clock("p1", 1000).await.unwrap();
    proxy.put("a", "1".into()).await.unwrap();

    let kept = tiers.hot_keys("_").await;
    let want = [
        entry_key("a"),
        "_tiering/format".to_owned(),
        "_tiering/proxies/p1".to_owned(),
    ];
    assert_eq!(kept, want);
}
