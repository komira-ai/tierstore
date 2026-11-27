//! The record.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{
    Instance, Invalid, Kind, ListCap, Marker, Mode, RECORD_MAX, Record, RecordError, Reload, SCHEMA,
};

fn release(name: &str) -> PathBuf {
    PathBuf::from(format!("/var/lib/hostkeeper/packages/tierstore/{name}"))
}

/// An update in flight: nothing done yet, `a` serves, `b` is next.
fn updating() -> Record {
    Record {
        updating: true,
        from: Some(release("0.4.0")),
        to: Some(release("0.5.0")),
        version: Some("0.5.0+0123456789ab".to_owned()),
        live: Some(Instance::A),
        next: Some(Instance::B),
        mode: Some(Mode::Handover),
        ..Record::idle(Some(Instance::A))
    }
}

fn with_markers(record: Record, markers: &[Marker]) -> Record {
    Record {
        markers: markers.iter().copied().collect(),
        ..record
    }
}

fn marks(mark: &'static str, needs: &'static str) -> Result<(), Invalid> {
    Err(Invalid::Marks { mark, needs })
}

/// Asserts the exact text of a record of the systemd arm with an update in flight (keys in
/// order, pretty, a newline at the end) and that it reads back to the same record. It would catch
/// a change of the stored format that the previous release's hook, which may read the record
/// after a rollback of the package, could not follow.
#[test]
fn a_record_is_written_as_json_and_reads_back() {
    let record = with_markers(
        updating(),
        &[Marker::Swapped, Marker::Verified, Marker::Stopping],
    );
    let text = String::from_utf8(record.encode()).unwrap();
    let want = "{\n  \"from\": \"/var/lib/hostkeeper/packages/tierstore/0.4.0\",\n  \"kind\": \"single\",\n  \"live\": \"a\",\n  \"markers\": [\n    \"swapped\",\n    \"verified\",\n    \"stopping\"\n  ],\n  \"mode\": \"handover\",\n  \"next\": \"b\",\n  \"schema\": 1,\n  \"to\": \"/var/lib/hostkeeper/packages/tierstore/0.5.0\",\n  \"updating\": true,\n  \"version\": \"0.5.0+0123456789ab\"\n}\n";
    assert_eq!(text, want);
    assert_eq!(Record::decode(text.as_bytes()).unwrap(), record);
}

/// Asserts that a record with nothing to say leaves its optional keys out, not as `null`, and
/// reads back: the keys of the idle record of a host that has no live instance are the ones the
/// previous release wrote.
#[test]
fn an_absent_optional_key_is_left_out() {
    let idle = Record::idle(None);
    let text = String::from_utf8(idle.encode()).unwrap();
    assert_eq!(
        text,
        "{\n  \"kind\": \"single\",\n  \"markers\": [],\n  \"schema\": 1,\n  \"updating\": false\n}\n"
    );
    assert_eq!(Record::decode(text.as_bytes()).unwrap(), idle);
    let live = String::from_utf8(Record::idle(Some(Instance::B)).encode()).unwrap();
    assert!(live.contains("\"live\": \"b\"") && !live.contains("next") && !live.contains("mode"));
    let restart = Record {
        mode: Some(Mode::Restart),
        ..updating()
    };
    let text = String::from_utf8(restart.encode()).unwrap();
    assert!(text.contains("\"mode\": \"restart\""));
    assert_eq!(Record::decode(text.as_bytes()).unwrap(), restart);
}

/// Asserts the helpers of the record: `committed` only with `stopping`, `has` per marker, `with`
/// adds a marker to a copy and leaves the original alone, and the words of modes and markers.
#[test]
fn the_markers_and_the_commit_follow_the_record() {
    let record = Record::idle(None);
    assert!(!record.committed());
    assert!(!record.has(Marker::Swapped));
    let swapped = record.with(Marker::Swapped);
    assert!(swapped.has(Marker::Swapped) && !swapped.has(Marker::Verified));
    assert!(!swapped.committed());
    assert!(record.markers.is_empty());
    let committed = swapped.with(Marker::Verified).with(Marker::Stopping);
    assert!(committed.committed());
    assert_eq!(
        committed.markers,
        BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping])
    );
    assert_eq!(Mode::Handover.as_str(), "handover");
    assert_eq!(Mode::Restart.as_str(), "restart");
    assert_eq!(Marker::Swapped.as_str(), "swapped");
    assert_eq!(Marker::Verified.as_str(), "verified");
    assert_eq!(Marker::Stopping.as_str(), "stopping");
    assert_eq!((record.schema, record.kind), (SCHEMA, Kind::Single));
}

