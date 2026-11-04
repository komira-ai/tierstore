//! The body of a registration: what is written, what reads back, and what reads as none.

use crate::registration::{PROTO, Registration};

fn registration(hb: u64) -> Registration {
    Registration {
        proto: PROTO,
        boot: "b".into(),
        hb,
    }
}

/// Asserts the bytes of a body, as the fixture of the format: the three fields in this order.
#[test]
fn the_body_is_the_json_of_proto_boot_and_hb() {
    let body = registration(2).encode();
    assert_eq!(&body[..], br#"{"proto":2,"boot":"b","hb":2}"#);
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

/// Asserts that only a registration whose count is above zero is one of a process that beats: the
/// first beat writes the count one, and the registration of `open` has zero.
#[test]
fn a_registration_beats_from_the_first_heartbeat_on() {
    assert!(!registration(0).beats());
    assert!(registration(1).beats());
}
