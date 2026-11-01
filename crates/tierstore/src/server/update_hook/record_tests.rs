//! The record.

use std::collections::BTreeSet;

use super::{Marker, Mode, RECORD_MAX, Record, RecordError, SCHEMA};

fn full() -> Record {
    Record {
        schema: SCHEMA,
        serving: 9302,
        previous: Some(9301),
        mode: Mode::Restart,
        markers: BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
    }
}

/// Asserts the exact text of a record and that it reads back to the same record. It would catch a
/// change of the on-disk format that the previous version's hook, which may run `rollback`, could
/// not read.
#[test]
fn a_record_is_written_as_json_and_reads_back() {
    let text = String::from_utf8(full().encode()).unwrap();
    let want = "{\n  \"markers\": [\n    \"swapped\",\n    \"verified\",\n    \"stopping\"\n  ],\n  \"mode\": \"restart\",\n  \"previous\": 9301,\n  \"schema\": 1,\n  \"serving\": 9302\n}\n";
    assert_eq!(text, want);
    assert_eq!(Record::decode(text.as_bytes()).unwrap(), full());
    let idle = Record::idle(9301);
    let idle_text = String::from_utf8(idle.encode()).unwrap();
    assert!(idle_text.contains("\"previous\": null"));
    assert!(idle_text.contains("\"markers\": []"));
    assert_eq!(Record::decode(idle_text.as_bytes()).unwrap(), idle);
}

/// Asserts the helpers on the record: committed only with `stopping`, the current slot is the
/// previous one while an update is in flight and the serving one otherwise.
#[test]
fn the_current_slot_and_the_commit_follow_the_record() {
    let mut record = Record::idle(9301);
    assert!(!record.committed());
    assert_eq!(record.current(), 9301);
    record.serving = 9302;
    record.previous = Some(9301);
    assert_eq!(record.current(), 9301);
    record.markers.insert(Marker::Swapped);
    assert!(!record.committed());
    record.markers.insert(Marker::Stopping);
    assert!(record.committed());
    assert_eq!(Mode::Handover.as_str(), "handover");
}

/// Asserts each way a stored record is refused. It would catch a record that is acted on though it
/// is too big, newer than the hook, never written by it, or has a field or word the hook does not
/// know.
#[test]
fn a_record_that_is_not_one_is_refused() {
    let big = vec![b' '; RECORD_MAX + 1];
    assert_eq!(Record::decode(&big), Err(RecordError::TooBig));
    assert!(matches!(
        Record::decode(b"not json"),
        Err(RecordError::Corrupt(_))
    ));
    assert_eq!(
        Record::decode(br#"{"schema":2}"#),
        Err(RecordError::Newer { found: 2 })
    );
    assert_eq!(
        Record::decode(br#"{"schema":0}"#),
        Err(RecordError::Older { found: 0 })
    );
    let base = r#"{"schema":1,"serving":9301,"previous":null,"mode":"handover","markers":[]}"#;
    assert!(Record::decode(base.as_bytes()).is_ok());
    for bad in [
        base.replace("handover", "bounce"),
        base.replace("[]", r#"["done"]"#),
        base.replace("9301", "70000"),
        base.replace("}", r#","extra":1}"#),
        base.replace(r#""mode":"handover","#, ""),
    ] {
        assert!(
            matches!(Record::decode(bad.as_bytes()), Err(RecordError::Corrupt(_))),
            "{bad}"
        );
    }
}
