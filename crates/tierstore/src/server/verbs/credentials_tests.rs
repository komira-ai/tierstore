//! The credential flags of the verbs that open the hot tier: a profile with a source that reads
//! none is refused before the tier is opened, and a source that is not one of the names is a flag
//! that is not valid.

use super::testing::{args, status};
use super::{entries, format_epoch};
use crate::{RecordingSleeper, TestClock};

/// The flags of an `s3:<bucket>` hot tier whose credentials are the environment's, with a profile.
const PROFILE_WITHOUT_USE: [&str; 6] = [
    "--hot",
    "s3:bucket",
    "--hot-credential-source",
    "environment",
    "--hot-credential-profile",
    "work",
];

/// The status, stdout and stderr of `verb` run on `words`.
fn ran(verb: &str, words: &[&str]) -> (String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let clock = TestClock::new();
    let code = match verb {
        "entries" => entries::run(&args(words), &mut out, &mut err, &clock),
        _ => {
            let sleeper = RecordingSleeper::new();
            format_epoch::run(&args(words), &mut out, &mut err, &clock, &sleeper)
        }
    };
    (format!("{code:?}"), String::from_utf8(err).unwrap())
}

/// Asserts that `entries` and `format-epoch` refuse a profile given with a credential source that
/// reads none, status 1, with the message exactly.
#[test]
fn a_profile_with_a_source_that_reads_none_is_refused() {
    let why = "--hot-credential-profile: only for the credential sources \"profile\" and \"auto\"";
    let mut words = vec!["entries", "ls"];
    words.extend(PROFILE_WITHOUT_USE);
    assert_eq!(
        ran("entries", &words),
        (status(1), format!("tierstore entries: {why}\n"))
    );
    let mut words = vec!["format-epoch"];
    words.extend(PROFILE_WITHOUT_USE);
    words.push("--check");
    assert_eq!(
        ran("format-epoch", &words),
        (status(1), format!("tierstore format-epoch: {why}\n"))
    );
}

/// Asserts that a credential source that is not one of the names is a flag that is not valid,
/// status 2; that `file` is taken for any tier; and that each source is taken as a name and then
/// refused for a tier that is not `s3:<bucket>`, status 1, with the message exactly.
#[test]
fn a_source_that_is_not_a_name_is_not_valid() {
    let unnamed = [
        "entries",
        "ls",
        "--hot",
        "memory",
        "--hot-credential-source",
        "imdsv1",
    ];
    assert_eq!(ran("entries", &unnamed).0, status(2));
    let file = [
        "entries",
        "ls",
        "--hot",
        "memory",
        "--hot-credential-source",
        "file",
    ];
    assert_eq!(ran("entries", &file), (status(0), String::new()));
    let why = "tierstore entries: --hot \"memory\": its endpoint, region and credentials flags are \
               for an `s3:<bucket>` tier\n";
    for name in [
        "auto",
        "environment",
        "profile",
        "web-identity",
        "container",
        "instance-metadata",
    ] {
        let words = [
            "entries",
            "ls",
            "--hot",
            "memory",
            "--hot-credential-source",
            name,
        ];
        assert_eq!(
            ran("entries", &words),
            (status(1), why.to_owned()),
            "{name}"
        );
    }
}
