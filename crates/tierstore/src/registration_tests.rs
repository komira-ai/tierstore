//! The body of a registration: what is written, what reads back, and what reads as none.

use crate::format::FormatPhase;
use crate::registration::{PROTO, Registration};

fn registration(hb: u64) -> Registration {
    Registration {
        proto: PROTO,
        boot: "b".into(),
        hb,
        phase: None,
        writes_in_flight: None,
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

/// Asserts that the two optional fields are written when they are set, after `hb` and in the order
/// `phase`, `writes_in_flight`, and that the body reads back as it was written.
#[test]
fn the_phase_and_the_writes_in_flight_are_written_after_hb_and_read_back() {
    let set = Registration {
        phase: Some(FormatPhase::ActiveMarks),
        writes_in_flight: Some(3),
        ..registration(2)
    };
    let body = set.encode();
    let want = br#"{"proto":2,"boot":"b","hb":2,"phase":"active-marks","writes_in_flight":3}"#;
    assert_eq!(&body[..], want);
    assert_eq!(Registration::decode(&body), Some(set));
}

/// Asserts that the body of a registration that sets neither field is the bytes of the release
/// before the fields, and that those bytes, and a body with only one of the two, read with the
/// other as none: an older proxy's registration is read by this release, and the registration of a
/// proxy that sets one field is not taken for one that sets both.
#[test]
fn a_registration_without_the_optional_fields_is_unchanged_and_reads_with_none() {
    let old = br#"{"proto":2,"boot":"b","hb":5}"#;
    assert_eq!(&registration(5).encode()[..], old);
    let read = Registration::decode(old).unwrap();
    assert_eq!((read.phase, read.writes_in_flight), (None, None));

    let only_phase = br#"{"proto":3,"boot":"b","hb":5,"phase":"drained"}"#;
    let read = Registration::decode(only_phase).unwrap();
    assert_eq!(
        (read.phase, read.writes_in_flight),
        (Some(FormatPhase::Drained), None)
    );
    let only_writes = br#"{"proto":3,"boot":"b","hb":5,"writes_in_flight":0}"#;
    let read = Registration::decode(only_writes).unwrap();
    assert_eq!((read.phase, read.writes_in_flight), (None, Some(0)));
}

/// Asserts that a phase word this release does not know reads as the phase `Unknown` and does not
/// make the registration unreadable, so a proxy of a later release that reports a later phase is
/// still seen to beat.
#[test]
fn a_phase_this_release_does_not_know_reads_as_unknown() {
    let body = br#"{"proto":4,"boot":"b","hb":1,"phase":"someday","writes_in_flight":1}"#;
    let read = Registration::decode(body).unwrap();
    assert_eq!(read.phase, Some(FormatPhase::Unknown));
    assert!(read.beats());
}
