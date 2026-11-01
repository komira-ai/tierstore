//! The record.

use std::collections::BTreeSet;

use super::{Marker, Mode, ModeReason, RECORD_MAX, Record, RecordError, SCHEMA, is_reason_token};

fn full() -> Record {
    Record {
        schema: SCHEMA,
        serving: 9302,
        previous: Some(9301),
        mode: Mode::Restart,
        mode_reason: Some(ModeReason::DeclaredRestart),
        markers: BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
        boot_id: Some("boot-1".to_owned()),
        guard_placed: true,
    }
}

/// Asserts the exact text of a record and that it reads back to the same record. It would catch a
/// change of the on-disk format that the previous version's hook, which may run `rollback`, could
/// not read.
#[test]
fn a_record_is_written_as_json_and_reads_back() {
    let text = String::from_utf8(full().encode()).unwrap();
    let want = "{\n  \"boot_id\": \"boot-1\",\n  \"guard_placed\": true,\n  \"markers\": [\n    \"swapped\",\n    \"verified\",\n    \"stopping\"\n  ],\n  \"mode\": \"restart\",\n  \"mode_reason\": \"declared_restart\",\n  \"previous\": 9301,\n  \"schema\": 1,\n  \"serving\": 9302\n}\n";
    assert_eq!(text, want);
    assert_eq!(Record::decode(text.as_bytes()).unwrap(), full());
    let idle = Record::idle(9301);
    let idle_text = String::from_utf8(idle.encode()).unwrap();
    assert!(idle_text.contains("\"previous\": null"));
    assert!(idle_text.contains("\"markers\": []"));
    assert!(
        idle_text.contains("\"boot_id\": null") && idle_text.contains("\"guard_placed\": false")
    );
    assert!(idle_text.contains("\"mode_reason\": null"));
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
        base.replace("}", r#","mode_reason":"because"}"#),
        base.replace("}", r#","mode_reason":"handover"}"#),
    ] {
        assert!(
            matches!(Record::decode(bad.as_bytes()), Err(RecordError::Corrupt(_))),
            "{bad}"
        );
    }
}

/// Asserts that a record written before `boot_id`, `guard_placed` and `mode_reason` existed still
/// reads, with each at its default. It would catch a format change that the previous version's
/// hook, which may run `rollback`, could not follow: formats change by adding fields only.
#[test]
fn a_record_without_the_newer_fields_reads_with_their_defaults() {
    let old = r#"{"schema":1,"serving":9301,"previous":null,"mode":"handover","markers":[]}"#;
    let record = Record::decode(old.as_bytes()).unwrap();
    assert_eq!((record.boot_id, record.guard_placed), (None, false));
    assert_eq!(record.mode_reason, None);
}

/// Asserts the word of each reason for a mode, that it reads back from the record, that it is
/// spelt as the report's reasons are, and the mode it chooses. It would catch a reason written
/// in a word the record then refuses, or a declared restart that chooses a handover.
#[test]
fn each_reason_for_a_mode_is_a_token_that_reads_back() {
    let words: Vec<&str> = ModeReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(
        words,
        [
            "handover_capable",
            "not_handover_capable",
            "declared_restart"
        ]
    );
    for reason in ModeReason::ALL {
        assert!(is_reason_token(reason.as_str()), "{reason:?}");
        let text = format!(
            r#"{{"schema":1,"serving":9301,"previous":null,"mode":"{}","mode_reason":"{}","markers":[]}}"#,
            reason.mode().as_str(),
            reason.as_str()
        );
        let record = Record::decode(text.as_bytes()).unwrap();
        assert_eq!(record.mode_reason, Some(reason));
    }
    assert_eq!(ModeReason::HandoverCapable.mode(), Mode::Handover);
    assert_eq!(ModeReason::NotHandoverCapable.mode(), Mode::Restart);
    assert_eq!(ModeReason::DeclaredRestart.mode(), Mode::Restart);
}
