//! The entry object: the body kept under `_entries/<sha256 of the key>`.

use tierstore::{Entry, EntryOp, EntryState, decode_entry, encode_entry, entry_key};

const HELD: &[u8] = include_bytes!("fixtures/entry.json");
const PENDING: &[u8] = include_bytes!("fixtures/entry_pending.json");
const NEWER: &[u8] = include_bytes!("fixtures/entry_newer.json");
const BOOTED: &[u8] = include_bytes!("fixtures/entry_booted.json");

fn held() -> Entry {
    Entry {
        v: 1,
        key: "models/a.bin".to_owned(),
        op: EntryOp::Put,
        st: EntryState::Held,
        owner: Some("node1-b7f3".to_owned()),
        nonce: "node1-b7f3.0".to_owned(),
        created_ms: 1_790_000_000_000,
        owner_boot: None,
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
        owner_boot: None,
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
        owner_boot: None,
    };
    assert_eq!(decode_entry(NEWER).unwrap(), want);
}

fn booted() -> Entry {
    Entry {
        owner_boot: Some("5e0c1d7a9b3f4c28a6d1e8f07b2c3a49".to_owned()),
        ..held()
    }
}

/// The entry as the release before `owner_boot` defines it: the fields of version 1 and no more, as
/// serde reads and writes it (an unknown field is ignored).
#[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct PreviousEntry {
    v: u32,
    key: String,
    op: EntryOp,
    st: EntryState,
    owner: Option<String>,
    nonce: String,
    created_ms: u64,
}

/// Asserts that the fixture of a held put that names the boot of its owner reads as that entry, and
/// that writing the entry gives the bytes of the fixture exactly: `owner_boot` is the last field,
/// and an entry without an owner boot is written without the field (the two fixtures above).
#[test]
fn a_held_put_that_names_the_boot_of_its_owner_reads_and_is_written_as_the_fixture() {
    assert_eq!(decode_entry(BOOTED).unwrap(), booted());
    assert_eq!(&encode_entry(&booted())[..], BOOTED);
}

/// Asserts both directions of a rolling update on the entry body: the release before reads the
/// entry that names a boot as the same entry (it ignores `owner_boot`), and an entry that release
/// rewrites, which has no `owner_boot`, reads here as the same entry with the owner's boot not
/// known.
#[test]
fn the_release_before_reads_an_entry_with_an_owner_boot_and_this_one_reads_what_it_writes() {
    let before: PreviousEntry = serde_json::from_slice(BOOTED).unwrap();
    let held = held();
    assert_eq!(
        (before.v, before.key.as_str(), before.op, before.st),
        (held.v, held.key.as_str(), held.op, held.st)
    );
    assert_eq!(before.owner, held.owner);
    assert_eq!(
        (before.nonce.as_str(), before.created_ms),
        (held.nonce.as_str(), held.created_ms)
    );

    let rewritten = serde_json::to_vec(&before).unwrap();
    assert_eq!(decode_entry(&rewritten).unwrap(), held);
}

/// Asserts the name an entry is kept under for a key of the fixtures: the prefix and the SHA-256 of
/// the key in hex, so that a release that names it another way does not find the entries of an
/// older one.
#[test]
fn the_entry_of_a_key_is_kept_under_the_hash_of_the_key() {
    let name = "_entries/bad625a3890ea33470a191b8cf197639d3565ab738e4d040304dbf4c1707d7af";
    assert_eq!(entry_key("models/a.bin"), name);
}
