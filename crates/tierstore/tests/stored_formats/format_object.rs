//! The format object: the body kept under `_tiering/format`, which pauses writes.

use std::time::Duration;

use tierstore::{Error, Proxy, ProxyConfig};

use crate::support::Tiers;
use tierstore::Backend as _;

const FORMAT: &str = "_tiering/format";
const DRAIN: &[u8] = include_bytes!("fixtures/format_drain.json");
const DRAINED: &[u8] = include_bytes!("fixtures/format_drained.json");
const ACTIVE: &[u8] = include_bytes!("fixtures/format_active.json");
const NEWER: &[u8] = include_bytes!("fixtures/format_newer.json");

/// A proxy that reads the format object before every write.
async fn eager(tiers: &Tiers) -> Proxy {
    let mut config = ProxyConfig::new("p1", 1000, tiers.clock.clone());
    config.format_refresh = Duration::ZERO;
    Proxy::open(tiers.hot.clone(), tiers.cold.clone(), config)
        .await
        .unwrap()
}

/// Asserts that the drain and drained fixtures pause a put with `Unavailable` and the pause text,
/// that the fixture of a newer writer (a phase, an epoch and a field this release does not know)
/// does not pause them but does not make them on entries either (the answer is `Unavailable` with
/// the text that says writes wait for the active format), and that the active fixture makes the
/// put on an entry, which a proxy that has not beaten is not live for.
#[tokio::test(start_paused = true)]
async fn the_fixtures_pause_writes_as_their_phases_say() {
    for fixture in [DRAIN, DRAINED] {
        let tiers = Tiers::new();
        tiers.seed(FORMAT, fixture).await;
        let put = eager(&tiers).await.put("k", "v".into()).await;
        let Err(Error::Unavailable(text)) = put else {
            panic!("{fixture:?}: {put:?}");
        };
        assert!(text.contains("writes are paused"), "{fixture:?}: {text}");
    }
    let tiers = Tiers::new();
    tiers.seed(FORMAT, NEWER).await;
    let put = eager(&tiers).await.put("k", "v".into()).await;
    let Err(Error::Unavailable(text)) = put else {
        panic!("{put:?}");
    };
    assert!(
        text.contains("writes wait until the format object says active"),
        "{text}"
    );
    let tiers = Tiers::new();
    tiers.seed(FORMAT, ACTIVE).await;
    let put = eager(&tiers).await.put("k", "v".into()).await;
    let Err(Error::Unavailable(text)) = put else {
        panic!("{put:?}");
    };
    assert!(text.contains("not live"), "{text}");
}
