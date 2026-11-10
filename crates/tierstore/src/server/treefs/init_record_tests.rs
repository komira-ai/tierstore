//! The record of the kernel's `INIT` handshake.

use std::time::Duration;

use fuser::{InitFlags, InitOutcome, Version};
use tierstore_testkit::dir::TestDir;

use super::{FILE, Record, RecordError, read, write};

/// The record this release writes, as the fixture holds it.
const FIXTURE: &str = include_str!("../../../tests/stored_formats/fixtures/fs_init_record.txt");

/// A record of a later release: other keys, a higher version, other values.
const NEWER: &str = include_str!("../../../tests/stored_formats/fixtures/fs_init_record_newer.txt");

/// The value the fixture is read to.
fn expected() -> Record {
    Record {
        outcome: InitOutcome {
            kernel_abi: Version(7, 40),
            capabilities: InitFlags::from_bits_retain(0x07ff_ffff),
            flags: InitFlags::from_bits_retain(0x0440_0300),
            max_readahead: 131_072,
            max_write: 1_048_576,
            time_gran: Duration::from_nanos(1),
            max_stack_depth: 1,
        },
        generation: 2,
        mount_id: "0123456789abcdef0123456789abcdef".to_owned(),
    }
}

/// Asserts that the fixture reads to its value and that the value is written as exactly the bytes
/// of the fixture.
#[test]
fn the_record_this_release_writes_is_read_and_written_byte_for_byte() {
    assert_eq!(Record::parse(FIXTURE).unwrap(), expected());
    assert_eq!(expected().text(), FIXTURE);
}

/// Asserts that a record of a later release (a higher version and keys this release does not
/// know) is read by the keys this release knows, and nothing is refused for the others.
#[test]
fn a_record_of_a_later_release_is_read_for_the_keys_this_release_knows() {
    let record = Record::parse(NEWER).unwrap();
    assert_eq!(record.generation, 5);
    assert_eq!(record.mount_id, "fedcba9876543210fedcba9876543210");
    assert_eq!(record.outcome.kernel_abi, Version(7, 43));
    assert_eq!(record.outcome.time_gran, Duration::from_secs(1));
    assert_eq!(record.outcome.max_stack_depth, 0);
    assert_eq!(
        record.outcome.capabilities,
        InitFlags::from_bits_retain(0xff_ffff_ffff)
    );
}

/// Asserts that the first record is generation 1 and the next process's is the next generation of
/// the same outcome.
#[test]
fn the_next_process_records_the_same_outcome_under_the_next_generation() {
    let first = Record::first(expected().outcome, "id".to_owned());
    assert_eq!(first.generation, 1);
    let next = first.next();
    assert_eq!(next.generation, 2);
    assert_eq!(next.outcome, first.outcome);
    assert_eq!(next.mount_id, "id");
}

/// Asserts, for each key the release needs, that a file without it is refused by its name, and
/// that a file that is not a record at all is refused as one.
#[test]
fn a_file_without_a_needed_key_or_not_a_record_is_refused_by_name() {
    assert!(matches!(
        Record::parse("something else 1\n"),
        Err(RecordError::NotARecord)
    ));
    assert!(matches!(Record::parse(""), Err(RecordError::NotARecord)));
    for key in [
        "generation",
        "mount_id",
        "kernel_abi",
        "capabilities",
        "flags",
        "max_readahead",
        "max_write",
        "time_gran_ns",
        "max_stack_depth",
    ] {
        let without: String = FIXTURE
            .lines()
            .filter(|line| !line.starts_with(key))
            .map(|line| format!("{line}\n"))
            .collect();
        match Record::parse(&without) {
            Err(RecordError::Missing(found)) => assert_eq!(found, key),
            other => panic!("without {key}: {other:?}"),
        }
    }
}

/// Asserts that a value that is not the kind of number its key holds is refused by the key's name.
#[test]
fn a_value_of_the_wrong_kind_is_refused_by_the_name_of_its_key() {
    let replaced = |key: &str, value: &str| -> String {
        FIXTURE
            .lines()
            .map(|line| {
                if line.starts_with(key) {
                    format!("{key} {value}\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect()
    };
    for (key, value) in [
        ("generation", "one"),
        ("generation", "99999999999"),
        ("kernel_abi", "740"),
        ("kernel_abi", "7.x"),
        ("capabilities", "123"),
        ("capabilities", "0xzz"),
        ("flags", "-1"),
        ("max_write", "big"),
        ("time_gran_ns", "1.5"),
    ] {
        match Record::parse(&replaced(key, value)) {
            Err(RecordError::Malformed(found)) => assert_eq!(found, key, "{value}"),
            other => panic!("{key} {value}: {other:?}"),
        }
    }
}

/// Asserts that a record written to a directory reads back, and that no half-written file is left
/// beside it.
#[test]
fn a_record_written_to_the_runtime_directory_reads_back() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-init-record");
    write(dir.path(), &expected()).unwrap();
    assert_eq!(read(dir.path()).unwrap(), expected());
    assert_eq!(
        std::fs::read_to_string(dir.path().join(FILE)).unwrap(),
        FIXTURE
    );
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [FILE]);
}

/// Asserts that a directory without the file, and a directory that cannot be written, are errors
/// that name the file.
#[test]
fn a_missing_file_and_an_unwritable_directory_are_errors_that_name_the_file() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-init-record-missing");
    let error = read(dir.path()).unwrap_err();
    assert!(matches!(error, RecordError::Io { .. }));
    assert!(error.to_string().contains("/init"), "{error}");
    let error = write(&dir.path().join("absent"), &expected()).unwrap_err();
    assert!(matches!(error, RecordError::Io { .. }));
}
