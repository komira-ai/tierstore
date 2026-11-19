//! The writer of the L2 tree list, the packs it cannot describe: `sidecar_from_v2` refuses, with the
//! exact `WriteError` and its full Display text, a pack the callback has no cells of
//! (`UnknownPack`), a pack of size 0 (`EmptyPack`), cells of another count than the size's
//! (`CellCount`), and a file or range past the end of its pack (`OutsidePack`, with a version 2
//! list patched to put a range past the pack and a file's offset plus size past `u64`); and a key
//! number past the keys (`V2Fault::KeyNumber`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{Tree, pack_bytes, small_tree};
use tierstore_list::{PackCells, PackInfo, V2Fault, WriteError, sidecar_from_v2};

const MIB: usize = 1 << 20;

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Where section `index` of the version 2 list `v2` (0 entries, 1 sha256, 2 key offsets, 3 keys,
/// 4 names, 5 ranges) begins, and its length in bytes, as its header says.
fn v2_section(v2: &[u8], index: usize) -> (usize, usize) {
    let offset = u32::from_le_bytes(v2[16 + 8 * index..20 + 8 * index].try_into().unwrap());
    let count = u32::from_le_bytes(v2[20 + 8 * index..24 + 8 * index].try_into().unwrap());
    let len = match index {
        0 | 1 => count as usize * 32,
        2 => (count as usize + 1) * 4,
        5 => count as usize * 24,
        _ => count as usize,
    };
    (offset as usize, len)
}

/// The index of the first entry record of `v2` that is a file of size above 0.
fn first_file_entry(v2: &[u8]) -> usize {
    let (at, len) = v2_section(v2, 0);
    (0..len / 32)
        .find(|index| {
            let record = &v2[at + index * 32..at + index * 32 + 32];
            let kind = u16::from_le_bytes([record[12], record[13]]) >> 12;
            let size = u64::from_le_bytes(record[24..32].try_into().unwrap());
            kind == 0 && size > 0
        })
        .expect("a file with bytes")
}

/// A tree of one pack of 100 bytes and one file of 50 bytes at its start.
fn one_file_tree() -> Tree {
    Tree::default()
        .pack("out/p", pack_bytes(5, 100))
        .file("f", "out/p", 0, 50)
}

#[test]
fn write_error_unknown_pack_names_the_key_the_callback_had_no_cells_for() {
    let tree = small_tree();
    let mut asked = Vec::new();
    let result = sidecar_from_v2(&tree.v2(), |key| {
        asked.push(key.to_owned());
        if key == "out/b/0" {
            None
        } else {
            tree.info(key)
        }
    });
    assert_eq!(result, Err(WriteError::UnknownPack("out/b/0".into())));
    assert_eq!(asked, ["out/a/0", "out/b/0"]);
    assert_eq!(
        result.unwrap_err().to_string(),
        "no cells of pack \"out/b/0\""
    );
}

#[test]
fn write_error_unknown_pack_first_key_stops_the_asking() {
    let tree = small_tree();
    let mut asked = 0;
    let result = sidecar_from_v2(&tree.v2(), |_| {
        asked += 1;
        None
    });
    assert_eq!(result, Err(WriteError::UnknownPack("out/a/0".into())));
    assert_eq!(asked, 1);
}

#[test]
fn write_error_empty_pack_is_a_pack_of_size_zero() {
    let tree = small_tree();
    let result = sidecar_from_v2(&tree.v2(), |key| {
        if key == "out/a/0" {
            Some(PackInfo::default())
        } else {
            tree.info(key)
        }
    });
    assert_eq!(result, Err(WriteError::EmptyPack("out/a/0".into())));
    assert_eq!(
        result.unwrap_err().to_string(),
        "pack \"out/a/0\" has size 0"
    );
}

#[test]
fn write_error_empty_pack_with_digests_given_is_still_empty() {
    let tree = one_file_tree();
    let result = sidecar_from_v2(&tree.v2(), |_| {
        Some(PackInfo {
            cells: PackCells {
                size: 0,
                digests: vec![[1; 32]],
            },
            ..PackInfo::default()
        })
    });
    assert_eq!(result, Err(WriteError::EmptyPack("out/p".into())));
}

#[test]
fn write_error_cell_count_too_few_digests() {
    let tree = small_tree();
    let result = sidecar_from_v2(&tree.v2(), |key| {
        tree.info(key).map(|mut info| {
            if key == "out/a/0" {
                info.cells.digests.pop();
            }
            info
        })
    });
    assert_eq!(
        result,
        Err(WriteError::CellCount {
            key: "out/a/0".into(),
            expected: 3,
            found: 2
        })
    );
    assert_eq!(
        result.unwrap_err().to_string(),
        "pack \"out/a/0\": 2 cell digests, its size has 3"
    );
}

#[test]
fn write_error_cell_count_too_many_digests() {
    let tree = small_tree();
    let result = sidecar_from_v2(&tree.v2(), |key| {
        tree.info(key).map(|mut info| {
            if key == "out/b/0" {
                info.cells.digests.push([9; 32]);
            }
            info
        })
    });
    assert_eq!(
        result,
        Err(WriteError::CellCount {
            key: "out/b/0".into(),
            expected: 1,
            found: 2
        })
    );
}

#[test]
fn write_error_cell_count_digests_of_a_larger_size_than_the_cells_given() {
    // A size of one cell and a byte has two cells; one digest is too few.
    let tree = one_file_tree();
    let result = sidecar_from_v2(&tree.v2(), |_| {
        Some(PackInfo {
            cells: PackCells {
                size: MIB as u64 + 1,
                digests: vec![[0; 32]],
            },
            ..PackInfo::default()
        })
    });
    assert_eq!(
        result,
        Err(WriteError::CellCount {
            key: "out/p".into(),
            expected: 2,
            found: 1
        })
    );
}

#[test]
fn write_error_outside_pack_file_past_the_end() {
    let tree = Tree::default()
        .pack("out/p", pack_bytes(1, 100))
        .file("f", "out/p", 60, 40);
    // The caller's cells are of the first 50 bytes only: the file's bytes 60..100 are past them.
    let result = sidecar_from_v2(&tree.v2(), |_| {
        Some(PackInfo {
            cells: PackCells::of(&pack_bytes(1, 50)),
            ..PackInfo::default()
        })
    });
    assert_eq!(result, Err(WriteError::OutsidePack("out/p".into())));
    assert_eq!(
        result.unwrap_err().to_string(),
        "pack \"out/p\": a file or range past its end"
    );
}

#[test]
fn write_error_outside_pack_file_ending_one_byte_past_the_end() {
    let tree = Tree::default()
        .pack("out/p", pack_bytes(1, 100))
        .file("f", "out/p", 0, 100);
    let result = sidecar_from_v2(&tree.v2(), |_| {
        Some(PackInfo {
            cells: PackCells::of(&pack_bytes(1, 99)),
            ..PackInfo::default()
        })
    });
    assert_eq!(result, Err(WriteError::OutsidePack("out/p".into())));
}

#[test]
fn write_error_outside_pack_file_ending_exactly_at_the_end_is_accepted() {
    let tree = Tree::default()
        .pack("out/p", pack_bytes(1, 100))
        .file("f", "out/p", 99, 1);
    assert!(sidecar_from_v2(&tree.v2(), |key| tree.info(key)).is_ok());
}

#[test]
fn write_error_outside_pack_range_past_the_end() {
    let tree = one_file_tree();
    let mut v2 = tree.v2();
    let (ranges, len) = v2_section(&v2, 5);
    assert_eq!(len, 24, "one range, of the file");
    // The range's length 50 becomes 101: past the pack of 100, the file's own bytes still fit.
    put_u64(&mut v2, ranges + 16, 101);
    let result = sidecar_from_v2(&v2, |key| tree.info(key));
    assert_eq!(result, Err(WriteError::OutsidePack("out/p".into())));
}

#[test]
fn write_error_outside_pack_range_starting_past_the_end() {
    let tree = one_file_tree();
    let mut v2 = tree.v2();
    let (ranges, _) = v2_section(&v2, 5);
    put_u64(&mut v2, ranges + 8, 100);
    put_u64(&mut v2, ranges + 16, 1);
    assert_eq!(
        sidecar_from_v2(&v2, |key| tree.info(key)),
        Err(WriteError::OutsidePack("out/p".into()))
    );
}

#[test]
fn write_error_outside_pack_range_ending_exactly_at_the_end_is_accepted() {
    let tree = one_file_tree();
    let mut v2 = tree.v2();
    let (ranges, _) = v2_section(&v2, 5);
    put_u64(&mut v2, ranges + 8, 50);
    put_u64(&mut v2, ranges + 16, 50);
    assert!(sidecar_from_v2(&v2, |key| tree.info(key)).is_ok());
}

#[test]
fn write_error_outside_pack_range_whose_end_overflows_u64() {
    let tree = one_file_tree();
    let mut v2 = tree.v2();
    let (ranges, _) = v2_section(&v2, 5);
    put_u64(&mut v2, ranges + 8, u64::MAX);
    put_u64(&mut v2, ranges + 16, 2);
    assert_eq!(
        sidecar_from_v2(&v2, |key| tree.info(key)),
        Err(WriteError::OutsidePack("out/p".into()))
    );
}

#[test]
fn write_error_outside_pack_file_whose_offset_plus_size_overflows_u64() {
    let tree = one_file_tree();
    let mut v2 = tree.v2();
    let (entries, _) = v2_section(&v2, 0);
    let file = first_file_entry(&v2);
    // The record's offset (bytes 16..24) becomes u64::MAX, its size stays 50.
    put_u64(&mut v2, entries + file * 32 + 16, u64::MAX);
    assert_eq!(
        sidecar_from_v2(&v2, |key| tree.info(key)),
        Err(WriteError::OutsidePack("out/p".into()))
    );
}

#[test]
fn write_error_outside_pack_names_the_key_of_the_pack_the_file_is_in() {
    let tree = small_tree();
    let result = sidecar_from_v2(&tree.v2(), |key| {
        tree.info(key).map(|mut info| {
            if key == "out/b/0" {
                // Cells of 99 bytes of the 100: `lib/b.txt` (60..100) ends past them.
                info.cells = PackCells::of(&tree.packs[key][..99]);
            }
            info
        })
    });
    assert_eq!(result, Err(WriteError::OutsidePack("out/b/0".into())));
}

#[test]
fn write_error_display_texts_in_full() {
    assert_eq!(
        WriteError::V2(V2Fault::NotV2).to_string(),
        "version 2 list: not a version 2 list"
    );
    assert_eq!(
        WriteError::UnknownPack("k".into()).to_string(),
        "no cells of pack \"k\""
    );
    assert_eq!(
        WriteError::EmptyPack("k".into()).to_string(),
        "pack \"k\" has size 0"
    );
    assert_eq!(
        WriteError::CellCount {
            key: "k".into(),
            expected: 3,
            found: 4
        }
        .to_string(),
        "pack \"k\": 4 cell digests, its size has 3"
    );
    assert_eq!(
        WriteError::OutsidePack("k".into()).to_string(),
        "pack \"k\": a file or range past its end"
    );
}

#[test]

fn v2_fault_display_texts_in_full() {
    assert_eq!(V2Fault::NotV2.to_string(), "not a version 2 list");
    assert_eq!(
        V2Fault::Version.to_string(),
        "not major 1, minor 0, no flags"
    );
    assert_eq!(
        V2Fault::Layout.to_string(),
        "sections not where the version 2 writer puts them"
    );
    assert_eq!(V2Fault::Counts.to_string(), "bad entry or digest count");
    assert_eq!(V2Fault::Keys.to_string(), "bad keys");
    assert_eq!(V2Fault::KeyNumber.to_string(), "a key number past the keys");
}

// ---------------------------------------------------------------------------------------------
// V2Fault::KeyNumber
// ---------------------------------------------------------------------------------------------

#[test]
fn v2_fault_key_number_file_entry_naming_the_key_after_the_last() {
    let tree = one_file_tree();
    let mut v2 = tree.v2();
    let (entries, _) = v2_section(&v2, 0);
    let file = first_file_entry(&v2);
    // One key, number 0; the file names key 1.
    put_u32(&mut v2, entries + file * 32 + 8, 1);
    assert_eq!(
        sidecar_from_v2(&v2, |key| tree.info(key)),
        Err(WriteError::V2(V2Fault::KeyNumber))
    );
}

#[test]
fn v2_fault_key_number_file_entry_naming_the_largest_key() {
    let tree = one_file_tree();
    let mut v2 = tree.v2();
    let (entries, _) = v2_section(&v2, 0);
    let file = first_file_entry(&v2);
    put_u32(&mut v2, entries + file * 32 + 8, u32::MAX);
    assert_eq!(
        sidecar_from_v2(&v2, |key| tree.info(key)),
        Err(WriteError::V2(V2Fault::KeyNumber))
    );
}

#[test]
fn v2_fault_key_number_range_naming_no_key() {
    let tree = one_file_tree();
    let mut v2 = tree.v2();
    let (ranges, _) = v2_section(&v2, 5);
    put_u32(&mut v2, ranges, 9);
    assert_eq!(
        sidecar_from_v2(&v2, |key| tree.info(key)),
        Err(WriteError::V2(V2Fault::KeyNumber))
    );
    assert_eq!(
        WriteError::V2(V2Fault::KeyNumber).to_string(),
        "version 2 list: a key number past the keys"
    );
}

#[test]
fn v2_fault_key_number_of_a_file_of_size_zero_is_not_checked() {
    // A file of size 0 names no pack: its key number field is not read.
    let tree = Tree::default()
        .pack("out/p", pack_bytes(5, 10))
        .empty_file("e")
        .file("f", "out/p", 0, 10);
    let mut v2 = tree.v2();
    let (entries, len) = v2_section(&v2, 0);
    for index in 0..len / 32 {
        let at = entries + index * 32;
        let kind = u16::from_le_bytes([v2[at + 12], v2[at + 13]]) >> 12;
        let size = u64::from_le_bytes(v2[at + 24..at + 32].try_into().unwrap());
        if kind == 0 && size == 0 {
            put_u32(&mut v2, at + 8, 77);
        }
    }
    assert!(sidecar_from_v2(&v2, |key| tree.info(key)).is_ok());
}
