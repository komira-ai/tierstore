//! The walk of a listing with a delimiter over a proxy that reads markers: the common prefixes are
//! made of the keys the proxy lists, so a prefix whose cold keys are all deleted is not one.

use std::sync::Arc;

use bytes::Bytes;

use super::listing::page;
use crate::{
    Backend, Marker, MarkerState, MemoryBackend, Proxy, ProxyConfig, SystemClock, encode_marker,
};

fn gone(key: &str) -> Bytes {
    encode_marker(&Marker {
        v: 1,
        key: key.to_owned(),
        st: MarkerState::Dirty,
        gone: true,
        dirty: true,
        why: None,
        owner: None,
        nonce: "n".to_owned(),
        extra: serde_json::Map::new(),
    })
}

/// Asserts that a listing with a delimiter, over a proxy in the phase `active-marks`, has no common
/// prefix made only of keys the cold tier lists and the markers say are deleted, and has the prefix
/// of a key that is live, so that the design's probe of such prefixes is not needed: the walk folds
/// the keys the proxy lists, and the proxy lists no deleted key.
#[tokio::test]
async fn a_common_prefix_made_only_of_gone_cold_keys_is_not_listed() {
    let hot = Arc::new(MemoryBackend::new());
    let cold = Arc::new(MemoryBackend::new());
    let marks = Arc::new(MemoryBackend::new());
    let format = r#"{"epoch":3,"marks_bucket":"hot-marks","phase":"active-marks"}"#;
    hot.put("_tiering/format", Bytes::from_static(format.as_bytes()))
        .await
        .unwrap();
    for key in ["d/1", "d/2", "e/1", "f/1", "top"] {
        cold.put(key, Bytes::from_static(b"cold")).await.unwrap();
    }
    for key in ["d/1", "d/2"] {
        marks.put(key, gone(key)).await.unwrap();
    }
    hot.put("f/1", Bytes::from_static(b"hot")).await.unwrap();
    let mut config = ProxyConfig::new("p1", 1 << 20, Arc::new(SystemClock));
    config.marks = Some(marks);
    let proxy = Proxy::open(hot, cold, config).await.unwrap();

    let delimiter = Some("/");
    let listed = page(&proxy, "", delimiter, None, 10, 11).await.unwrap();
    let keys: Vec<&str> = listed.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["top"]);
    assert_eq!(listed.prefixes, ["e/", "f/"]);
    assert!(listed.next.is_none());

    // One entry to a page: the pages walk the same prefixes, and the deleted ones are never one.
    let first = page(&proxy, "", delimiter, None, 1, 2).await.unwrap();
    assert_eq!(first.prefixes, ["e/"]);
    assert!(first.next.is_some());
}
