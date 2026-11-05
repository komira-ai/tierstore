//! The formats of the journal and checkpoint objects of the releases before the entries.

use bytes::Bytes;

use super::{
    Checkpoint, ENTRIES_PREFIX, FORMAT_KEY, JOURNAL_PREFIX, JournalEntry, JournalOp, LEASE_KEY,
    RESERVED_PREFIXES, TIERING_PREFIX, USAGE_KEY, decode_checkpoint, decode_journal,
    encode_checkpoint, encode_journal, is_reserved, proxy_key,
};
use crate::Error;

fn entry(op: JournalOp, key: &str) -> JournalEntry {
    JournalEntry {
        op,
        key: key.to_owned(),
        size: 7,
        time: 1_790_000_000_007,
        cold_tag: None,
        hot_tag: None,
    }
}

/// Asserts that a journal object of every op round-trips, and that the fields a plain entry does
/// not have are left out of the bytes.
#[test]
fn a_journal_object_round_trips_and_leaves_out_absent_tags() {
    let mut mirror = entry(JournalOp::PutMirror, "c");
    mirror.cold_tag = Some("cold".to_owned());
    mirror.hot_tag = Some("hot".to_owned());
    let entries = [
        entry(JournalOp::Put, "a"),
        entry(JournalOp::Delete, "d"),
        mirror,
    ];
    let body = encode_journal(&entries);
    assert_eq!(decode_journal(&body).unwrap(), entries);
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains(r#""op":"PUT""#), "{text}");
    assert!(text.contains(r#""cold_tag":"cold""#), "{text}");
    assert_eq!(text.matches("cold_tag").count(), 1, "{text}");
}

/// Asserts that an entry of an op this release does not know is read as `Unknown`, and an entry
/// written before the tags existed is read with none.
#[test]
fn an_unknown_op_is_read_as_unknown_and_missing_tags_as_none() {
    let body = br#"[{"op":"FUTURE","key":"k","size":1,"time":2}]"#;
    let read = decode_journal(body).unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].op, JournalOp::Unknown);
    assert_eq!(
        (read[0].cold_tag.clone(), read[0].hot_tag.clone()),
        (None, None)
    );
}

/// Asserts that a journal body that is not a list of entries is an error that says so.
#[test]
fn a_corrupt_journal_object_is_an_error() {
    let error = decode_journal(b"{").unwrap_err();
    let Error::Other(message) = error else {
        panic!("{error:?}");
    };
    assert!(message.starts_with("corrupt journal object: "), "{message}");
}

/// Asserts that a checkpoint round-trips as the three numbers the release before wrote.
#[test]
fn a_checkpoint_round_trips_as_its_three_numbers() {
    let checkpoint = Checkpoint {
        seq: 4,
        evict_seq: 3,
        evict_idx: 1,
    };
    let body = encode_checkpoint(&checkpoint);
    assert_eq!(
        body,
        Bytes::from_static(br#"{"seq":4,"evict_seq":3,"evict_idx":1}"#)
    );
    assert_eq!(decode_checkpoint(&body).unwrap(), checkpoint);
    assert_eq!(Checkpoint::default().seq, 0);
}

/// Asserts that a checkpoint body that does not decode is an error that says so.
#[test]
fn a_corrupt_checkpoint_object_is_an_error() {
    let error = decode_checkpoint(b"[]").unwrap_err();
    let Error::Other(message) = error else {
        panic!("{error:?}");
    };
    assert!(
        message.starts_with("corrupt checkpoint object: "),
        "{message}"
    );
}

/// Asserts that every key the crate writes for itself (the lease, the format, the usage, the
/// registrations, the checkpoints, the entries and the journals) is under a reserved prefix, so
/// that none is a key a user may own.
#[test]
fn internal_keys_are_under_reserved_prefixes() {
    let internal = [
        LEASE_KEY.to_owned(),
        FORMAT_KEY.to_owned(),
        USAGE_KEY.to_owned(),
        proxy_key("p1"),
        "_tiering/checkpoint/p1".to_owned(),
        crate::entry::entry_key("a/b"),
        crate::entry::entry_key(""),
        "_journal/p1/0000000000000001".to_owned(),
    ];
    for key in &internal {
        assert!(is_reserved(key), "{key} is not under a reserved prefix");
    }
    assert!(!is_reserved("tiering/lease"));
    assert!(!is_reserved("_other/x"));
}

/// Asserts that the reserved set is exactly the three prefixes, so that widening it is a
/// reviewed change.
#[test]
fn reserved_set_is_the_three_prefixes() {
    assert_eq!(RESERVED_PREFIXES, ["_journal/", "_tiering/", "_entries/"]);
    assert_eq!(
        RESERVED_PREFIXES,
        [JOURNAL_PREFIX, TIERING_PREFIX, ENTRIES_PREFIX]
    );
}
