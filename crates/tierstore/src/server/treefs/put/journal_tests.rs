//! The journal of the stage: its lines (golden, against the fixtures), the reading of a journal a
//! later release or a stopped process wrote, the rewrite and the appends.

use tierstore_testkit::dir::TestDir;

use super::journal::{FILE, Journal, NEW_FILE, Record, decode, encode};
use super::replay::apply;
use super::store::Entry;
use super::testing::sha;

/// The journal this release writes for a create, a create, a rename, a commit and a remove.
const FIXTURE: &str =
    include_str!("../../../../tests/stored_formats/fixtures/fs_put_journal.jsonl");

/// A journal with fields and an operation a later release adds, and a torn last line.
const NEWER: &str =
    include_str!("../../../../tests/stored_formats/fixtures/fs_put_journal_newer.jsonl");

const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

fn fixture_records() -> Vec<Record> {
    vec![
        Record::Create {
            id: 0,
            dir: "cas/b/1/n".to_owned(),
            name: "L0.part".to_owned(),
            ms: 1_800_000_000_000,
        },
        Record::Create {
            id: 1,
            dir: "cas/b/1/n".to_owned(),
            name: "pack.part".to_owned(),
            ms: 1_800_000_000_001,
        },
        Record::Rename {
            id: 1,
            name: format!("pack.sha256-{HELLO}.part"),
        },
        Record::Commit {
            id: 1,
            name: "pack".to_owned(),
            sha256: HELLO.to_owned(),
            size: 5,
            ms: 1_800_000_000_002,
        },
        Record::Remove { id: 0 },
    ]
}

/// Asserts that the fixture reads to its five records, with nothing skipped.
#[test]
fn the_fixture_reads_to_its_records() {
    assert_eq!(decode(FIXTURE.as_bytes()), (fixture_records(), 0));
}

/// Asserts that this release writes those records as exactly the bytes of the fixture.
#[test]
fn the_records_are_written_as_the_fixture() {
    let mut bytes = Vec::new();
    for record in fixture_records() {
        bytes.extend(encode(&record).unwrap());
    }
    assert_eq!(String::from_utf8(bytes).unwrap(), FIXTURE);
}

/// Asserts that a journal of a later release is read: its unknown fields are ignored, its unknown
/// operation and the torn last line are skipped and counted, and what it says is applied.
#[test]
fn a_newer_journal_is_read_and_its_unknown_lines_skipped() {
    let (records, skipped) = decode(NEWER.as_bytes());
    assert_eq!(skipped, 2);
    assert_eq!(
        records,
        vec![
            Record::Create {
                id: 4,
                dir: "cas/b/2/n".to_owned(),
                name: "L1.part".to_owned(),
                ms: 1_800_000_000_000,
            },
            Record::Rename {
                id: 4,
                name: format!("L1.sha256-{HELLO}.part"),
            },
            Record::Commit {
                id: 4,
                name: "L1".to_owned(),
                sha256: HELLO.to_owned(),
                size: 5,
                ms: 1_800_000_000_003,
            },
        ]
    );
    let (entries, unapplied) = apply(records);
    assert_eq!(unapplied, 0);
    let entry: &Entry = &entries[&4];
    assert_eq!(
        (
            entry.dir.as_str(),
            entry.name.as_str(),
            entry.size,
            entry.committed
        ),
        ("cas/b/2/n", "L1", 5, Some(sha(b"hello")))
    );
}

/// Asserts that the fixture applied leaves the committed `pack` alone: `L0` was removed.
#[test]
fn the_fixture_applied_leaves_the_committed_name() {
    let (entries, unapplied) = apply(fixture_records());
    assert_eq!(unapplied, 0);
    assert_eq!(entries.keys().copied().collect::<Vec<_>>(), vec![1]);
    assert_eq!(entries[&1].name, "pack");
    assert_eq!(entries[&1].committed, Some(sha(b"hello")));
    assert_eq!(entries[&1].ms, 1_800_000_000_002);
}

/// Asserts that a rewrite leaves exactly its records in `journal`, no `journal.new`, and a journal
/// that appends after them and counts its lines; and that a missing journal reads as empty.
#[test]
fn a_rewrite_replaces_the_journal_and_appends_go_after_it() {
    let dir = TestDir::under(std::env::temp_dir(), "put-journal-rewrite");
    assert!(Journal::read(dir.path()).unwrap().is_empty());
    std::fs::write(dir.path().join(FILE), "old\n").unwrap();
    let records = fixture_records();
    let mut journal = Journal::rewrite(dir.path(), &records[..2]).unwrap();
    assert_eq!(journal.lines(), 2);
    journal.append(&records[2]).unwrap();
    assert_eq!(journal.lines(), 3);
    let text = Journal::read(dir.path()).unwrap();
    assert_eq!(decode(&text), (records[..3].to_vec(), 0));
    assert!(!dir.path().join(NEW_FILE).exists());
}

/// Asserts that a journal that cannot be read (a directory in its place) and one that cannot be
/// rewritten (no stage directory) are errors.
#[test]
fn a_journal_that_cannot_be_read_or_written_is_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "put-journal-errors");
    std::fs::create_dir(dir.path().join(FILE)).unwrap();
    assert!(Journal::read(dir.path()).is_err());
    assert!(Journal::rewrite(&dir.path().join("gone"), &fixture_records()).is_err());
}
