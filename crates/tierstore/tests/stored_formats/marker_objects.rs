//! The marker object: the body kept under the key's own name in the marks bucket.

use tierstore::{Marker, MarkerState, MarkerWhy, decode_marker, encode_marker};

const DIRTY: &[u8] = include_bytes!("fixtures/marker.json");
const LOCKED: &[u8] = include_bytes!("fixtures/marker_lock.json");
const NEWER: &[u8] = include_bytes!("fixtures/marker_newer.json");

fn dirty() -> Marker {
    Marker {
        v: 1,
        key: "models/a.bin".to_owned(),
        st: MarkerState::Dirty,
        gone: false,
        dirty: true,
        why: None,
        owner: None,
        nonce: "3f9c0d7a41b2e86f5a0c9d17e4b3a285".to_owned(),
        extra: serde_json::Map::new(),
    }
}

fn deleting() -> Marker {
    Marker {
        v: 1,
        key: "models/a.bin".to_owned(),
        st: MarkerState::Lock,
        gone: true,
        dirty: true,
        why: Some(MarkerWhy::Delete),
        owner: Some("node1-b7f3".to_owned()),
        nonce: "b1e5d2c80a794f36e8d1c05a7b2f4963".to_owned(),
        extra: serde_json::Map::new(),
    }
}

/// The marker of the fixture of a newer writer, as this release reads it.
fn newer() -> Marker {
    let extra = serde_json::json!({"lease_ms": 120_000, "note": {"x": [1, 2]}});
    Marker {
        v: 1,
        key: "models/a.bin".to_owned(),
        st: MarkerState::Other("FROZEN".to_owned()),
        gone: false,
        dirty: true,
        why: Some(MarkerWhy::Other("REBALANCE".to_owned())),
        owner: Some("node1-b7f3".to_owned()),
        nonce: "0a4c7e91d3b5f8260e1a9c3d5b7f2e84".to_owned(),
        extra: extra.as_object().unwrap().clone(),
    }
}

/// Asserts that the fixture of a dirty marker, which has no `why` and no `owner`, and the fixture
/// of a delete's lock, which has both and says the key is gone, read as those markers. Catches a
/// renamed or removed field, or a state or a reason spelled another way.
#[test]
fn the_marker_fixtures_read_as_a_dirty_marker_and_the_lock_of_a_delete() {
    assert_eq!(decode_marker(DIRTY).unwrap(), dirty());
    assert_eq!(decode_marker(LOCKED).unwrap(), deleting());
}

/// Asserts that writing those two markers gives the bytes of the fixtures exactly: the fields in
/// order, `why` and `owner` left out when they are not set. Catches a field that is renamed,
/// reordered or re-meant in what is written.
#[test]
fn the_dirty_marker_and_the_delete_lock_are_written_as_the_marker_fixtures() {
    assert_eq!(&encode_marker(&dirty())[..], DIRTY);
    assert_eq!(&encode_marker(&deleting())[..], LOCKED);
}

/// Asserts that a marker a newer writer made, with a state, a reason and two fields this reader
/// does not know, still reads: the known fields are kept, the unknown words are kept as written,
/// and the unknown fields are kept with their values; and that writing it back, as a release does
/// that changes `gone` and nothing else, keeps all of them.
#[test]
fn the_marker_fixtures_read_and_an_added_field_state_and_why_still_read_and_are_kept_on_rewrite() {
    assert_eq!(decode_marker(NEWER).unwrap(), newer());
    assert_eq!(&encode_marker(&newer())[..], NEWER);

    let mut changed = decode_marker(NEWER).unwrap();
    changed.gone = true;
    let rewritten = decode_marker(&encode_marker(&changed)).unwrap();
    assert!(rewritten.gone);
    assert_eq!(rewritten.st, MarkerState::Other("FROZEN".to_owned()));
    assert_eq!(
        rewritten.why,
        Some(MarkerWhy::Other("REBALANCE".to_owned()))
    );
    assert_eq!(rewritten.extra, newer().extra);
}

/// Asserts that the marker of a key with a `/`, a space and multi-byte text keeps the key as it
/// is: the object is named by the key, and the body repeats it.
#[test]
fn the_key_of_a_marker_is_kept_as_written_whatever_it_holds() {
    let key = "a/b c/é€/日本";
    let marker = Marker {
        key: key.to_owned(),
        ..dirty()
    };
    let body = encode_marker(&marker);
    assert_eq!(decode_marker(&body).unwrap().key, key);
    assert!(String::from_utf8(body.to_vec()).unwrap().contains(key));
}
