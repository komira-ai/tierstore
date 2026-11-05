//! The update hook's `state.json`: one record for both service managers. The launchd arm writes
//! the keys of the single-instance record of the release before; the systemd arm adds `live`,
//! `next` and `mode`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use tierstore::server::update_hook::{Instance, Kind, Marker, Mode, Record, RecordError};

const SINGLE: &[u8] = include_bytes!("fixtures/hook_record_single.json");
const SYSTEMD: &[u8] = include_bytes!("fixtures/hook_record_systemd.json");
const REST: &[u8] = include_bytes!("fixtures/hook_record_systemd_rest.json");
const NEWER: &[u8] = include_bytes!("fixtures/hook_record_systemd_newer.json");

/// The record of the previous release: an update in flight, no instance letters, no mode.
fn single() -> Record {
    Record {
        schema: 1,
        kind: Kind::Single,
        updating: true,
        markers: BTreeSet::from([Marker::Swapped]),
        from: Some(PathBuf::from("/usr/local/tierstore/releases/a")),
        to: Some(PathBuf::from("/usr/local/tierstore/releases/b")),
        version: Some("0.4.0".to_owned()),
        live: None,
        next: None,
        mode: None,
    }
}

/// The record of the systemd arm: an update in flight, `a` serving, `b` next, by hand-over.
fn systemd() -> Record {
    Record {
        schema: 1,
        kind: Kind::Single,
        updating: true,
        markers: BTreeSet::from([Marker::Swapped]),
        from: Some(PathBuf::from(
            "/var/lib/hostkeeper/packages/tierstore/0.4.0",
        )),
        to: Some(PathBuf::from(
            "/var/lib/hostkeeper/packages/tierstore/0.5.0",
        )),
        version: Some("0.5.0+0123456789ab".to_owned()),
        live: Some(Instance::A),
        next: Some(Instance::B),
        mode: Some(Mode::Handover),
    }
}

/// Asserts that the record as the release before wrote it (no `live`, `next` or `mode`) reads as
/// its value with those three absent, and is written back as the fixture, byte for byte: the
/// launchd arm writes the previous release's record.
#[test]
fn the_record_of_the_release_before_reads_and_is_written_as_the_fixture() {
    assert_eq!(Record::decode(SINGLE).unwrap(), single());
    assert_eq!(single().encode(), SINGLE);
    assert_eq!(single().check(), Ok(()));
}

/// Asserts that the record of the systemd arm with an update in flight reads as its value and is
/// written as the fixture: pretty JSON, the keys in order, a newline at the end. Catches a renamed
/// or removed key, or a word of `mode` or of an instance that changed.
#[test]
fn the_systemd_record_fixture_reads_and_is_written_as_the_fixture() {
    assert_eq!(Record::decode(SYSTEMD).unwrap(), systemd());
    assert_eq!(systemd().encode(), SYSTEMD);
    assert_eq!(systemd().check(), Ok(()));
}

/// Asserts that the record of a systemd host at rest reads as `Record::idle` of the live instance
/// and that `Record::idle` is written as the fixture, with the keys it has nothing to say for left
/// out.
#[test]
fn the_record_fixture_of_a_host_at_rest_is_the_idle_record_both_ways() {
    assert_eq!(
        Record::decode(REST).unwrap(),
        Record::idle(Some(Instance::B))
    );
    assert_eq!(Record::idle(Some(Instance::B)).encode(), REST);
}

/// Asserts that the record ignores a key it does not know, so that a later hook may add an
/// optional one and this one still reads the record.
#[test]
fn the_record_ignores_a_key_added_later() {
    for (fixture, want) in [(SINGLE, single()), (SYSTEMD, systemd())] {
        let text = String::from_utf8(fixture.to_vec()).unwrap();
        let added = text.replace("\"kind\"", "\"added_later\": 1,\n  \"kind\"");
        assert_ne!(added, text);
        assert_eq!(Record::decode(added.as_bytes()).unwrap(), want);
    }
}

/// Asserts that a record of a raised schema that carries a key this hook does not know (the
/// fixture of a newer hook) is refused as `Newer` with the schema it has, not as corrupt: the hook
/// leaves a record it cannot understand alone.
#[test]
fn a_record_of_a_raised_schema_with_a_new_key_reads_as_newer() {
    assert_eq!(
        Record::decode(NEWER).unwrap_err(),
        RecordError::Newer { found: 2 }
    );
}
