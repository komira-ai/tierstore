//! The verb that is gone: whatever it is given, it exits 2 and names the interface.

use super::{NOT_A_PROCESS, run};
use crate::server::verbs::testing::{args, status};

/// Runs `tierstore fs <words>` and gives its status and what it wrote to stdout and stderr.
fn fs(words: &[&str]) -> (String, String, String) {
    let mut all = vec!["fs"];
    all.extend_from_slice(words);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args(&all), &mut out, &mut err);
    (
        format!("{code:?}"),
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Asserts that a start of the verb (`--config` alone) is status 2 whatever the file, which need
/// not exist: nothing on stdout and, on stderr, the one line that says the file system is the `fs`
/// interface of the node's file.
#[test]
fn the_fs_verb_exits_2_and_names_the_interface() {
    let dir =
        tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tfs-verb-not-a-process");
    let file = dir.path().join("fs.toml");
    let (code, out, err) = fs(&["--config", file.to_str().unwrap()]);
    assert_eq!(code, status(2));
    assert!(out.is_empty(), "{out}");
    assert_eq!(err, format!("{NOT_A_PROCESS}\n"));
    assert!(err.contains("[[interface]]"), "{err}");
    assert!(err.contains("type = \"fs\""), "{err}");
}

/// Asserts that the forms of the old verb, the check of a file, the list of attachments, either
/// flag order, and no arguments at all, are each status 2 with the same line and nothing on
/// stdout, so that a unit of the old layout is stopped and told why.
#[test]
fn every_form_of_the_old_verb_exits_2_with_the_same_line() {
    for words in [
        &["--config", "fs.toml", "--check-config"][..],
        &["--check-config", "--config", "fs.toml"],
        &["attachments", "--config", "fs.toml"],
        &["--config", "fs.toml", "attachments"],
        &["--bogus"],
        &[],
    ] {
        let (code, out, err) = fs(words);
        assert_eq!(code, status(2), "{words:?}");
        assert!(out.is_empty(), "{words:?}: {out}");
        assert_eq!(err, format!("{NOT_A_PROCESS}\n"), "{words:?}");
    }
}

/// Asserts that a request for help is a success that says where the verb went, and that the verb
/// is answered by name before the flags of a start are read.
#[test]
fn help_of_the_verb_says_where_it_went() {
    let (code, out, err) = fs(&["--help"]);
    assert_eq!(code, status(0));
    assert!(err.is_empty(), "{err}");
    assert!(out.contains("tierstore attachments"), "{out}");
    let answered = crate::server::verbs::answer(&args(&["fs", "--help"])).unwrap();
    assert_eq!(format!("{answered:?}"), status(0));
}
