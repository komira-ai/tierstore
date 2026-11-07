//! The format object: which bodies decode, which phases pause writes, and how it is read.

use bytes::Bytes;

use crate::faulty::{FaultyBackend, Op};
use crate::format::{
    Format, FormatPhase, format_body, marks_active, read_format, read_format_tagged,
};
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

/// Asserts that only the phase `active-marks`, at epoch 3 or later and with the bucket named, makes
/// a proxy write on markers: not a record without the epoch or the name, not `active` at any epoch,
/// not a pause, not a phase it does not know.
#[test]
fn only_active_marks_runs_marks() {
    let cases = [
        (r#"{"phase":"active-marks"}"#, None),
        (
            r#"{"epoch":3,"marks_bucket":"b","phase":"active-marks"}"#,
            Some("b"),
        ),
        (r#"{"epoch":3,"phase":"active"}"#, None),
        (r#"{"phase":"drain"}"#, None),
        (r#"{"phase":"someday"}"#, None),
    ];
    for (json, runs) in cases {
        let format = decoded(json);
        assert_eq!(format.runs_marks(), runs, "{json}");
    }
}

/// Asserts that only the phase `active-marks` is active for the leader's passes, whatever the epoch
/// and the name, and that `marks_active` reads it from the hot tier: no object, another phase and a
/// phase this release does not know are false, and a hot tier that fails the read is that error.
#[tokio::test]
async fn only_active_marks_runs_the_markers() {
    for (json, runs) in [
        (
            r#"{"epoch":3,"phase":"active-marks","marks_bucket":"m"}"#,
            true,
        ),
        (r#"{"phase":"active-marks"}"#, true),
        (r#"{"epoch":3,"phase":"active"}"#, false),
        (r#"{"phase":"drained"}"#, false),
        (r#"{"phase":"someday"}"#, false),
    ] {
        assert_eq!(decoded(json).is_active_marks(), runs, "{json}");
        let hot = MemoryBackend::new();
        hot.put(FORMAT_KEY, Bytes::from(json)).await.unwrap();
        assert_eq!(marks_active(&hot).await, Ok(runs), "{json}");
    }
    let empty = MemoryBackend::new();
    assert_eq!(marks_active(&empty).await, Ok(false));
    let corrupt = MemoryBackend::new();
    corrupt.put(FORMAT_KEY, Bytes::from("{")).await.unwrap();
    assert!(marks_active(&corrupt).await.is_err());
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

/// Asserts when a record opens writes on markers and to which bucket: the phase `active-marks`, an
/// epoch of 3 or more, and a name for the bucket, all three; not the phase `active`, not an epoch
/// below 3 or none, not a record without a name.
#[test]
fn a_record_opens_writes_on_markers_only_with_the_phase_the_epoch_and_the_bucket() {
    let opens = decoded(r#"{"phase":"active-marks","epoch":3,"marks_bucket":"m"}"#);
    assert_eq!(opens.runs_marks(), Some("m"));
    let later = decoded(r#"{"phase":"active-marks","epoch":4,"marks_bucket":"m"}"#);
    assert_eq!(later.runs_marks(), Some("m"));
    let shut = [
        r#"{"phase":"active-marks","epoch":2,"marks_bucket":"m"}"#,
        r#"{"phase":"active-marks","marks_bucket":"m"}"#,
        r#"{"phase":"active-marks","epoch":3}"#,
        r#"{"phase":"active","epoch":3,"marks_bucket":"m"}"#,
    ];
    for json in shut {
        assert_eq!(decoded(json).runs_marks(), None, "{json}");
    }
    assert!(!opens.runs_entries());
    assert!(!opens.pauses_writes());
}

/// Asserts that only `drain` with the marks bucket still named is the pause of a rollback from
/// `active-marks`: it gives the leader a pass over the markers, pauses writes, and runs neither
/// the entries nor the markers for a proxy; the `drain` that precedes the activation (no name),
/// `drained` with a name and `active-marks` are not it.
#[test]
fn only_drain_that_names_the_marks_bucket_is_the_pause_of_a_rollback() {
    let rollback = decoded(r#"{"phase":"drain","epoch":3,"marks_bucket":"m"}"#);
    assert!(rollback.rolls_back_marks());
    assert!(rollback.runs_a_pass());
    assert!(rollback.pauses_writes());
    assert_eq!(rollback.runs_marks(), None);
    assert!(!rollback.runs_entries());
    for json in [
        r#"{"phase":"drain"}"#,
        r#"{"phase":"drain","epoch":2}"#,
        r#"{"phase":"drained","marks_bucket":"m"}"#,
    ] {
        let format = decoded(json);
        assert!(!format.rolls_back_marks(), "{json}");
        assert!(!format.runs_a_pass(), "{json}");
    }
    let active_marks = decoded(r#"{"phase":"active-marks"}"#);
    assert!(active_marks.runs_a_pass());
    assert!(!active_marks.rolls_back_marks());
}
