//! The body of an entry: the bytes it is written as, what reads back, and what a reader of another
//! release makes of it.

use crate::entry_tests::entry;
use crate::{Entry, EntryOp, EntryState, decode_entry, encode_entry, entry_key};

/// Asserts that the name of an entry is the prefix and the SHA-256 of the key in hex (the digest of
/// "abc" is the well-known one), that it has the same length for a key of 1024 bytes, and that two
/// keys have two names.
#[test]
fn the_entry_name_is_the_prefix_and_the_sha256_hex_of_the_key_at_any_length() {
    let abc = "_entries/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(entry_key("abc"), abc);
    assert_eq!(entry_key(&"k".repeat(1024)).len(), "_entries/".len() + 64);
    assert_ne!(entry_key("abc"), entry_key("abd"));
}

/// Asserts the exact bytes of a held entry and of a pending one, whose owner is written as `null`,
/// and that both read back as the entries written.
#[test]
fn an_entry_is_written_as_json_with_a_null_owner_and_reads_back() {
    let held = entry(EntryOp::Put, EntryState::Held, Some("p1"));
    let want =
        r#"{"v":1,"key":"k","op":"PUT","st":"HELD","owner":"p1","nonce":"seed","created_ms":5}"#;
    assert_eq!(encode_entry(&held), want);
    assert_eq!(decode_entry(want.as_bytes()).unwrap(), held);

    let pending = entry(EntryOp::Del, EntryState::Pending, None);
    let want =
        r#"{"v":1,"key":"k","op":"DEL","st":"PENDING","owner":null,"nonce":"seed","created_ms":5}"#;
    assert_eq!(encode_entry(&pending), want);
    assert_eq!(decode_entry(want.as_bytes()).unwrap(), pending);
}

/// Asserts that every operation and state is spelled as the format says.
#[test]
fn every_operation_and_state_has_its_word() {
    let ops = [
        (EntryOp::Put, "PUT"),
        (EntryOp::Del, "DEL"),
        (EntryOp::Cb, "CB"),
        (EntryOp::Evict, "EVICT"),
    ];
    for (op, word) in ops {
        let body = encode_entry(&entry(op, EntryState::Sealed, None));
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains(&format!(r#""op":"{word}""#)), "{text}");
    }
    let states = [
        (EntryState::Held, "HELD"),
        (EntryState::Pending, "PENDING"),
        (EntryState::Sealed, "SEALED"),
    ];
    for (st, word) in states {
        let body = encode_entry(&entry(EntryOp::Put, st, None));
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains(&format!(r#""st":"{word}""#)), "{text}");
    }
}

/// Asserts that a body a newer writer made, with a field this reader does not know, a later
/// version, and an operation and a state it does not know, still reads: the unknown words are
/// `Unknown`, and the field is ignored.
#[test]
fn a_body_with_added_fields_and_words_still_reads() {
    let body = r#"{"v":2,"key":"k","op":"XFER","st":"FROZEN","owner":null,"nonce":"n","created_ms":9,"extra":[1]}"#;
    let read = decode_entry(body.as_bytes()).unwrap();
    let want = Entry {
        v: 2,
        key: "k".to_owned(),
        op: EntryOp::Unknown,
        st: EntryState::Unknown,
        owner: None,
        nonce: "n".to_owned(),
        created_ms: 9,
        owner_boot: None,
    };
    assert_eq!(read, want);
}

/// Asserts that a body that is not an entry is refused with an error that says it is a corrupt
/// entry.
#[test]
fn a_body_that_is_not_an_entry_is_refused() {
    let error = decode_entry(b"{}").unwrap_err();
    assert!(error.to_string().contains("corrupt entry"), "{error}");
}

/// Asserts that the owner's boot is written as the last field when the entry has one, and left out
/// when it has none, so that an entry without one is the bytes of the format before the field, and
/// that both read back as the entries written.
#[test]
fn the_owner_boot_is_written_after_the_other_fields_and_left_out_when_there_is_none() {
    let mut booted = entry(EntryOp::Put, EntryState::Held, Some("p1"));
    booted.owner_boot = Some("b1".to_owned());
    let want = r#"{"v":1,"key":"k","op":"PUT","st":"HELD","owner":"p1","nonce":"seed","created_ms":5,"owner_boot":"b1"}"#;
    assert_eq!(encode_entry(&booted), want);
    assert_eq!(decode_entry(want.as_bytes()).unwrap(), booted);

    let plain = entry(EntryOp::Put, EntryState::Held, Some("p1"));
    let text = String::from_utf8(encode_entry(&plain).to_vec()).unwrap();
    assert!(!text.contains("owner_boot"), "{text}");
}
