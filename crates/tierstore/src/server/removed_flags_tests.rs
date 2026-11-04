//! The flags of the releases before the entries are dropped from the arguments with one warning
//! each.

use std::ffi::OsString;

use super::drop_removed;

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

/// Asserts that each removed flag and the value after it are taken out of the arguments, the other
/// flags are kept in order, and the warnings come one for each removed flag, naming it and what
/// replaced it.
#[test]
fn a_removed_flag_and_its_value_are_dropped_with_one_warning() {
    let (kept, warnings) = drop_removed(args(&[
        "tierstore",
        "--journal-linger",
        "20ms",
        "--hot",
        "memory",
        "--journal-retention",
        "24h",
        "--reconcile-interval",
        "5s",
        "--registration-retention",
        "0s",
        "--cold",
        "memory",
    ]));
    assert_eq!(
        kept,
        args(&["tierstore", "--hot", "memory", "--cold", "memory"])
    );
    assert_eq!(
        warnings,
        [
            "tierstore: --journal-linger is ignored: writes are made on the entry of their key, \
             and there is no journal to batch",
            "tierstore: --journal-retention is ignored: there is no per-node journal to keep; the \
             old objects are removed by the release after this one",
            "tierstore: --reconcile-interval is ignored: the reconciler waits as long as its own \
             idle wait says, and not at all while it makes progress",
            "tierstore: --registration-retention is ignored: the registration of a proxy is no \
             longer removed by a journal check",
        ]
    );
}

/// Asserts that the `--flag=value` form is dropped without taking the next argument, and that a
/// flag given twice, in both forms, is warned about once.
#[test]
fn the_equals_form_keeps_the_next_argument_and_a_repeated_flag_is_warned_once() {
    let (kept, warnings) = drop_removed(args(&[
        "tierstore",
        "--journal-linger=5ms",
        "--hot",
        "--journal-linger",
        "7ms",
    ]));
    assert_eq!(kept, args(&["tierstore", "--hot"]));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].starts_with("tierstore: --journal-linger is ignored: "));
}

/// Asserts that arguments that only begin like a removed flag, an argument that is not UTF-8, and
/// a removed flag at the very end with no value after it are each handled: the first two kept, the
/// last dropped with its warning.
#[test]
fn names_that_only_look_alike_are_kept_and_a_flag_without_a_value_is_dropped() {
    let odd = OsString::from_vec(vec![0xff, b'x']);
    let mut given = args(&[
        "tierstore",
        "--journal-lingering=1",
        "--reconcile-interval-x",
        "journal-linger",
    ]);
    given.push(odd.clone());
    given.push(OsString::from("--registration-retention"));
    let (kept, warnings) = drop_removed(given);
    let mut want = args(&[
        "tierstore",
        "--journal-lingering=1",
        "--reconcile-interval-x",
        "journal-linger",
    ]);
    want.push(odd);
    assert_eq!(kept, want);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("--registration-retention"));
}

/// Asserts that arguments with none of the removed flags come back as they were, with no warning,
/// and that no arguments at all is no arguments.
#[test]
fn arguments_without_a_removed_flag_come_back_unchanged() {
    let given = args(&["tierstore", "--config", "/etc/tierstore/tierstore.toml"]);
    let (kept, warnings) = drop_removed(given.clone());
    assert_eq!(kept, given);
    assert!(warnings.is_empty());
    let (kept, warnings) = drop_removed(Vec::new());
    assert!(kept.is_empty() && warnings.is_empty());
}