/// Asserts each way a stored record is refused, with its words: too big, not JSON, no schema, a
/// schema newer or older than the hook's, and a body of the right schema with a word or a type the
/// hook does not know. It would catch a record that is acted on though the hook cannot read it.
#[test]
fn a_record_that_is_not_one_is_refused() {
    let big = vec![b' '; RECORD_MAX + 1];
    assert_eq!(Record::decode(&big), Err(RecordError::TooBig));
    assert!(RecordError::TooBig.to_string().contains("larger than"));
    for bad in [&b"not json"[..], b"{}", b"{\"schema\":\"1\"}", b""] {
        assert!(
            matches!(Record::decode(bad), Err(RecordError::Corrupt(_))),
            "{bad:?}"
        );
    }
    let newer = Record::decode(br#"{"schema":2}"#).unwrap_err();
    assert_eq!(newer, RecordError::Newer { found: 2 });
    assert!(newer.to_string().contains("schema 2, newer than 1"));
    let older = Record::decode(br#"{"schema":0}"#).unwrap_err();
    assert_eq!(older, RecordError::Older { found: 0 });
    assert!(older.to_string().contains("schema 0"));
    let base = r#"{"schema":1,"kind":"single","updating":false,"markers":[]}"#;
    assert_eq!(Record::decode(base.as_bytes()).unwrap(), Record::idle(None));
    for bad in [
        base.replace("single", "double"),
        base.replace("[]", r#"["done"]"#),
        base.replace("false", "\"no\""),
        base.replace("}", r#","live":"c"}"#),
        base.replace("}", r#","next":1}"#),
        base.replace("}", r#","mode":"bounce"}"#),
        base.replace("}", r#","from":5}"#),
        base.replace(r#""kind":"single","#, ""),
        base.replace(r#","markers":[]"#, ""),
    ] {
        assert!(
            matches!(Record::decode(bad.as_bytes()), Err(RecordError::Corrupt(_))),
            "{bad}"
        );
    }
}

/// Asserts that a key the hook does not know is ignored, so that a later hook may add an optional
/// one and this one still reads the record (and the previous release's hook reads this one's).
#[test]
fn a_key_added_later_is_ignored() {
    let text = String::from_utf8(updating().encode()).unwrap();
    let added = text.replace("\"kind\"", "\"added_later\": [1, 2],\n  \"kind\"");
    assert_eq!(Record::decode(added.as_bytes()).unwrap(), updating());
}

/// Asserts that the records the rules allow pass `check`: a host at rest, with and without a live
/// instance, a first install in flight, an update at each stage, one committed and in flight, and
/// one committed and finished (`updating` false, the markers and `to` and `version` kept).
#[test]
fn a_record_that_keeps_the_rules_passes_the_check() {
    let first_install = Record {
        from: None,
        live: None,
        mode: None,
        ..updating()
    };
    let finished = Record {
        updating: false,
        from: None,
        next: None,
        mode: None,
        live: Some(Instance::B),
        ..with_markers(
            updating(),
            &[Marker::Swapped, Marker::Verified, Marker::Stopping],
        )
    };
    let verified = with_markers(updating(), &[Marker::Swapped, Marker::Verified]);
    let launchd = Record {
        live: None,
        next: None,
        mode: None,
        ..updating()
    };
    for record in [
        Record::idle(None),
        Record::idle(Some(Instance::A)),
        updating(),
        with_markers(updating(), &[Marker::Swapped]),
        verified.clone(),
        verified.with(Marker::Stopping),
        first_install,
        launchd,
        finished,
        Record {
            live: None,
            ..updating()
        },
        Record {
            next: None,
            ..updating()
        },
    ] {
        assert_eq!(record.check(), Ok(()), "{record:?}");
    }
}

/// Asserts each rule of `check` and its words: the markers follow each other, markers need an
/// update or a commit, an update or a commit names `to` and `version`, a record at rest names none
/// of `from`, `next`, `mode`, `to` or `version`, and `live` and `next` differ. It would catch a
/// record the steps act on though it contradicts itself.
#[test]
fn a_record_that_breaks_a_rule_fails_the_check() {
    let all = [Marker::Swapped, Marker::Verified, Marker::Stopping];
    let rest = || Record::idle(Some(Instance::A));
    let cases = [
        (
            with_markers(updating(), &[Marker::Verified]),
            marks("verified", "swapped"),
        ),
        (
            with_markers(updating(), &[Marker::Swapped, Marker::Stopping]),
            marks("stopping", "verified"),
        ),
        (
            with_markers(rest(), &[Marker::Swapped]),
            Err(Invalid::Orphan),
        ),
        (
            Record {
                to: None,
                ..updating()
            },
            marks("updating", "to and version"),
        ),
        (
            Record {
                version: None,
                ..updating()
            },
            marks("updating", "to and version"),
        ),
        (
            Record {
                to: None,
                ..with_markers(rest(), &all)
            },
            marks("stopping", "to and version"),
        ),
        (
            Record {
                to: Some(release("0.5.0")),
                ..with_markers(rest(), &all)
            },
            marks("stopping", "to and version"),
        ),
        (
            Record {
                from: Some(release("0.4.0")),
                ..rest()
            },
            marks("from", "updating"),
        ),
        (
            Record {
                next: Some(Instance::B),
                ..rest()
            },
            marks("next", "updating"),
        ),
        (
            Record {
                mode: Some(Mode::Restart),
                ..rest()
            },
            marks("mode", "updating"),
        ),
        (
            Record {
                to: Some(release("0.5.0")),
                ..rest()
            },
            marks("to or version", "updating or stopping"),
        ),
        (
            Record {
                version: Some("0.5.0".to_owned()),
                ..rest()
            },
            marks("to or version", "updating or stopping"),
        ),
        (
            Record {
                next: Some(Instance::A),
                ..updating()
            },
            Err(Invalid::Same),
        ),
    ];
    for (record, want) in cases {
        assert_eq!(record.check(), want, "{record:?}");
    }
    assert_eq!(
        Invalid::Marks {
            mark: "next",
            needs: "updating"
        }
        .to_string(),
        "next is set but updating is not"
    );
    assert_eq!(
        Invalid::Orphan.to_string(),
        "markers are set with no update in flight"
    );
    assert_eq!(
        Invalid::Same.to_string(),
        "live and next are the same instance"
    );
}

/// An update in flight of a node with an fs process: `a` serves both units, `b` is next for both.
fn updating_with_fs() -> Record {
    Record {
        fs_live: Some(Instance::A),
        fs_next: Some(Instance::B),
        ..updating()
    }
}

/// Asserts that the keys of the fs process are written after the others' letters, only when they
/// are set, and read back; and that a record written before the fs keys existed (none of them)
/// decodes with both absent. Catches a format change the previous release's record cannot be read
/// across: the keys are additive, the schema stays 1.
#[test]
fn the_fs_keys_are_additive_and_read_back() {
    let record = updating_with_fs();
    let text = String::from_utf8(record.encode()).unwrap();
    assert!(text.contains("\"fs_live\": \"a\""), "{text}");
    assert!(text.contains("\"fs_next\": \"b\""), "{text}");
    assert!(text.contains("\"schema\": 1"), "{text}");
    assert_eq!(Record::decode(text.as_bytes()).unwrap(), record);

    let without = String::from_utf8(updating().encode()).unwrap();
    assert!(!without.contains("fs_"), "{without}");
    let old = Record::decode(without.as_bytes()).unwrap();
    assert_eq!((old.fs_live, old.fs_next), (None, None));
    let idle = String::from_utf8(Record::idle(Some(Instance::A)).encode()).unwrap();
    assert!(!idle.contains("fs_"), "{idle}");
}

/// Asserts that a record of the fs process at rest and one in flight pass `check`, and that the
/// fs keys break the rules they must: `fs_next` only while updating and only beside `fs_live`, and
/// never the same instance as `fs_live`. Catches a record that names an fs instance to start
/// from nowhere, which a step would act on.
#[test]
fn the_fs_keys_hold_to_their_rules() {
    let at_rest = Record {
        fs_live: Some(Instance::B),
        ..Record::idle(Some(Instance::A))
    };
    assert_eq!(at_rest.check(), Ok(()));
    assert_eq!(updating_with_fs().check(), Ok(()));
    assert_eq!(
        Record {
            fs_next: None,
            ..updating_with_fs()
        }
        .check(),
        Ok(())
    );
    assert_eq!(
        Record {
            fs_next: Some(Instance::B),
            ..at_rest.clone()
        }
        .check(),
        marks("fs_next", "updating")
    );
    assert_eq!(
        Record {
            fs_live: None,
            ..updating_with_fs()
        }
        .check(),
        marks("fs_next", "fs_live")
    );
    assert_eq!(
        Record {
            fs_next: Some(Instance::A),
            ..updating_with_fs()
        }
        .check(),
        Err(Invalid::FsSame)
    );
    assert_eq!(
        Invalid::FsSame.to_string(),
        "fs_live and fs_next are the same instance"
    );
}

/// Asserts that `fs_switch` is written only when set (a record without it encodes with no such
/// key, byte for byte as before), that it reads back, and that a record with no such key reads as
/// not switched. Catches a new key that changes the bytes of every record, which the previous
/// release's hook would then read.
#[test]
fn fs_switch_is_written_only_when_set_and_reads_back() {
    let plain = updating();
    let text = String::from_utf8(plain.encode()).unwrap();
    assert!(!text.contains("fs_switch"), "{text}");
    assert!(!Record::decode(plain.encode().as_slice()).unwrap().fs_switch);

    let switched = Record {
        fs_live: Some(Instance::A),
        fs_switch: true,
        ..updating()
    };
    let text = String::from_utf8(switched.encode()).unwrap();
    assert!(text.contains("\"fs_switch\": true"), "{text}");
    assert_eq!(Record::decode(text.as_bytes()), Ok(switched));
}

/// Asserts that `fs_switch` passes `check` while an update is in flight and after its commit, and
/// breaks the rules it must: not at rest with no commit, and not beside `fs_next`. Catches a
/// switch noted on a record that no update owns, and a record that both hands the fs process over
/// and drops it.
#[test]
fn fs_switch_holds_to_its_rules() {
    let switched = Record {
        fs_live: Some(Instance::A),
        fs_switch: true,
        ..updating()
    };
    assert_eq!(switched.check(), Ok(()));
    let committed = with_markers(
        Record {
            updating: false,
            from: None,
            next: None,
            mode: None,
            fs_live: None,
            ..switched.clone()
        },
        &[Marker::Swapped, Marker::Verified, Marker::Stopping],
    );
    assert_eq!(committed.check(), Ok(()));
    assert_eq!(
        Record {
            fs_switch: true,
            ..Record::idle(Some(Instance::A))
        }
        .check(),
        marks("fs_switch", "updating or stopping")
    );
    assert_eq!(
        Record {
            fs_next: Some(Instance::B),
            ..switched
        }
        .check(),
        Err(Invalid::FsSwitchNext)
    );
    assert_eq!(
        Invalid::FsSwitchNext.to_string(),
        "fs_switch and fs_next are both set"
    );
}

/// A config update applied in place by `a`, of the cap `from` to `to`, set on the mount `m1`.
fn reloading() -> Record {
    Record {
        updating: true,
        to: Some(PathBuf::from(
            "/var/lib/hostkeeper/packages/tierstore-config/1.1.0",
        )),
        version: Some("0.5.0+0123456789ab".to_owned()),
        reload: Some(Reload {
            guard: true,
            tree_list_ram_bytes: Some(ListCap {
                from: 536_870_912,
                to: 1 << 30,
                control_socket: PathBuf::from("/run/tierstore-fs/control.sock"),
                mount: Some("m1".to_owned()),
            }),
        }),
        ..Record::idle(Some(Instance::A))
    }
}

/// Asserts the exact text of the record of an update applied in place (the key `reload`, its cap
/// with the mount, and none of `next` or `mode`), that it reads back, and that a reload with no
/// cap, or a cap not set yet, is written with no such key and reads back. Catches a change of the
/// stored format the hook of the next release could not read.
#[test]
fn reload_is_written_and_reads_back() {
    let text = String::from_utf8(reloading().encode()).unwrap();
    let want = "{\n  \"kind\": \"single\",\n  \"live\": \"a\",\n  \"markers\": [],\n  \"reload\": {\n    \"guard\": true,\n    \"tree_list_ram_bytes\": {\n      \"control_socket\": \"/run/tierstore-fs/control.sock\",\n      \"from\": 536870912,\n      \"mount\": \"m1\",\n      \"to\": 1073741824\n    }\n  },\n  \"schema\": 1,\n  \"to\": \"/var/lib/hostkeeper/packages/tierstore-config/1.1.0\",\n  \"updating\": true,\n  \"version\": \"0.5.0+0123456789ab\"\n}\n";
    assert_eq!(text, want);
    assert_eq!(Record::decode(text.as_bytes()), Ok(reloading()));
    for reload in [
        Reload::default(),
        Reload {
            guard: false,
            tree_list_ram_bytes: Some(ListCap {
                mount: None,
                ..reloading().reload.unwrap().tree_list_ram_bytes.unwrap()
            }),
        },
    ] {
        let record = Record {
            reload: Some(reload),
            ..reloading()
        };
        let text = String::from_utf8(record.encode()).unwrap();
        assert!(!text.contains("mount"), "{text}");
        assert_eq!(Record::decode(text.as_bytes()), Ok(record));
    }
    let bare = "{\"schema\":1,\"kind\":\"single\",\"updating\":false,\"markers\":[],\"reload\":{}}";
    assert_eq!(
        Record::decode(bare.as_bytes()).unwrap().reload,
        Some(Reload::default())
    );
}

/// Asserts the compatibility of the key both ways: a record of this hook with no reload has no
/// key `reload`, byte for byte the record of the hook before it, which a record of that hook (no
/// such key) reads as none; and a record with the key, read by a hook that does not know it, is
/// the same record without it (`a_key_added_later_is_ignored` reads an unknown key the same way).
#[test]
fn reload_is_additive_both_ways() {
    let plain = updating();
    let text = String::from_utf8(plain.encode()).unwrap();
    assert!(!text.contains("reload"), "{text}");
    assert_eq!(Record::decode(text.as_bytes()).unwrap().reload, None);
    let with = String::from_utf8(reloading().encode()).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&with).unwrap();
    let _ = value.as_object_mut().unwrap().remove("reload");
    let before = Record {
        reload: None,
        ..reloading()
    };
    assert_eq!(Record::decode(value.to_string().as_bytes()), Ok(before));
}

/// Asserts that `reload` passes `check` with an update in flight and after its commit, and breaks
/// the rules it must: not at rest with no commit, and not beside a key of an update that starts an
/// instance. Catches a reload noted on a record no update owns, and one that also starts the other
/// instance.
#[test]
fn reload_holds_to_its_rules() {
    assert_eq!(reloading().check(), Ok(()));
    let committed = with_markers(
        Record {
            updating: false,
            ..reloading()
        },
        &[Marker::Swapped, Marker::Verified, Marker::Stopping],
    );
    assert_eq!(committed.check(), Ok(()));
    assert_eq!(
        Record {
            updating: false,
            to: None,
            version: None,
            ..reloading()
        }
        .check(),
        marks("reload", "updating or stopping")
    );
    for starts in [
        Record {
            next: Some(Instance::B),
            ..reloading()
        },
        Record {
            mode: Some(Mode::Restart),
            ..reloading()
        },
        Record {
            fs_live: Some(Instance::A),
            fs_next: Some(Instance::B),
            ..reloading()
        },
        Record {
            fs_live: Some(Instance::A),
            fs_switch: true,
            ..reloading()
        },
    ] {
        assert_eq!(starts.check(), Err(Invalid::ReloadStarts));
    }
    assert_eq!(
        Invalid::ReloadStarts.to_string(),
        "reload is set with next, mode, fs_next or fs_switch: a reload starts no instance"
    );
}
