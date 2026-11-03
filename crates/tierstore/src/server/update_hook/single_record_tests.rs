//! The record of a single-instance host: its bytes, the rules `SingleRecord::check` holds it to,
//! and what `SingleRecord::decode` takes and refuses.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::record::{Marker, Record};
use super::single_testing::{DECLARATION, NEW, OLD, in_flight};
use super::{RECORD_MAX, RecordError, SingleRecord};

fn markers(list: &[Marker]) -> BTreeSet<Marker> {
    list.iter().copied().collect()
}

/// Asserts the record's bytes and its rules: the idle record is exactly the JSON the docs show,
/// an update in flight carries `from`, `to` and `version` and reads back as itself, a record of
/// the two-slot hook is refused here and this one there, and the rules refuse `verified` without
/// `swapped`, `stopping` without `verified`, markers with no update in flight and no commit, an
/// update in flight that does not name its releases, a commit that does not name its release, and
/// releases named at rest. It would catch a hook that acts on the other platform's record, or on
/// an update it cannot roll back.
#[test]
fn the_single_record_reads_back_and_refuses_the_other_kind() {
    let idle = SingleRecord::idle();
    assert_eq!(
        String::from_utf8(idle.encode()).unwrap(),
        "{\n  \"kind\": \"single\",\n  \"markers\": [],\n  \"schema\": 1,\n  \"updating\": false\n}\n"
    );
    assert_eq!(SingleRecord::decode(&idle.encode()), Ok(idle.clone()));
    let flying = in_flight();
    let text = String::from_utf8(flying.encode()).unwrap();
    assert!(text.contains(&format!("\"from\": \"{OLD}\"")), "{text}");
    assert!(text.contains(&format!("\"to\": \"{NEW}\"")), "{text}");
    assert!(text.contains("\"version\": \"0.4.0\""), "{text}");
    assert_eq!(SingleRecord::decode(flying.encode().as_slice()), Ok(flying));
    assert!(matches!(
        SingleRecord::decode(&Record::idle(9301).encode()),
        Err(RecordError::Corrupt(_))
    ));
    assert!(matches!(
        Record::decode(&idle.encode()),
        Err(RecordError::Corrupt(_))
    ));
    let committed = SingleRecord {
        updating: false,
        from: None,
        markers: markers(&[Marker::Swapped, Marker::Verified, Marker::Stopping]),
        ..in_flight()
    };
    assert_eq!(committed.check(), Ok(()));
    let rules: [(SingleRecord, &str); 7] = [
        (
            SingleRecord {
                markers: markers(&[Marker::Verified]),
                ..in_flight()
            },
            "verified is set but swapped is not",
        ),
        (
            SingleRecord {
                markers: markers(&[Marker::Swapped, Marker::Stopping]),
                ..in_flight()
            },
            "stopping is set but verified is not",
        ),
        (
            SingleRecord {
                markers: markers(&[Marker::Swapped]),
                ..SingleRecord::idle()
            },
            "markers are set with no update in flight",
        ),
        (
            SingleRecord {
                from: None,
                ..in_flight()
            },
            "updating is set but from, to and version is not",
        ),
        (
            SingleRecord {
                version: None,
                ..committed.clone()
            },
            "stopping is set but to and version is not",
        ),
        (
            SingleRecord {
                from: Some(PathBuf::from(OLD)),
                ..committed.clone()
            },
            "from is set but updating is not",
        ),
        (
            SingleRecord {
                to: Some(PathBuf::from(NEW)),
                ..SingleRecord::idle()
            },
            "to or version is set but updating or stopping is not",
        ),
    ];
    for (record, why) in rules {
        assert_eq!(record.check().unwrap_err().to_string(), why, "{record:?}");
    }
    assert!(DECLARATION.contains("[9300]"));
}

/// Asserts what `SingleRecord::decode` takes and refuses on its own: a key it does not know (a
/// later hook's optional one) is ignored; malformed JSON, a missing required key and a kind other
/// than `single` are `Corrupt`; schema 0 is `Older` and schema 2 `Newer`; a record of exactly
/// `RECORD_MAX` bytes is read and one byte more is `TooBig`. It would catch a reader that refuses
/// an additive field during a recovery, or one that reads an unbounded file.
#[test]
fn the_single_record_decoder_takes_additive_keys_and_refuses_the_rest() {
    let idle = SingleRecord::idle();
    let with_more =
        br#"{"schema":1,"kind":"single","updating":false,"markers":[],"later":{"x":1}}"#;
    assert_eq!(SingleRecord::decode(with_more), Ok(idle.clone()));
    for bad in [
        &b"{\"schema\":1,"[..],
        br#"{"schema":1,"kind":"single","markers":[]}"#,
        br#"{"schema":1,"kind":"slots","updating":false,"markers":[]}"#,
        br#"{"kind":"single","updating":false,"markers":[]}"#,
    ] {
        assert!(
            matches!(SingleRecord::decode(bad), Err(RecordError::Corrupt(_))),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
    assert_eq!(
        SingleRecord::decode(br#"{"schema":0,"kind":"single"}"#),
        Err(RecordError::Older { found: 0 })
    );
    assert_eq!(
        SingleRecord::decode(br#"{"schema":2,"kind":"single"}"#),
        Err(RecordError::Newer { found: 2 })
    );
    let mut full = idle.encode();
    full.resize(RECORD_MAX, b' ');
    assert_eq!(SingleRecord::decode(&full), Ok(idle));
    full.push(b' ');
    assert_eq!(SingleRecord::decode(&full), Err(RecordError::TooBig));
}
