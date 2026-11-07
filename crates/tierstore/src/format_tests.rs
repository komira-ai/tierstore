//! The format object: which bodies decode, which phases pause writes, and how it is read.

use bytes::Bytes;

use crate::faulty::{FaultyBackend, Op};
use crate::format::{Format, FormatPhase, format_body, read_format, read_format_tagged};
use crate::journal::FORMAT_KEY;
use crate::{Backend, Error, MemoryBackend};

fn decoded(json: &str) -> Format {
    serde_json::from_str(json).unwrap()
}

/// Asserts that the three phases of the design decode, with and without an epoch, and that only
/// `drain` and `drained` pause writes.
#[test]
fn drain_and_drained_pause_writes_and_the_other_phases_do_not() {
    let cases = [
        (r#"{"phase":"drain"}"#, FormatPhase::Drain, None, true),
        (r#"{"phase":"drained"}"#, FormatPhase::Drained, None, true),
        (
            r#"{"epoch":2,"phase":"active"}"#,
            FormatPhase::Active,
            Some(2),
            false,
        ),
    ];
    for (json, phase, epoch, paused) in cases {
        let format = decoded(json);
        assert_eq!((format.phase, format.epoch), (phase, epoch), "{json}");
        assert_eq!(format.pauses_writes(), paused, "{json}");
    }
}

/// Asserts that a phase this release does not know, and a field it does not know, are read
/// without an error and pause nothing: a newer writer's object does not stop this release.
#[test]
fn an_unknown_phase_and_unknown_fields_are_read_and_pause_nothing() {
    let format = decoded(r#"{"phase":"someday","note":{"a":1},"epoch":9}"#);
    assert_eq!(format.phase, FormatPhase::Unknown);
    assert!(!format.pauses_writes());
}

/// Asserts the bytes this release writes for `drained`: the phase, and the epoch only if there is
/// one.
#[test]
fn the_body_of_drained_carries_the_epoch_only_when_there_is_one() {
    let plain = Format {
        phase: FormatPhase::Drained,
        epoch: None,
        marks_bucket: None,
    };
    assert_eq!(
        format_body(&plain),
        Bytes::from_static(br#"{"phase":"drained"}"#)
    );
    let numbered = Format {
        phase: FormatPhase::Drained,
        epoch: Some(1),
        marks_bucket: None,
    };
    assert_eq!(
        format_body(&numbered),
        Bytes::from_static(br#"{"phase":"drained","epoch":1}"#)
    );
}

/// Asserts that a missing object is no format, an object is read with the tag the hot tier gives
/// it, a body that is not a format is an error that names the object, and an error of the hot
/// tier is passed on, for both reads.
#[tokio::test]
async fn the_format_is_read_with_its_tag_and_a_bad_read_is_an_error() {
    let hot = MemoryBackend::new();
    assert_eq!(read_format(&hot).await, Ok(None));
    assert_eq!(read_format_tagged(&hot).await, Ok(None));

    hot.put(FORMAT_KEY, Bytes::from_static(br#"{"phase":"drain"}"#))
        .await
        .unwrap();
    let want = Format {
        phase: FormatPhase::Drain,
        epoch: None,
        marks_bucket: None,
    };
    assert_eq!(read_format(&hot).await, Ok(Some(want.clone())));
    let tag = hot.head(FORMAT_KEY).await.unwrap().tag;
    assert_eq!(read_format_tagged(&hot).await, Ok(Some((want, tag))));

    let faulty = FaultyBackend::new(std::sync::Arc::new(hot));
    let down = Error::Unavailable("hot".to_owned());
    faulty.fail_next_with(Op::Get, down.clone());
    assert_eq!(read_format(&faulty).await, Err(down.clone()));
    faulty.fail_next_with(Op::Get, down.clone());
    assert_eq!(read_format_tagged(&faulty).await, Err(down));

    let bad = MemoryBackend::new();
    bad.put(FORMAT_KEY, Bytes::from_static(b"not json"))
        .await
        .unwrap();
    for error in [
        read_format(&bad).await.unwrap_err(),
        read_format_tagged(&bad).await.unwrap_err(),
    ] {
        let Error::Other(text) = error else {
            panic!("{error:?}");
        };
        assert!(text.starts_with("corrupt format object: "), "{text}");
    }
}

/// Asserts that only the phase `active` with an epoch of 2 or more makes a proxy put on entries:
/// not a missing object's absent epoch, not `active` with no epoch or epoch 1, not another phase
/// with epoch 2, and that a later epoch does.
#[test]
fn only_active_with_epoch_2_or_more_runs_entries() {
    let cases = [
        (r#"{"phase":"active"}"#, false),
        (r#"{"epoch":1,"phase":"active"}"#, false),
        (r#"{"epoch":2,"phase":"drain"}"#, false),
        (r#"{"epoch":2,"phase":"someday"}"#, false),
        (r#"{"epoch":2,"phase":"active"}"#, true),
        (r#"{"epoch":3,"phase":"active"}"#, true),
    ];
    for (json, runs) in cases {
        assert_eq!(decoded(json).runs_entries(), runs, "{json}");
    }
}

/// The fixture of the format object of the phase `active-marks`.
const ACTIVE_MARKS: &str =
    include_str!("../tests/stored_formats/fixtures/format_active_marks.json");

/// Asserts that the phase `active-marks` decodes with its epoch and the name of the marks bucket,
/// pauses no write, and does not run the entry protocol (the phase is not `active`, whatever the
/// epoch), and that the bytes this release writes for it are those of the fixture: the phase, the
/// epoch and the bucket in that order.
#[test]
fn active_marks_carries_a_marks_bucket_pauses_nothing_and_runs_no_entries() {
    let json = ACTIVE_MARKS;
    let format = decoded(json);
    let want = Format {
        phase: FormatPhase::ActiveMarks,
        epoch: Some(3),
        marks_bucket: Some("tier-hot-marks".to_owned()),
    };
    assert_eq!(format, want);
    assert!(!format.pauses_writes());
    assert!(!format.runs_entries());
    assert_eq!(&format_body(&want)[..], json.as_bytes());
}

/// Asserts that a format object with no `marks_bucket`, each of the three that releases before it
/// wrote, reads with none and is written back without the field, byte for byte, so that a
/// release that does not know the field sees the objects it always saw.
#[test]
fn a_format_without_a_marks_bucket_reads_with_none_and_is_written_without_one() {
    for json in [
        r#"{"phase":"drain"}"#,
        r#"{"phase":"drained","epoch":1}"#,
        r#"{"phase":"active","epoch":2}"#,
    ] {
        let format = decoded(json);
        assert_eq!(format.marks_bucket, None, "{json}");
        assert_eq!(&format_body(&format)[..], json.as_bytes(), "{json}");
    }
}

/// Asserts that `active-marks` is spelled with its hyphen only: the words `active_marks`,
/// `activemarks` and `Active-Marks` are phases this release does not know.
#[test]
fn only_the_hyphenated_word_is_the_phase_active_marks() {
    for word in ["active_marks", "activemarks", "Active-Marks", "marks"] {
        let format = decoded(&format!(r#"{{"phase":"{word}"}}"#));
        assert_eq!(format.phase, FormatPhase::Unknown, "{word}");
    }
}
