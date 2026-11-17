//! The name and target tables the walk checks after the records, and hostile bytes: every
//! truncation and single-byte change of a list, and counts and offsets near the top of 32 and 64
//! bits.

use super::*;

// ----- the name and target tables -----

/// A list of one file at the root, named by name 0 of `names`, with the tables as given.
fn with_names(names: Vec<u8>) -> Vec<u8> {
    let mut s = spec(vec![Rec::empty_file(0, NONE)]);
    s.names = names;
    s.bytes()
}

fn with_targets(targets: Vec<u8>) -> Vec<u8> {
    let mut s = spec(vec![Rec::empty_file(0, NONE)]);
    s.targets = targets;
    s.bytes()
}

/// Asserts that a name that is empty, `.`, `..`, begins `.wh.`, has a NUL or a `/`, or is over 255
/// bytes is refused after the walk, and 255 bytes is taken.
#[test]
fn a_name_a_tree_may_not_have_is_refused() {
    let long = "x".repeat(MAX_NAME_BYTES + 1);
    let edge = "x".repeat(MAX_NAME_BYTES);
    for bad in ["", ".", "..", ".wh.x", "a\0b", "a/b", long.as_str()] {
        assert_table(
            &with_names(table(&[bad.as_bytes()])),
            Table::Names,
            0,
            StringFault::BadName,
        );
    }
    build(&with_names(table(&[edge.as_bytes()]))).unwrap();
    build(&with_names(table(&[".wha"]))).unwrap();
    // The fault is of the string that has it.
    assert_table(
        &with_names(table(&["-", "..", "b"])),
        Table::Names,
        1,
        StringFault::BadName,
    );
    assert_eq!(
        build_err(&with_names(table(&[".."]))).to_string(),
        "tree list name 0: bad name"
    );
}

/// Asserts that names equal to, or before, the name before them are refused.
#[test]
fn names_not_ascending_are_refused() {
    for names in [["b", "a"], ["a", "a"]] {
        assert_table(
            &with_names(table(&names)),
            Table::Names,
            1,
            StringFault::NotAscending,
        );
    }
    assert_eq!(
        build_err(&with_names(table(&["b", "a"]))).to_string(),
        "tree list name 1: not strictly ascending"
    );
    // Bytewise, not by character: "z" sorts before "é".
    build(&with_names(table(&["z", "é"]))).unwrap();
}

/// Asserts that a name that is not UTF-8 is refused, before the order of the names is judged.
#[test]
fn a_name_that_is_not_utf8_is_refused() {
    let bad = with_names(table(&[b"a".to_vec(), vec![0xff, 0xfe]]));
    assert_table(&bad, Table::Names, 1, StringFault::NotUtf8);
    assert_eq!(
        build_err(&with_names(table(&[[0xc0u8]]))).to_string(),
        "tree list name 0: not UTF-8"
    );
}

/// Asserts that a name whose end is past the data, or before its start, is refused as out of
/// bounds, the last end being the one the table fits.
#[test]
fn a_name_outside_its_table_is_refused() {
    assert_table(
        &with_names(table_raw(&[100, 3], b"abc")),
        Table::Names,
        0,
        StringFault::Bounds,
    );
    assert_table(
        &with_names(table_raw(&[2, 1, 3], b"abc")),
        Table::Names,
        1,
        StringFault::Bounds,
    );
}

/// Asserts that a target that is empty, over 4096 bytes or has a NUL is refused after the walk,
/// and 4096 bytes and a target with a `/` are taken.
#[test]
fn a_target_a_link_may_not_have_is_refused() {
    let long = "t".repeat(MAX_TARGET_BYTES + 1);
    let edge = "t".repeat(MAX_TARGET_BYTES);
    for bad in ["", "a\0b", long.as_str()] {
        assert_table(
            &with_targets(table(&[bad.as_bytes()])),
            Table::Targets,
            0,
            StringFault::BadTarget,
        );
    }
    build(&with_targets(table(&[edge.as_bytes()]))).unwrap();
    build(&with_targets(table(&["../a/b"]))).unwrap();
    assert_table(
        &with_targets(table(&["ok", ""])),
        Table::Targets,
        1,
        StringFault::NotAscending,
    );
    assert_eq!(
        build_err(&with_targets(table(&["a", "b\0"]))).to_string(),
        "tree list target 1: bad target"
    );
}

/// Asserts that the target table is judged for order, UTF-8 and bounds as the name table is.
#[test]
fn the_target_table_is_judged_for_order_encoding_and_bounds() {
    assert_table(
        &with_targets(table(&["b", "a"])),
        Table::Targets,
        1,
        StringFault::NotAscending,
    );
    assert_table(
        &with_targets(table(&[[0xffu8]])),
        Table::Targets,
        0,
        StringFault::NotUtf8,
    );
    assert_table(
        &with_targets(table_raw(&[9, 2], b"ab")),
        Table::Targets,
        0,
        StringFault::Bounds,
    );
    assert_eq!(
        build_err(&with_targets(table_raw(&[9, 2], b"ab"))).to_string(),
        "tree list target 0: past the end of its table"
    );
}

/// Asserts that the names are judged before the targets, and a record fault before both.
#[test]
fn faults_are_found_in_the_order_records_names_targets() {
    let mut s = spec(vec![Rec::empty_file(0, NONE)]);
    s.names = table(&[".."]);
    s.targets = table(&[""]);
    assert_table(&s.bytes(), Table::Names, 0, StringFault::BadName);
    s.records[0].mode = 0xffff;
    assert_fault(&s.bytes(), 0, RecordFault::BadMode(0xffff));
}

