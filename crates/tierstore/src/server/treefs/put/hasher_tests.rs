//! The inline hash and the hash of a whole file.

use std::io::Write as _;

use tierstore_testkit::dir::TestDir;

use super::hasher::{InlineHash, hash_file};
use super::testing::sha;

/// Asserts that writes made one after the other from 0 give the SHA-256 of all their bytes, and
/// that a write of nothing anywhere changes nothing.
#[test]
fn sequential_writes_give_the_digest_of_the_whole() {
    let mut hash = InlineHash::new();
    assert_eq!(hash.digest(0), Some(sha(b"")));
    hash.wrote(0, b"hello ");
    hash.wrote(99, b"");
    hash.wrote(6, b"world");
    assert_eq!(hash.digest(11), Some(sha(b"hello world")));
}

/// Asserts that a write before the end (an overwrite), past it (a hole) or a digest asked for a size
/// other than the bytes hashed gives no digest, and that the digest stays unknown after.
#[test]
fn any_other_write_makes_the_digest_unknown_for_good() {
    let mut hash = InlineHash::new();
    hash.wrote(0, b"abc");
    assert_eq!(hash.digest(4), None);
    hash.wrote(1, b"x");
    assert_eq!(hash.digest(3), None);
    hash.wrote(3, b"d");
    assert_eq!(hash.digest(4), None);

    let mut hole = InlineHash::new();
    hole.wrote(5, b"x");
    assert_eq!(hole.digest(6), None);

    assert_eq!(InlineHash::unknown().digest(0), None);
}

/// Asserts that the hash of a file read in chunks is the SHA-256 and the length of all of its
/// bytes, for an empty file and for one longer than two chunks.
#[test]
fn a_file_is_hashed_whole_in_chunks() {
    let dir = TestDir::under(std::env::temp_dir(), "put-hash-file");
    let path = dir.path().join("f");
    std::fs::write(&path, b"").unwrap();
    let empty = std::fs::File::open(&path).unwrap();
    assert_eq!(hash_file(&empty).unwrap(), (sha(b""), 0));

    let bytes: Vec<u8> = (0..(2 << 20) + 7).map(|i| (i % 251) as u8).collect();
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(&bytes).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    assert_eq!(hash_file(&file).unwrap(), (sha(&bytes), bytes.len() as u64));
}

/// Asserts that a file that cannot be read is said, not hashed.
#[test]
fn a_file_that_cannot_be_read_is_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "put-hash-unreadable");
    let path = dir.path().join("f");
    std::fs::write(&path, b"x").unwrap();
    let write_only = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    assert!(hash_file(&write_only).is_err());
}
