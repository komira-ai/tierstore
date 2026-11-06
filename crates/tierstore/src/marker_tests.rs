//! The body of a marker: what is written, what reads back, and what is kept of what a reader does
//! not know.

use crate::Error;
use crate::marker::{MARKER_VERSION, Marker, MarkerState, MarkerWhy, decode_marker, encode_marker};

fn marker(st: MarkerState) -> Marker {
    Marker {
        v: MARKER_VERSION,
        key: "a/b".to_owned(),
        st,
        gone: false,
        dirty: true,
        why: None,
        owner: None,
        nonce: "00112233445566778899aabbccddeeff".to_owned(),
        extra: serde_json::Map::new(),
    }
}

/// Asserts the bytes of a marker with no `why` and no `owner`: the fields in the order of the
/// design, the two left out.
#[test]
fn a_marker_without_why_or_owner_is_written_without_them() {
    let body = encode_marker(&marker(MarkerState::Want));
    let want = r#"{"v":1,"key":"a/b","st":"WANT","gone":false,"dirty":true,"nonce":"00112233445566778899aabbccddeeff"}"#;
    assert_eq!(&body[..], want.as_bytes());
}

/// Asserts that each state and each reason is spelled as the design spells it, and reads back as
/// itself.
#[test]
fn every_state_and_reason_is_spelled_as_the_design_spells_it() {
    let states = [
        (MarkerState::Dirty, "DIRTY"),
        (MarkerState::Lock, "LOCK"),
        (MarkerState::Clearing, "CLEARING"),
        (MarkerState::Want, "WANT"),
    ];
    for (state, word) in states {
        let body = encode_marker(&marker(state.clone()));
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains(&format!(r#""st":"{word}""#)), "{text}");
        assert_eq!(decode_marker(&body).unwrap().st, state);
    }
    let reasons = [
        (MarkerWhy::Overwrite, "OVERWRITE"),
        (MarkerWhy::Delete, "DELETE"),
        (MarkerWhy::Evict, "EVICT"),
        (MarkerWhy::CopyBack, "COPYBACK"),
        (MarkerWhy::ColdLand, "COLDLAND"),
    ];
    for (why, word) in reasons {
        let locked = Marker {
            why: Some(why.clone()),
            owner: Some("p1".to_owned()),
            ..marker(MarkerState::Lock)
        };
        let body = encode_marker(&locked);
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains(&format!(r#""why":"{word}","owner":"p1""#)),
            "{text}"
        );
        assert_eq!(decode_marker(&body).unwrap(), locked);
    }
}

/// Asserts that a state and a reason this release does not know are kept as the words they were
/// and written back as those words, and are not any of the known ones.
#[test]
fn a_state_and_a_reason_this_release_does_not_know_are_kept_as_written() {
    let body =
        br#"{"v":1,"key":"k","st":"Dirty","gone":true,"dirty":false,"why":"delete","nonce":"n"}"#;
    let read = decode_marker(body).unwrap();
    assert_eq!(read.st, MarkerState::Other("Dirty".to_owned()));
    assert_eq!(read.why, Some(MarkerWhy::Other("delete".to_owned())));
    assert_eq!(&encode_marker(&read)[..], &body[..]);
}

/// Asserts that fields this release does not know are kept in `extra`, with their values whatever
/// their shape, and written back after the known fields, ordered by name.
#[test]
fn fields_this_release_does_not_know_are_kept_and_written_back() {
    let body = br#"{"v":2,"key":"k","st":"DIRTY","gone":false,"dirty":true,"nonce":"n","z":1,"a":{"b":[true,null]}}"#;
    let read = decode_marker(body).unwrap();
    assert_eq!(read.v, 2);
    let names: Vec<&str> = read.extra.keys().map(String::as_str).collect();
    assert_eq!(names, ["a", "z"]);
    let want = br#"{"v":2,"key":"k","st":"DIRTY","gone":false,"dirty":true,"nonce":"n","a":{"b":[true,null]},"z":1}"#;
    assert_eq!(&encode_marker(&read)[..], &want[..]);
}

/// Asserts that a body that is not a marker is an error that names the marker: not JSON, JSON of
/// another shape, and an object that lacks a field the marker needs.
#[test]
fn a_body_that_is_not_a_marker_is_an_error_naming_the_marker() {
    for body in [
        &b""[..],
        b"not json",
        b"[]",
        b"{}",
        br#"{"v":1,"key":"k","st":"DIRTY","gone":false,"dirty":true}"#,
        br#"{"v":1,"key":"k","st":"DIRTY","gone":"no","dirty":true,"nonce":"n"}"#,
    ] {
        let Err(Error::Other(text)) = decode_marker(body) else {
            panic!("{body:?}");
        };
        assert!(text.starts_with("corrupt marker: "), "{text}");
    }
}

/// Asserts which states a writer other than the owner waits for: a lock and a clearing, and a
/// state this release does not know, which the design treats as a lock; not `DIRTY` and `WANT`.
#[test]
fn a_writer_waits_for_a_lock_a_clearing_and_a_state_it_does_not_know() {
    let held = |st: MarkerState| st.is_held();
    assert!(!held(MarkerState::Dirty));
    assert!(!held(MarkerState::Want));
    assert!(held(MarkerState::Lock));
    assert!(held(MarkerState::Clearing));
    assert!(held(MarkerState::Other("FROZEN".to_owned())));
}

/// Asserts which reasons leave the key owing the cold tier a copy: all but an eviction and a
/// copy-back, and a reason this release does not know is taken to owe one.
#[test]
fn only_an_eviction_and_a_copy_back_leave_the_tiers_equal() {
    let owes = [
        (MarkerWhy::Overwrite, true),
        (MarkerWhy::Delete, true),
        (MarkerWhy::ColdLand, true),
        (MarkerWhy::Other("REBALANCE".to_owned()), true),
        (MarkerWhy::Evict, false),
        (MarkerWhy::CopyBack, false),
    ];
    for (why, owed) in owes {
        assert_eq!(why.owes_cold_a_copy(), owed, "{why:?}");
    }
}
