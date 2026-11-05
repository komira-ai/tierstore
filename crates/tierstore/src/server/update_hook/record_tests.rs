//! The record.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{Instance, Invalid, Kind, Marker, Mode, RECORD_MAX, Record, RecordError, SCHEMA};

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
