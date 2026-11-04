//! The dispatch of the verbs.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use super::answer;
use super::testing::{args, status};

/// Asserts that a start (no verb, or a flag where the verb would be, or an argument that is not
/// text) is not taken for a verb, so that the server's flags are parsed. It would catch a verb
/// that answers what is not its own.
#[test]
fn what_is_not_a_verb_is_left_to_the_flags_of_a_start() {
    assert!(answer(&args(&[])).is_none());
    assert!(answer(&args(&["--listen", "127.0.0.1:0"])).is_none());
    assert!(answer(&args(&["bounce"])).is_none());
    let not_text = vec![OsString::from("tierstore"), OsString::from_vec(vec![0xff])];
    assert!(answer(&not_text).is_none());
}

/// Asserts that `format-epoch` and `entries` are answered by name, before any flag of a start is
/// read: help for each is a status 0.
#[test]
fn the_operator_verbs_are_answered_by_name() {
    let epoch = answer(&args(&["format-epoch", "--help"])).unwrap();
    assert_eq!(format!("{epoch:?}"), status(0));
    let entries = answer(&args(&["entries", "--help"])).unwrap();
    assert_eq!(format!("{entries:?}"), status(0));
}
