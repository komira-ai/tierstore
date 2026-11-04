//! The journal object: the body a proxy writes under `_journal/<proxy_id>/<seq>`.

use tierstore::{JournalEntry, JournalOp, decode_journal, encode_journal};

const JOURNAL: &[u8] = include_bytes!("fixtures/journal_object.json");
const JOURNAL_NEWER: &[u8] = include_bytes!("fixtures/journal_object_newer.json");

fn entries() -> Vec<JournalEntry> {
    vec![
        JournalEntry {
            op: JournalOp::Put,
            key: "models/a.bin".to_owned(),
            size: 1_048_576,
            time: 1_790_000_000_000,
            cold_tag: None,
            hot_tag: None,
        },
        JournalEntry {
            op: JournalOp::Delete,
            key: "models/b.bin".to_owned(),
            size: 0,
            time: 1_790_000_000_500,
            cold_tag: None,
            hot_tag: None,
        },
    ]
}

/// Asserts that the journal object of the fixture, with a put and a delete entry, reads as
/// those two entries in order. Catches a renamed or removed entry field, or an operation that
/// is spelled another way.
#[test]
fn the_journal_fixture_reads_as_its_put_and_delete_entries() {
    assert_eq!(decode_journal(JOURNAL).unwrap(), entries());
}

/// Asserts that writing those two entries gives the bytes of the fixture exactly. Catches a
/// field that is renamed, reordered or re-meant in what is written.
#[test]
fn the_put_and_delete_entries_are_written_as_the_journal_fixture() {
    assert_eq!(&encode_journal(&entries())[..], JOURNAL);
}

/// Asserts that a journal object a newer writer made, with a field this reader does not know
/// and an operation it does not know, still reads: the known fields are kept, the unknown
/// operation is read as `Unknown` so that a reconciler skips it, and the extra field is ignored.
#[test]
fn a_journal_object_with_an_added_field_and_an_added_operation_still_reads() {
    let read = decode_journal(JOURNAL_NEWER).unwrap();
    let want = vec![
        JournalEntry {
            op: JournalOp::Put,
            key: "models/a.bin".to_owned(),
            size: 5,
            time: 1_790_000_000_000,
            cold_tag: None,
            hot_tag: None,
        },
        JournalEntry {
            op: JournalOp::Unknown,
            key: "models/b.bin".to_owned(),
            size: 0,
            time: 1_790_000_000_500,
            cold_tag: None,
            hot_tag: None,
        },
    ];
    assert_eq!(read, want);
}

/// Asserts that a body that is not a list of entries is refused with an error that names the
/// journal object, and not read as an empty journal.
#[test]
fn a_body_that_is_not_a_journal_is_refused() {
    let error = decode_journal(br#"{"op":"PUT"}"#).unwrap_err();
    assert!(
        error.to_string().contains("corrupt journal object"),
        "{error}"
    );
}
