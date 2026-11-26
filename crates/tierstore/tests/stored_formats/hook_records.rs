//! The update hook's `state.json`: one record for both service managers. The launchd arm writes
//! the keys of the single-instance record of the release before; the systemd arm adds `live`,
//! `next` and `mode`, and, on a node with an fs process, `fs_live` and `fs_next`; the config hook's
//! record of a change applied in place adds `reload`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use tierstore::server::update_hook::{
    Instance, Kind, ListCap, Marker, Mode, Record, RecordError, Reload,
};

const SINGLE: &[u8] = include_bytes!("fixtures/hook_record_single.json");
const SYSTEMD: &[u8] = include_bytes!("fixtures/hook_record_systemd.json");
const SYSTEMD_FS: &[u8] = include_bytes!("fixtures/hook_record_systemd_fs.json");
const SYSTEMD_SWITCH: &[u8] = include_bytes!("fixtures/hook_record_systemd_switch.json");
const SYSTEMD_RELOAD: &[u8] = include_bytes!("fixtures/hook_record_systemd_reload.json");
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
        fs_live: None,
        fs_next: None,
        fs_switch: false,
        reload: None,
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
        fs_live: None,
        fs_next: None,
        fs_switch: false,
        reload: None,
    }
}

/// The record of the systemd arm on a node with an fs process: the same update, and the fs
/// process handed over from `a` to `b` too.
fn systemd_fs() -> Record {
    Record {
        fs_live: Some(Instance::A),
        fs_next: Some(Instance::B),
        ..systemd()
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

/// Asserts that the record of the systemd arm on a node with an fs process reads as its value and
/// is written as the fixture, with `fs_live` and `fs_next` among the keys in order and the schema
/// still 1, and that the fixture of the record without those keys reads with both absent. Catches
/// a renamed or removed fs key, and a raised schema, which the previous release's hook would
/// refuse.
#[test]
fn the_fs_record_fixture_reads_and_is_written_as_the_fixture() {
    assert_eq!(Record::decode(SYSTEMD_FS).unwrap(), systemd_fs());
    assert_eq!(systemd_fs().encode(), SYSTEMD_FS);
    assert_eq!(systemd_fs().check(), Ok(()));
    let without = Record::decode(SYSTEMD).unwrap();
    assert_eq!((without.fs_live, without.fs_next), (None, None));
}

/// The record of the switch of a node to one process, in flight: the fs instance `a` that ran is
/// named, and `fs_switch` is set.
fn systemd_switch() -> Record {
    Record {
        fs_live: Some(Instance::A),
        fs_switch: true,
        ..systemd()
    }
}

/// Asserts that the record of the switch reads as its value and is written as the fixture, with
/// `fs_switch` among the keys in order and the schema still 1, that the fixtures of records
/// without it read as not switched, and that the same record without the key is the record of an
/// ordinary update. Catches a renamed or removed key, a key written when it is not set (which
/// would change every record), and a raised schema.
#[test]
fn the_switch_record_fixture_reads_and_is_written_as_the_fixture() {
    assert_eq!(Record::decode(SYSTEMD_SWITCH).unwrap(), systemd_switch());
    assert_eq!(systemd_switch().encode(), SYSTEMD_SWITCH);
    assert_eq!(systemd_switch().check(), Ok(()));
    for fixture in [SINGLE, SYSTEMD, SYSTEMD_FS] {
        assert!(!Record::decode(fixture).unwrap().fs_switch);
    }
    let text = String::from_utf8(SYSTEMD_SWITCH.to_vec()).unwrap();
    let without = text.replace("  \"fs_switch\": true,\n", "");
    assert_ne!(without, text);
    assert!(!Record::decode(without.as_bytes()).unwrap().fs_switch);
}

/// The config hook's record of a change applied in place, in flight after its `cutover`: `a` serves
/// and starts no other instance, and the cap of the tree lists was set on the mount `4b5a6978`.
fn systemd_reload() -> Record {
    Record {
        updating: true,
        markers: BTreeSet::from([Marker::Swapped]),
        to: Some(PathBuf::from(
            "/var/lib/hostkeeper/packages/tierstore-config/1.1.0",
        )),
        version: Some("0.5.0+0123456789ab".to_owned()),
        reload: Some(Reload {
            guard: false,
            tree_list_ram_bytes: Some(ListCap {
                from: 536_870_912,
                to: 1 << 30,
                control_socket: PathBuf::from("/run/tierstore-fs/control.sock"),
                mount: Some("4b5a6978".to_owned()),
            }),
        }),
        ..Record::idle(Some(Instance::A))
    }
}

/// Asserts that the record of a change applied in place reads as its value and is written as the
/// fixture, with `reload` among the keys in order and the schema still 1; that the fixtures of
/// records without it read as none; and that the fixture with the key taken out, which is what a
/// hook that does not know the key reads of it, is the same record with no reload. Catches a
/// renamed or removed key, a key written when it is not set, and a raised schema.
#[test]
fn the_reload_record_fixture_reads_and_is_written_as_the_fixture() {
    assert_eq!(Record::decode(SYSTEMD_RELOAD).unwrap(), systemd_reload());
    assert_eq!(systemd_reload().encode(), SYSTEMD_RELOAD);
    assert_eq!(systemd_reload().check(), Ok(()));
    for fixture in [SINGLE, SYSTEMD, SYSTEMD_FS, SYSTEMD_SWITCH, REST] {
        assert_eq!(Record::decode(fixture).unwrap().reload, None);
    }
    let mut value: serde_json::Value = serde_json::from_slice(SYSTEMD_RELOAD).unwrap();
    assert!(value.as_object_mut().unwrap().remove("reload").is_some());
    assert_eq!(
        Record::decode(value.to_string().as_bytes()).unwrap(),
        Record {
            reload: None,
            ..systemd_reload()
        }
    );
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
    for (fixture, want) in [
        (SINGLE, single()),
        (SYSTEMD, systemd()),
        (SYSTEMD_FS, systemd_fs()),
    ] {
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
