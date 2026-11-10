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
    assert!(error.to_string().starts_with("the table of tree numbers: "), "{error}");
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
