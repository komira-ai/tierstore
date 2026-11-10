//! The table of tree numbers.

use std::os::unix::fs::FileExt as _;

use tierstore_testkit::dir::TestDir;

use super::{FILE, LAYOUT, TableError, TreeTable};
use crate::tree_manifest::TreeDigest;

fn digest(seed: &[u8]) -> TreeDigest {
    TreeDigest::of(seed)
}

/// Asserts that numbers are given from 1 in the order of first sight, that a digest keeps its
/// number, and that each number reads back to its digest from the file.
#[test]
fn a_digest_gets_the_next_number_once_and_reads_back_by_number() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table");
    let table = TreeTable::create(dir.path()).unwrap();
    let (a, b) = (digest(b"a"), digest(b"b"));
    assert_eq!(table.number_of(&a).unwrap(), 1);
    assert_eq!(table.number_of(&b).unwrap(), 2);
    assert_eq!(table.number_of(&a).unwrap(), 1);
    assert_eq!(table.find(&b), Some(2));
    assert_eq!(table.find(&digest(b"c")), None);
    assert_eq!(table.digest_of(1).unwrap(), Some(a));
    assert_eq!(table.digest_of(2).unwrap(), Some(b));
    assert_eq!(table.digest_of(0).unwrap(), None);
    assert_eq!(table.digest_of(3).unwrap(), None);
}

/// Asserts the file's layout: the header names the layout, and record `n` follows at 16 + 32 (n - 1).
#[test]
fn the_file_has_a_header_and_fixed_size_records() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-file");
    let table = TreeTable::create(dir.path()).unwrap();
    let a = digest(b"a");
    let _ = table.number_of(&a).unwrap();
    let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
    assert_eq!(&bytes[..8], b"tsfstree");
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), LAYOUT);
    assert_eq!(bytes.len(), 16 + 32);
    assert_eq!(hex::encode(&bytes[16..48]), a.to_string());
}

/// Asserts that a table made again over an old file starts empty, and that a file that cannot be
/// made, written or read is an error that names the table.
#[test]
fn a_new_mount_starts_the_table_empty_and_failures_are_errors() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-new");
    let first = TreeTable::create(dir.path()).unwrap();
    let _ = first.number_of(&digest(b"a")).unwrap();
    let second = TreeTable::create(dir.path()).unwrap();
    assert_eq!(second.digest_of(1).unwrap(), None);
    assert_eq!(second.number_of(&digest(b"z")).unwrap(), 1);
    let missing = dir.path().join("none");
    let error = TreeTable::create(&missing).unwrap_err();
    assert!(
        error.to_string().starts_with("the table of tree numbers: "),
        "{error}"
    );
    // A table whose file is gone from under it cannot be read: the record is short.
    std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join(FILE))
        .unwrap()
        .set_len(16)
        .unwrap();
    assert!(matches!(second.digest_of(1), Err(TableError::Io(_))));
}

/// Asserts that a table with every number taken refuses the next, and a digest it has still reads.
#[test]
fn a_table_with_every_number_taken_is_full() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-full");
    let table = TreeTable::create(dir.path()).unwrap();
    // Take the last number without writing 2^39 records: a sparse file reads zeros up to it.
    table.exhaust();
    let error = table.number_of(&digest(b"late")).unwrap_err();
    assert!(matches!(error, TableError::Full));
    assert_eq!(error.to_string(), "the table of tree numbers is full");
    let file = std::fs::File::open(dir.path().join(FILE)).unwrap();
    assert_eq!(file.read_at(&mut [0u8; 1], 0).unwrap(), 1);
}

/// Asserts that a record that was written under a number is not read back as another digest when
/// the file holds bytes that are not a digest.
#[test]
fn a_digest_is_what_the_record_holds() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-rec");
    let table = TreeTable::create(dir.path()).unwrap();
    let a = digest(b"a");
    let _ = table.number_of(&a).unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join(FILE))
        .unwrap();
    file.write_all_at(&[0xab; 32], 16).unwrap();
    let got = table.digest_of(1).unwrap().unwrap();
    assert_eq!(got.to_string(), "ab".repeat(32));
}

/// Asserts that a start of the same epoch reopens the table: every number is kept, a digest keeps
/// its number, a new digest gets the next one, and what was written survives to a third start.
#[test]
fn the_table_of_tree_numbers_is_reopened_not_recreated_when_it_matches() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-reopen");
    let (first, reopened) = TreeTable::open_or_create(dir.path(), 7).unwrap();
    assert!(!reopened);
    let (a, b) = (digest(b"a"), digest(b"b"));
    assert_eq!(
        (first.number_of(&a).unwrap(), first.number_of(&b).unwrap()),
        (1, 2)
    );
    drop(first);
    let (second, reopened) = TreeTable::open_or_create(dir.path(), 7).unwrap();
    assert!(reopened);
    assert_eq!(second.epoch(), 7);
    assert_eq!(second.find(&b), Some(2));
    assert_eq!(second.digest_of(1).unwrap(), Some(a));
    assert_eq!(second.number_of(&b).unwrap(), 2);
    let c = digest(b"c");
    assert_eq!(second.number_of(&c).unwrap(), 3);
    drop(second);
    let (third, reopened) = TreeTable::open_or_create(dir.path(), 7).unwrap();
    assert!(reopened);
    assert_eq!(third.find(&c), Some(3));
}

