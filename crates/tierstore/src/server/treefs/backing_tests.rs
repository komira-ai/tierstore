//! The backing ids an earlier release left: the file read, the ids kept and never closed, and the
//! file written once with no count marked exact.

use tierstore_testkit::dir::TestDir;

use super::{Adopted, FILE, Saved, saved};
use crate::server::treefs::locked;
use crate::server::treefs::testing::{FakeKernel, sha};
use crate::tree_manifest::TreeDigest;

fn blob(name: &str) -> TreeDigest {
    TreeDigest::from_hex(&sha(name.as_bytes())).unwrap()
}

/// The table of two blobs, `a` and `b`, as an earlier release wrote it at a clean stop: the
/// fixture.
const FIXTURE: &str = include_str!("../../../tests/stored_formats/fixtures/fs_backing_table.txt");

/// The same with words a later release adds to a line, and a line that is nothing.
const NEWER: &str =
    include_str!("../../../tests/stored_formats/fixtures/fs_backing_table_newer.txt");

/// The two lines of the fixture.
fn fixture_lines() -> Vec<Saved> {
    vec![
        Saved {
            blob: blob("b"),
            id: 101,
            opens: 1,
            exact: false,
        },
        Saved {
            blob: blob("a"),
            id: 100,
            opens: 2,
            exact: true,
        },
    ]
}

/// Asserts that the fixture reads to its two lines (one counted exactly), that a later release's
/// extra words are ignored, that a line that is not a line is left out, and that a missing file
/// is no lines.
#[test]
fn the_file_is_read_line_by_line_and_a_later_releases_words_are_ignored() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-saved");
    assert_eq!(saved(&dir.path().join(FILE)).unwrap(), Vec::new());
    std::fs::write(dir.path().join(FILE), FIXTURE).unwrap();
    assert_eq!(saved(&dir.path().join(FILE)).unwrap(), fixture_lines());
    std::fs::write(dir.path().join(FILE), NEWER).unwrap();
    assert_eq!(saved(&dir.path().join(FILE)).unwrap(), fixture_lines());
    for bad in [
        "x 1 1",
        &format!("{} one 1", blob("a")),
        &format!("{} 1", blob("a")),
    ] {
        std::fs::write(dir.path().join(FILE), bad).unwrap();
        assert_eq!(saved(&dir.path().join(FILE)).unwrap(), Vec::new(), "{bad}");
    }
    // A path that is not a file is an error, not an empty table.
    assert!(saved(dir.path()).is_err());
}

/// Asserts that the ids of the fixture are had by blob, each the handle `wrap` made for its id,
/// that a blob with no line has none, and that no id is closed, not even when the ids are dropped.
#[test]
fn the_adopted_ids_are_had_by_blob_and_never_closed() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-ids");
    let kernel = FakeKernel::new();
    let adopted = Adopted::new(&dir.path().join(FILE), &fixture_lines(), &|id| {
        kernel.wrap(id)
    });
    assert_eq!(adopted.len(), 2);
    let id = |name: &str| adopted.get(&blob(name)).and_then(FakeKernel::id_of);
    assert_eq!((id("a"), id("b"), id("c")), (Some(100), Some(101), None));
    drop(adopted);
    assert!(locked(&kernel.closed).is_empty());
}

/// Asserts that the file is written again as the fixture has it with the `exact` mark taken off,
/// in the order of the blobs, when a line has it, and is not written when none has.
#[test]
fn the_file_is_written_once_without_the_mark_when_a_line_has_it() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-unmark");
    let path = dir.path().join(FILE);
    let kernel = FakeKernel::new();
    std::fs::write(&path, FIXTURE).unwrap();
    let _ = Adopted::new(&path, &saved(&path).unwrap(), &|id| kernel.wrap(id));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        FIXTURE.replace(" exact", "")
    );
    std::fs::write(&path, NEWER.replace(" exact", "")).unwrap();
    let unmarked = saved(&path).unwrap();
    assert!(unmarked.iter().all(|line| !line.exact));
    let _ = Adopted::new(&path, &unmarked, &|id| kernel.wrap(id));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        NEWER.replace(" exact", "")
    );
}

/// Asserts that a file that cannot be written again (here its path is a directory) leaves the ids
/// served as they were read.
#[test]
fn a_file_that_cannot_be_written_leaves_the_ids_served() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-unwritable");
    let path = dir.path().join(FILE);
    std::fs::create_dir_all(path.join("inside")).unwrap();
    let kernel = FakeKernel::new();
    let adopted = Adopted::new(&path, &fixture_lines(), &|id| kernel.wrap(id));
    assert_eq!(adopted.len(), 2);
    assert!(path.join("inside").is_dir());
}
