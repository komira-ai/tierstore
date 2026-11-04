//! The entry object: the body kept under `_entries/<sha256 of the key>`.

use tierstore::{Entry, EntryOp, EntryState, decode_entry, encode_entry, entry_key};

const HELD: &[u8] = include_bytes!("fixtures/entry.json");
const PENDING: &[u8] = include_bytes!("fixtures/entry_pending.json");
const NEWER: &[u8] = include_bytes!("fixtures/entry_newer.json");

fn held() -> Entry {
    Entry {
        v: 1,
        key: "models/a.bin".to_owned(),
        op: EntryOp::Put,
        st: EntryState::Held,
        owner: Some("node1-b7f3".to_owned()),
        nonce: "node1-b7f3.0".to_owned(),
        created_ms: 1_790_000_000_000,
    }
}

fn pending() -> Entry {
    Entry {
        v: 1,
        key: "models/a.bin".to_owned(),
        op: EntryOp::Del,
        st: EntryState::Pending,
        owner: None,
        nonce: "node1-b7f3.4".to_owned(),
        created_ms: 1_790_000_000_000,
    }
}

/// Asserts that the fixture of a held put reads as that entry and the fixture of a handed-over
/// delete, whose owner is `null`, reads as that entry. Catches a renamed or removed field, or an
/// operation or state spelled another way.
#[test]
fn the_entry_fixtures_read_as_a_held_put_and_a_pending_delete() {
    assert_eq!(decode_entry(HELD).unwrap(), held());
    assert_eq!(decode_entry(PENDING).unwrap(), pending());
}

/// Asserts that writing those two entries gives the bytes of the fixtures exactly. Catches a field
/// that is renamed, reordered or re-meant in what is written.
#[test]
fn the_held_put_and_the_pending_delete_are_written_as_the_entry_fixtures() {
    assert_eq!(&encode_entry(&held())[..], HELD);
    assert_eq!(&encode_entry(&pending())[..], PENDING);
}

/// Asserts that an entry a newer writer made, with a version, a field, an operation and a state
/// this reader does not know, still reads: the known fields are kept and the unknown words read as
/// `Unknown`, which no transition accepts.
#[test]
fn an_entry_with_an_added_field_operation_and_state_still_reads() {
    let want = Entry {
        v: 2,
        key: "models/a.bin".to_owned(),
        op: EntryOp::Unknown,
        st: EntryState::Unknown,
        owner: Some("node1-b7f3".to_owned()),
        nonce: "node1-b7f3.9".to_owned(),
        created_ms: 1_790_000_000_000,
    };
    assert_eq!(decode_entry(NEWER).unwrap(), want);
}

/// Asserts the name an entry is kept under for a key of the fixtures: the prefix and the SHA-256 of
/// the key in hex, so that a release that names it another way does not find the entries of an
/// older one.
#[test]
fn the_entry_of_a_key_is_kept_under_the_hash_of_the_key() {
    let name = "_entries/bad625a3890ea33470a191b8cf197639d3565ab738e4d040304dbf4c1707d7af";
    assert_eq!(entry_key("models/a.bin"), name);
}