/// Asserts that a table of another epoch (the mount of an earlier boot), a table with no epoch (a
/// FUSE mount's), and a file that is not a table of this layout are made afresh, with the new
/// epoch and no number, and that an epoch of 0 never reopens anything.
#[test]
fn a_table_of_another_epoch_or_of_none_is_made_afresh() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-afresh");
    let a = digest(b"a");
    let kept = |epoch: u32| {
        let (table, reopened) = TreeTable::open_or_create(dir.path(), epoch).unwrap();
        (table.find(&a), reopened)
    };
    let (table, _) = TreeTable::open_or_create(dir.path(), 5).unwrap();
    let _ = table.number_of(&a).unwrap();
    assert_eq!(kept(6), (None, false));
    let _ = TreeTable::open_or_create(dir.path(), 6)
        .unwrap()
        .0
        .number_of(&a)
        .unwrap();
    assert_eq!(kept(6), (Some(1), true));
    let fuse = TreeTable::create(dir.path()).unwrap();
    assert_eq!(fuse.epoch(), 0);
    let _ = fuse.number_of(&a).unwrap();
    assert_eq!(kept(6), (None, false));
    let _ = TreeTable::open_or_create(dir.path(), 6)
        .unwrap()
        .0
        .number_of(&a)
        .unwrap();
    assert_eq!(kept(0), (None, false));
    for damaged in [
        b"short".to_vec(),
        [b"notatree".as_slice(), &[0; 8]].concat(),
        [
            b"tsfstree".as_slice(),
            &9u32.to_le_bytes(),
            &6u32.to_le_bytes(),
        ]
        .concat(),
    ] {
        std::fs::write(dir.path().join(FILE), damaged).unwrap();
        assert_eq!(kept(6), (None, false));
        let _ = TreeTable::open_or_create(dir.path(), 6)
            .unwrap()
            .0
            .number_of(&a)
            .unwrap();
    }
}

/// Asserts that a record written in part (a start killed in the middle of a write) is no number:
/// the numbers before it are kept, and the next digest takes its place.
#[test]
fn a_record_written_in_part_is_not_a_number() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-part");
    let (table, _) = TreeTable::open_or_create(dir.path(), 3).unwrap();
    let (a, b) = (digest(b"a"), digest(b"b"));
    let _ = table.number_of(&a).unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join(FILE))
        .unwrap();
    file.write_all_at(&[1; 10], 16 + 32).unwrap();
    drop(table);
    let (table, reopened) = TreeTable::open_or_create(dir.path(), 3).unwrap();
    assert!(reopened);
    assert_eq!(table.find(&a), Some(1));
    assert_eq!(table.number_of(&b).unwrap(), 2);
    assert_eq!(table.digest_of(2).unwrap(), Some(b));
}

/// Asserts that a file that cannot be read as a table is an error, not an empty table: a
/// directory in its place.
#[test]
fn a_table_that_cannot_be_opened_is_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-unreadable");
    std::fs::create_dir(dir.path().join(FILE)).unwrap();
    assert!(matches!(
        TreeTable::open_or_create(dir.path(), 4),
        Err(TableError::Io(_))
    ));
}

/// The reader of the release before the epoch: it checks the magic and the layout, ignores the
/// reserved word, and reads record `n` at 16 + 32 (n - 1).
fn previous_reader(bytes: &[u8]) -> Vec<String> {
    assert_eq!(&bytes[..8], b"tsfstree");
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 1);
    bytes[16..].chunks_exact(32).map(hex::encode).collect()
}

/// Asserts (B8) that the release before this one reads the table this release writes: the same
/// magic and layout word, the records where it looks for them, and the epoch only in the word it
/// ignored.
#[test]
fn previous_release_reads_the_table_written_by_this_release() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-compat-new");
    let (table, _) = TreeTable::open_or_create(dir.path(), 0xdead_beef).unwrap();
    let (a, b) = (digest(b"a"), digest(b"b"));
    let _ = (table.number_of(&a).unwrap(), table.number_of(&b).unwrap());
    let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
    assert_eq!(&bytes[12..16], 0xdead_beef_u32.to_le_bytes());
    assert_eq!(previous_reader(&bytes), [a.to_string(), b.to_string()]);
}

/// Asserts (B8) that this release reads the table the release before it wrote (a checked-in file,
/// whose reserved word is 0): its numbers are read, new digests are numbered after them, and the
/// table is no mount of any epoch, so a start of an NFS mount makes its own.
#[test]
fn this_release_reads_the_table_written_by_the_previous_release() {
    let golden = include_bytes!("table_previous.golden");
    let dir = TestDir::under(std::env::temp_dir(), "tfs-table-compat-old");
    std::fs::write(dir.path().join(FILE), golden).unwrap();
    let table = TreeTable::load(dir.path()).unwrap().unwrap();
    assert_eq!(table.epoch(), 0);
    let (ab, cd) = (
        TreeDigest::from_hex(&"ab".repeat(32)).unwrap(),
        TreeDigest::from_hex(&"cd".repeat(32)).unwrap(),
    );
    assert_eq!((table.find(&ab), table.find(&cd)), (Some(1), Some(2)));
    assert_eq!(table.digest_of(2).unwrap(), Some(cd));
    assert_eq!(table.number_of(&digest(b"new")).unwrap(), 3);
    assert!(!TreeTable::open_or_create(dir.path(), 9).unwrap().1);
    assert!(
        TreeTable::load(dir.path().join("missing").as_path())
            .unwrap()
            .is_none()
    );
}
