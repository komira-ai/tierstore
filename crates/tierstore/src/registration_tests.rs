//! The body of a registration: what is written, what reads back, and what reads as none.

use crate::registration::{PROTO, Registration};

fn registration(hb: u64) -> Registration {
    Registration {
        proto: PROTO,
        boot: "b".into(),
        hb,
        peers: None,
    }
}

/// Asserts the bytes of a body, as the fixture of the format: the three fields in this order.
#[test]
fn the_body_is_the_json_of_proto_boot_and_hb() {
    let body = registration(2).encode();
    assert_eq!(&body[..], br#"{"proto":2,"boot":"b","hb":2}"#);
}

/// Asserts the bytes of a body that names peers, and that one that names none says so (an empty
/// list, not a missing field), and that both read back as written.
#[test]
fn the_peers_are_written_last_and_an_empty_list_is_written_too() {
    let mut named = registration(2);
    named.peers = Some(vec!["x".into(), "y".into()]);
    let body = named.encode();
    assert_eq!(
        &body[..],
        br#"{"proto":2,"boot":"b","hb":2,"peers":["x","y"]}"#
    );
    assert_eq!(Registration::decode(&body), Some(named));

    let mut none = registration(2);
    none.peers = Some(vec![]);
    let body = none.encode();
    assert_eq!(&body[..], br#"{"proto":2,"boot":"b","hb":2,"peers":[]}"#);
    assert_eq!(Registration::decode(&body), Some(none));
}

/// Asserts both directions of a rolling update on the body: the body of a release before `peers`
/// reads as a registration that names none and so names no boot as alive, and the previous
/// release's reader (the three fields, unknown ones ignored) reads the body of this one.
#[test]
fn a_body_without_peers_names_no_boots_and_the_release_before_reads_one_with() {
    let before = Registration::decode(br#"{"proto":2,"boot":"b","hb":3}"#).unwrap();
    assert_eq!((before.peers.clone(), before.named()), (None, None));

    #[derive(serde::Deserialize)]
    struct Previous {
        proto: u32,
        boot: String,
        hb: u64,
    }
    let mut now = registration(4);
    now.peers = Some(vec!["x".into()]);
    let read: Previous = serde_json::from_slice(&now.encode()).unwrap();
    assert_eq!((read.proto, read.boot.as_str(), read.hb), (2, "b", 4));
}

/// Asserts that a registration names as alive its own boot and its peers, each once, and none if it
/// does not say who its peers are.
#[test]
fn a_registration_names_its_boot_and_its_peers() {
    let mut named = registration(1);
    named.peers = Some(vec!["a".into(), "b".into()]);
    let boots: Vec<String> = named.named().unwrap().into_iter().collect();
    assert_eq!(boots, ["a", "b"]);
    named.peers = Some(vec![]);
    assert_eq!(
        named.named().unwrap().into_iter().collect::<Vec<_>>(),
        ["b"]
    );
}

/// Asserts that a body reads back as the registration that was encoded.
#[test]
fn a_registration_reads_back_as_written() {
    let written = registration(7);
    assert_eq!(Registration::decode(&written.encode()), Some(written));
}

/// Asserts that a field a later release adds is ignored, so that this release reads the
/// registration of that one.
#[test]
fn unknown_fields_are_ignored() {
    let body = br#"{"proto":2,"boot":"b","hb":3,"later":{"x":1}}"#;
    let read = Registration::decode(body).unwrap();
    assert_eq!((read.proto, read.boot.as_str(), read.hb), (2, "b", 3));
}

/// Asserts that the empty body of an older release's registration, text that is not JSON, JSON of
/// another shape and an object without a field all read as none, and none of them panics.
#[test]
fn an_older_or_damaged_body_reads_as_none() {
    for body in [
        &b""[..],
        b"not json",
        b"[]",
        b"{}",
        br#"{"proto":1,"boot":"b"}"#,
    ] {
        assert_eq!(Registration::decode(body), None, "{body:?}");
    }
}
