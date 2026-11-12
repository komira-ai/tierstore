//! The hand-over file of the put directory: the fixtures read to what they say, what is written is
//! the fixture, a later release's lines and words are left out, and the file is taken once.

use tierstore_testkit::dir::TestDir;

use super::{
    FILE, Handed, HeldDir, decode, encode, read, remove, remove_or_say, write, write_or_say,
};

/// What this release writes.
const FIXTURE: &str = include_str!("../../../../tests/stored_formats/fixtures/fs_put_handover.txt");

/// What a later release may write: words after those this one knows, lines it does not know, and
/// lines that are not what they say.
const NEWER: &str =
    include_str!("../../../../tests/stored_formats/fixtures/fs_put_handover_newer.txt");

/// A directory the kernel holds.
fn held(lookups: Option<u64>, made: bool, path: &str) -> HeldDir {
    HeldDir {
        lookups,
        made,
        path: path.to_owned(),
    }
}

/// What the fixture says.
fn fixture() -> Handed {
    Handed {
        dirs: vec![
            held(Some(3), false, "cas"),
            held(Some(1), false, "cas/b"),
            held(None, false, "cas/b/build-1"),
            held(Some(1), true, "cas/b/build-1/nonce-1/sub"),
        ],
        opens: vec![(1_099_511_627_777, 4), (2_199_023_255_554, 7)],
    }
}

/// Asserts that the fixture reads to its directories and opens, in order.
#[test]
fn the_fixture_reads_to_what_it_says() {
    assert_eq!(decode(FIXTURE), Some(fixture()));
}

/// Asserts that what this release writes is the fixture, byte for byte.
#[test]
fn what_is_written_is_the_fixture() {
    assert_eq!(encode(&fixture()), FIXTURE);
}

/// Asserts that a later release's file is read: the words after those known and the lines not
/// known are left out, and so is each line that is not what it says.
#[test]
fn a_later_file_is_read_and_what_is_not_known_left_out() {
    assert_eq!(
        decode(NEWER),
        Some(Handed {
            dirs: vec![held(Some(3), false, "cas"), held(Some(2), true, "cas/b/n")],
            opens: vec![(1_099_511_627_777, 4)],
        })
    );
}

/// Asserts that a text that is not a hand-over is none (empty, or another first line), and that a
/// later version is read by the lines this release knows.
#[test]
fn a_text_that_is_not_a_hand_over_is_none_and_a_later_version_is_read() {
    for text in ["", "tierstore-fs-init 1\n"] {
        assert_eq!(decode(text), None, "{text:?}");
    }
    assert_eq!(
        decode("tierstore-fs-put-handover 2\ndir 1 seen cas\nnew-line 3\n"),
        Some(Handed {
            dirs: vec![held(Some(1), false, "cas")],
            opens: vec![],
        })
    );
    assert_eq!(
        decode("tierstore-fs-put-handover 1"),
        Some(Handed::default())
    );
}

/// Asserts that the file is written by a rename and read until it is removed, that a file that is
/// not a hand-over is read as none and said, and that a remove of no file is no error.
#[test]
fn the_file_is_written_and_read_until_removed() {
    let dir = TestDir::under(std::env::temp_dir(), "put-carry-file");
    write(dir.path(), &fixture()).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join(FILE)).unwrap(),
        FIXTURE
    );
    assert!(!dir.path().join(format!("{FILE}.new")).exists());
    assert_eq!(read(dir.path()).unwrap(), Some(fixture()));
    assert_eq!(read(dir.path()).unwrap(), Some(fixture()));
    remove(dir.path()).unwrap();
    assert_eq!(read(dir.path()).unwrap(), None);
    std::fs::write(dir.path().join(FILE), "garbage\n").unwrap();
    let (guard, warnings) = super::super::testing::warnings();
    assert_eq!(read(dir.path()).unwrap(), None);
    drop(guard);
    let said = format!(
        "tierstore fs: {} is not a hand-over of the put directory; left out\n",
        dir.path().join(FILE).display()
    );
    assert_eq!(warnings.text(), said);
    remove(dir.path()).unwrap();
    remove(dir.path()).unwrap();
    assert!(!dir.path().join(FILE).exists());
}

/// Asserts that a directory the file cannot be written, read or removed in says so.
#[test]
fn a_file_that_cannot_be_written_or_read_is_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "put-carry-bad");
    let missing = dir.path().join("missing");
    assert!(write(&missing, &fixture()).is_err());
    std::fs::create_dir(dir.path().join(FILE)).unwrap();
    std::fs::write(dir.path().join(FILE).join("inside"), "").unwrap();
    assert!(write(dir.path(), &fixture()).is_err());
    assert!(read(dir.path()).is_err());
    assert!(remove(dir.path()).is_err());
}

/// Asserts that a write or a remove that fails is said with what the system said, and one that
/// does not says nothing.
#[test]
fn a_write_or_a_remove_that_fails_is_said() {
    let dir = TestDir::under(std::env::temp_dir(), "put-carry-say");
    let (guard, warnings) = super::super::testing::warnings();
    write_or_say(dir.path(), &fixture());
    remove_or_say(dir.path());
    assert_eq!(warnings.text(), "");
    write_or_say(&dir.path().join("missing"), &fixture());
    std::fs::create_dir(dir.path().join(FILE)).unwrap();
    remove_or_say(dir.path());
    drop(guard);
    let said = warnings.text();
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines.len(), 2, "{said}");
    assert!(lines[0].starts_with("tierstore fs: the put directory's hand-over is not written: "));
    assert!(lines[1].starts_with("tierstore fs: the put directory's hand-over is not removed: "));
}