/// Asserts that a name past the walk's records, never used by a record, is still judged.
#[test]
fn a_name_no_record_uses_is_judged() {
    let mut s = spec(vec![Rec::empty_file(0, NONE)]);
    s.names = table(&["-", ".wh.b"]);
    assert_table(&s.bytes(), Table::Names, 1, StringFault::BadName);
}

// ----- hostile bytes -----

/// Asserts that every truncation of a list, to each length from 0, is refused or built, and
/// never panics.
#[test]
fn every_truncation_of_a_list_is_refused_or_built() {
    let bytes = sample();
    let mut refused = 0;
    for len in 0..bytes.len() {
        if build(&bytes[..len]).is_err() {
            refused += 1;
        }
    }
    // Every strict prefix lacks part of the records, so none is a list.
    assert_eq!(refused, bytes.len());
    build(&bytes).unwrap();
}

/// Asserts that a single changed byte, at every offset, to 0x00, 0xff, the byte with its low bit
/// flipped and the byte with its high bit flipped, is refused or built, and never panics.
#[test]
fn every_single_byte_change_is_refused_or_built() {
    let bytes = sample();
    let mut built = 0;
    let mut refused = 0;
    for at in 0..bytes.len() {
        for to in [0x00, 0xff, bytes[at] ^ 0x01, bytes[at] ^ 0x80] {
            let mut changed = bytes.clone();
            changed[at] = to;
            match build(&changed) {
                Ok(_) => built += 1,
                Err(_) => refused += 1,
            }
        }
    }
    assert!(refused > 0);
    assert!(built > 0);
}

/// Asserts that the same changes to a list of a different shape (one with many directories and
/// links) are refused or built.
#[test]
fn every_single_byte_change_of_a_deep_list_is_refused_or_built() {
    let list = TreeList {
        entries: vec![
            link("a/b/c/l", "x"),
            file("a/b/c/f", 2, Some(("k", 0)), Some(sha(1))),
            dir("e/f/g", 0o700),
        ],
    };
    let bytes = list.encode_v2().unwrap();
    for at in 0..bytes.len() {
        for to in [0x00, 0xff, bytes[at] ^ 0x01, bytes[at] ^ 0x80] {
            let mut changed = bytes.clone();
            changed[at] = to;
            let _ = build(&changed);
        }
    }
}

/// Asserts that a count, a record length and a section offset near the top of 32 bits are refused
/// without a panic or a large allocation.
#[test]
fn counts_and_offsets_near_the_top_of_32_bits_are_refused() {
    let bytes = sample();
    let records = TreeListV2Error::Header(HeaderFault::Section(Section::Records));
    for count in [u32::MAX, u32::MAX - 1, 1 << 31] {
        assert_eq!(
            build_err(&patched(&bytes, 20, &count.to_le_bytes())),
            records
        );
    }
    for len in [u32::MAX, u32::MAX - 1, 1 << 31] {
        assert_eq!(build_err(&patched(&bytes, 16, &len.to_le_bytes())), records);
    }
    for at in [u32::MAX, u32::MAX - 1, u32::MAX - 63] {
        assert_eq!(build_err(&patched(&bytes, 36, &at.to_le_bytes())), records);
    }
    for (field, table) in [(24, Table::Keys), (28, Table::Names), (32, Table::Targets)] {
        for at in [u32::MAX, u32::MAX - 3] {
            assert_eq!(
                build_err(&patched(&bytes, field, &at.to_le_bytes())),
                TreeListV2Error::Header(HeaderFault::Section(Section::Table(table)))
            );
        }
        // The count at the start of the table.
        let start = u32_at(&bytes, field) as usize;
        for count in [u32::MAX, 0x4000_0000, 1 << 31] {
            assert_eq!(
                build_err(&patched(&bytes, start, &count.to_le_bytes())),
                TreeListV2Error::Header(HeaderFault::Section(Section::Table(table)))
            );
        }
    }
}

/// Asserts that a last end of a table near the top of 32 bits is refused, and an end past the
/// data in an earlier place is refused as out of bounds by the walk.
#[test]
fn ends_near_the_top_of_32_bits_are_refused() {
    let bytes = sample();
    let names = u32_at(&bytes, 28) as usize;
    let count = u32_at(&bytes, names) as usize;
    let last = names + 4 + 4 * (count - 1);
    assert_eq!(
        build_err(&patched(&bytes, last, &u32::MAX.to_le_bytes())),
        TreeListV2Error::Header(HeaderFault::Section(Section::Table(Table::Names)))
    );
    let bad_end = patched(&bytes, names + 4, &u32::MAX.to_le_bytes());
    assert_table(&bad_end, Table::Names, 0, StringFault::Bounds);
}

/// Asserts that offsets and sizes near the top of 64 bits in records are refused or built, never
/// a panic.
#[test]
fn offsets_and_sizes_near_the_top_of_64_bits_do_not_panic() {
    for field in [F_SIZE, F_OFFSET] {
        for value in [u64::MAX, u64::MAX - 1, 1 << 63] {
            for record in 5..9 {
                let _ = build(&patched_record(record, field, &value.to_le_bytes()));
            }
        }
    }
}
