//! The hand-over file of the put directory: the fixtures read to what they say, what is written is
//! the fixture, a later release's lines and words are left out, and the file is taken once.

use tierstore_testkit::dir::TestDir;

use super::{FILE, Handed, HeldDir, decode, encode, remove, take, write};

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

/// Asserts that a text that is not a hand-over of this layout is none: empty, another first
/// line, or another version.
#[test]
fn a_text_that_is_not_a_hand_over_is_none() {
    for text in [
        "",
        "tierstore-fs-init 1\n",
        "tierstore-fs-put-handover 2\ndir 1 seen cas\n",
    ] {
        assert_eq!(decode(text), None, "{text:?}");
    }
    assert_eq!(
        decode("tierstore-fs-put-handover 1"),
        Some(Handed::default())
    );
}

/// Asserts that the file is written by a rename, taken once (the second take finds none), that a
/// file that is not a hand-over is taken as none and removed, and that a remove of no file is no
/// error.
#[test]
fn the_file_is_written_and_taken_once() {
    let dir = TestDir::under(std::env::temp_dir(), "put-carry-file");
    write(dir.path(), &fixture()).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join(FILE)).unwrap(),
        FIXTURE
    );
    assert!(!dir.path().join(format!("{FILE}.new")).exists());
    assert_eq!(take(dir.path()).unwrap(), Some(fixture()));
    assert_eq!(take(dir.path()).unwrap(), None);
    std::fs::write(dir.path().join(FILE), "garbage\n").unwrap();
    let (guard, warnings) = super::super::testing::warnings();
    assert_eq!(take(dir.path()).unwrap(), None);
    drop(guard);
    let said = format!(
        "tierstore fs: {} is not a hand-over of the put directory; left out\n",
        dir.path().join(FILE).display()
    );
    assert_eq!(warnings.text(), said);
    assert!(!dir.path().join(FILE).exists());
    remove(dir.path()).unwrap();
    write(dir.path(), &Handed::default()).unwrap();
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
    assert!(take(dir.path()).is_err());
    assert!(remove(dir.path()).is_err());
}
