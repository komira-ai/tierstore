//! The update hook's `state.json`: the record of the two-slot hook of Linux and the record of the
//! single-instance hook of macOS.

use std::collections::BTreeSet;
use std::path::PathBuf;

use tierstore::server::update_hook::{Marker, Mode, Record, RecordError, SingleKind, SingleRecord};

const RECORD: &[u8] = include_bytes!("fixtures/hook_record.json");
const RECORD_IDLE: &[u8] = include_bytes!("fixtures/hook_record_idle.json");
const RECORD_FIRST_RELEASE: &[u8] = include_bytes!("fixtures/hook_record_first_release.json");
const RECORD_NEWER: &[u8] = include_bytes!("fixtures/hook_record_newer.json");
const SINGLE: &[u8] = include_bytes!("fixtures/hook_record_single.json");

fn in_flight() -> Record {
    Record {
        schema: 1,
        serving: 9302,
        previous: Some(9301),
        mode: Mode::Handover,
        markers: BTreeSet::from([Marker::Swapped, Marker::Verified]),
        boot_id: Some("7c1e2f0a-5d3b-4c8e-9a41-0b6d2e8f3a17".to_owned()),
        guard_placed: true,
    }
}

fn single() -> SingleRecord {
    SingleRecord {
        schema: 1,
        kind: SingleKind::Single,
        updating: true,
        markers: BTreeSet::from([Marker::Swapped]),
        from: Some(PathBuf::from("/usr/local/tierstore/releases/a")),
        to: Some(PathBuf::from("/usr/local/tierstore/releases/b")),
        version: Some("0.4.0".to_owned()),
    }
}

/// Asserts that the record of a two-slot host with an update in flight (two ports, two markers,
/// a boot id and a placed guard) reads as that value. Catches a renamed or removed field, or a
/// marker word that changed.
#[test]
fn the_record_fixture_of_an_update_in_flight_reads_as_its_value() {
    assert_eq!(Record::decode(RECORD).unwrap(), in_flight());
}

/// Asserts that the value is written as the bytes of the fixture: pretty JSON with the keys in
/// order and a newline at the end.
#[test]
fn the_record_of_an_update_in_flight_is_written_as_the_fixture() {
    assert_eq!(in_flight().encode(), RECORD);
}

/// Asserts that the record of a host at rest reads as `Record::idle` and that `Record::idle`
/// is written as the fixture, with the absent port and boot id as `null`.
#[test]
fn the_record_fixture_of_a_host_at_rest_is_the_idle_record_both_ways() {
    assert_eq!(Record::decode(RECORD_IDLE).unwrap(), Record::idle(9302));
    assert_eq!(Record::idle(9302).encode(), RECORD_IDLE);
}

/// Asserts that a record as the first release wrote it, without a boot id and without
/// `guard_placed`, still reads, with those two at their defaults: a field added later has a
/// default, so a record of an older hook is read.
#[test]
fn a_record_of_the_first_release_still_reads_with_the_added_fields_at_their_defaults() {
    let read = Record::decode(RECORD_FIRST_RELEASE).unwrap();
    let want = Record {
        previous: Some(9301),
        markers: BTreeSet::from([Marker::Swapped]),
        ..Record::idle(9302)
    };
    assert_eq!(read, want);
}

/// Asserts that the record of a single-instance host with an update in flight reads as its value
/// and is written back as the fixture, byte for byte.
#[test]
fn the_single_instance_record_fixture_reads_and_is_written_as_the_fixture() {
    assert_eq!(SingleRecord::decode(SINGLE).unwrap(), single());
    assert_eq!(single().encode(), SINGLE);
}

/// Asserts that the single-instance record ignores a key it does not know, so that a later hook
/// may add an optional one.
#[test]
fn the_single_instance_record_ignores_a_key_added_later() {
    let text = String::from_utf8(SINGLE.to_vec()).unwrap();
    let added = text.replace("\"kind\"", "\"added_later\": 1,\n  \"kind\"");
    assert_eq!(SingleRecord::decode(added.as_bytes()).unwrap(), single());
}

/// Asserts that a two-slot record of a raised schema that carries a key this hook does not know
/// (the fixture of a newer hook) is refused as `Newer` with the schema it has, not as a corrupt
/// record: the hook reads it as newer than it understands and leaves it alone.
#[test]
fn a_two_slot_record_of_a_raised_schema_with_a_new_key_reads_as_newer() {
    let error = Record::decode(RECORD_NEWER).unwrap_err();
    assert_eq!(error, RecordError::Newer { found: 2 });
}
