//! Every way `TreeListL2::open` refuses a PACKS record (key, size, cell shift, cell count, first
//! cell), the keys' length and the cells' total, a range, an extension record, and the order in
//! which these are judged; and the full `Display` text of every error of the crate. Each test
//! patches the bytes of a valid list (the header's CRC set again) and asserts the whole error
//! value. The refusals of the magic, the header and the tree id are in `refuse.rs`, those of the
//! entries in `refuse_entries.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{recrc, section, small_tree};
use tierstore_list::{
    EntryFault, ExtensionFault, HeaderFault, Malformed, OpenError, PackFault, Section, TreeListL2,
};

const SECTIONS: [Section; 8] = [
    Section::Entries,
    Section::Digests,
    Section::Packs,
    Section::Cells,
    Section::Keys,
    Section::Names,
    Section::Ranges,
    Section::Extensions,
];

fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// `put_uNN` with the offset computed first: the offset is often read from the bytes being edited.
macro_rules! p16 {
    ($bytes:expr, $at:expr, $value:expr) => {{
        let at = $at;
        put_u16($bytes, at, $value)
    }};
}
macro_rules! p32 {
    ($bytes:expr, $at:expr, $value:expr) => {{
        let at = $at;
        put_u32($bytes, at, $value)
    }};
}
macro_rules! p64 {
    ($bytes:expr, $at:expr, $value:expr) => {{
        let at = $at;
        put_u64($bytes, at, $value)
    }};
}

/// The L2 list of `small_tree()`, edited by `edit`, its header CRC set again.
fn edited(edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let (_, mut l2) = small_tree().lists();
    edit(&mut l2);
    recrc(&mut l2);
    l2
}

fn set_count(l2: &mut [u8], index: usize, count: u64) {
    p64!(l2, 64 + 16 * index, count);
}

fn open_err(bytes: Vec<u8>) -> OpenError {
    TreeListL2::open(bytes).unwrap_err()
}

fn header_fault(fault: HeaderFault) -> OpenError {
    OpenError::Malformed(Malformed::Header(fault))
}

fn pack_fault(index: u32, fault: PackFault) -> OpenError {
    OpenError::Malformed(Malformed::Pack { index, fault })
}

/// The offset of PACKS record `index`.
fn pack_at(l2: &[u8], index: usize) -> usize {
    section(l2, 2).0 + 48 * index
}

/// A list whose extension section is `payload`.
fn with_extensions(payload: &[u8]) -> Vec<u8> {
    edited(|l2| {
        let (offset, _) = section(l2, 7);
        l2.truncate(offset);
        l2.extend_from_slice(payload);
        set_count(l2, 7, payload.len() as u64);
    })
}

/// One extension record: its tag, its length field, and `payload` padded to a multiple of 8.
fn record(tag: u32, length: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = tag.to_le_bytes().to_vec();
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(payload);
    while !out.len().is_multiple_of(8) {
        out.push(0);
    }
    out
}

// ---- PACKS, the keys and the cells -------------------------------------------------------------

#[test]
fn a_key_that_does_not_begin_where_the_one_before_ends_is_key_start() {
    let l2 = edited(|l2| p32!(l2, pack_at(l2, 1), 0));
    assert_eq!(open_err(l2), pack_fault(1, PackFault::KeyStart));
    let l2 = edited(|l2| p32!(l2, pack_at(l2, 0), 1));
    assert_eq!(open_err(l2), pack_fault(0, PackFault::KeyStart));
    let l2 = edited(|l2| p32!(l2, pack_at(l2, 1), 8));
    assert_eq!(open_err(l2), pack_fault(1, PackFault::KeyStart));
}

#[test]
fn a_key_of_0_or_1025_bytes_is_key_length_with_the_length() {
    for length in [0u32, 1025, 4096, u32::MAX] {
        let l2 = edited(|l2| p32!(l2, pack_at(l2, 0) + 4, length));
        assert_eq!(
            open_err(l2),
            pack_fault(0, PackFault::KeyLength(length)),
            "length {length}"
        );
    }
}

#[test]
fn a_key_of_1024_bytes_passes_the_length_check_and_fails_the_bounds() {
    let l2 = edited(|l2| p32!(l2, pack_at(l2, 0) + 4, 1024));
    assert_eq!(open_err(l2), pack_fault(0, PackFault::KeyBounds));
}

#[test]
fn a_key_past_the_end_of_the_keys_is_key_bounds() {
    // The last key one byte longer than the keys.
    let l2 = edited(|l2| p32!(l2, pack_at(l2, 1) + 4, 8));
    assert_eq!(open_err(l2), pack_fault(1, PackFault::KeyBounds));
}

#[test]
fn a_key_that_is_not_utf8_is_key_not_utf8() {
    let l2 = edited(|l2| {
        let at = section(l2, 4).0;
        l2[at] = 0xff;
    });
    assert_eq!(open_err(l2), pack_fault(0, PackFault::KeyNotUtf8));
    let l2 = edited(|l2| {
        let at = section(l2, 4).0 + 7;
        l2[at..at + 2].copy_from_slice(&[0xc3, 0x28]);
    });
    assert_eq!(open_err(l2), pack_fault(1, PackFault::KeyNotUtf8));
}

#[test]
fn keys_that_are_equal_are_key_not_ascending() {
    let l2 = edited(|l2| {
        let at = section(l2, 4).0;
        let first = l2[at..at + 7].to_vec();
        l2[at + 7..at + 14].copy_from_slice(&first);
    });
    assert_eq!(open_err(l2), pack_fault(1, PackFault::KeyNotAscending));
}

#[test]
fn keys_that_descend_are_key_not_ascending() {
    let l2 = edited(|l2| {
        let at = section(l2, 4).0;
        l2[at + 4] = b'c';
        assert_eq!(&l2[at..at + 14], b"out/c/0out/b/0");
    });
    assert_eq!(open_err(l2), pack_fault(1, PackFault::KeyNotAscending));
}

#[test]
fn a_pack_of_size_0_is_empty() {
    let l2 = edited(|l2| p64!(l2, pack_at(l2, 0) + 8, 0));
    assert_eq!(open_err(l2), pack_fault(0, PackFault::Empty));
}

#[test]
fn a_pack_cell_shift_other_than_20_is_refused_with_the_shift() {
    for shift in [0u8, 19, 21] {
        let l2 = edited(|l2| {
            let at = pack_at(l2, 1) + 28;
            l2[at] = shift;
        });
        assert_eq!(
            open_err(l2),
            pack_fault(1, PackFault::CellShift(shift)),
            "shift {shift}"
        );
    }
}

#[test]
fn a_cell_count_that_is_not_that_of_the_size_is_refused_with_both() {
    // Pack 0 is 2.5 MiB: three cells.
    let l2 = edited(|l2| p32!(l2, pack_at(l2, 0) + 24, 4));
    assert_eq!(
        open_err(l2),
        pack_fault(
            0,
            PackFault::CellCount {
                expected: 3,
                found: 4
            }
        )
    );
    let l2 = edited(|l2| p32!(l2, pack_at(l2, 0) + 24, 2));
    assert_eq!(
        open_err(l2),
        pack_fault(
            0,
            PackFault::CellCount {
                expected: 3,
                found: 2
            }
        )
    );
}

#[test]
fn a_size_whose_cells_are_not_the_cell_count_is_refused_at_the_cell_boundary() {
    // 2 MiB is two cells, 2 MiB + 1 is three: a count of 3 with the first size is wrong.
    let l2 = edited(|l2| p64!(l2, pack_at(l2, 0) + 8, 2 << 20));
    assert_eq!(
        open_err(l2),
        pack_fault(
            0,
            PackFault::CellCount {
                expected: 2,
                found: 3
            }
        )
    );
    let l2 = edited(|l2| p64!(l2, pack_at(l2, 0) + 8, 1));
    assert_eq!(
        open_err(l2),
        pack_fault(
            0,
            PackFault::CellCount {
                expected: 1,
                found: 3
            }
        )
    );
}

#[test]
fn a_size_so_large_its_cells_do_not_fit_a_count_is_a_cell_count_fault() {
    let l2 = edited(|l2| p64!(l2, pack_at(l2, 0) + 8, u64::MAX));
    assert_eq!(
        open_err(l2),
        pack_fault(
            0,
            PackFault::CellCount {
                expected: u64::MAX.div_ceil(1 << 20),
                found: 3
            }
        )
    );
}

#[test]
fn a_first_cell_that_is_not_where_the_packs_before_end_is_cell_first() {
    let l2 = edited(|l2| p64!(l2, pack_at(l2, 1) + 16, 0));
    assert_eq!(
        open_err(l2),
        pack_fault(
            1,
            PackFault::CellFirst {
                expected: 3,
                found: 0
            }
        )
    );
    let l2 = edited(|l2| p64!(l2, pack_at(l2, 0) + 16, 1));
    assert_eq!(
        open_err(l2),
        pack_fault(
            0,
            PackFault::CellFirst {
                expected: 0,
                found: 1
            }
        )
    );
}

#[test]
fn keys_that_run_on_past_the_last_pack_are_keys_length() {
    let l2 = edited(|l2| {
        let (_, count) = section(l2, 4);
        set_count(l2, 4, count + 1);
    });
    assert_eq!(open_err(l2), OpenError::Malformed(Malformed::KeysLength));
}

#[test]
fn cells_that_are_not_the_packs_cells_together_are_cell_total() {
    // Pack 0 has 3 cells and pack 1 has 1.
    let fewer = edited(|l2| set_count(l2, 3, 3));
    assert_eq!(
        open_err(fewer),
        OpenError::Malformed(Malformed::CellTotal {
            expected: 4,
            found: 3
        })
    );
    let more = edited(|l2| set_count(l2, 3, 0));
    assert_eq!(
        open_err(more),
        OpenError::Malformed(Malformed::CellTotal {
            expected: 4,
            found: 0
        })
    );
}

#[test]
fn a_list_with_no_packs_and_cells_is_a_cell_total_fault() {
    let l2 = edited(|l2| set_count(l2, 2, 0));
    // Without packs the keys are no one's: the keys' length is the first fault.
    assert_eq!(open_err(l2), OpenError::Malformed(Malformed::KeysLength));
    let l2 = edited(|l2| {
        set_count(l2, 2, 0);
        set_count(l2, 4, 0);
    });
    assert_eq!(
        open_err(l2),
        OpenError::Malformed(Malformed::CellTotal {
            expected: 0,
            found: 4
        })
    );
}

// ---- ranges ------------------------------------------------------------------------------------

/// The offset of range record `index`.
fn range_at(l2: &[u8], index: usize) -> usize {
    section(l2, 6).0 + 24 * index
}

#[test]
fn a_range_naming_no_pack_is_range() {
    for pack in [2u32, 3, u32::MAX] {
        let l2 = edited(|l2| p32!(l2, range_at(l2, 0), pack));
        assert_eq!(
            open_err(l2),
            OpenError::Malformed(Malformed::Range(0)),
            "pack {pack}"
        );
    }
}

#[test]
fn an_empty_range_is_range() {
    let l2 = edited(|l2| p64!(l2, range_at(l2, 0) + 16, 0));
    assert_eq!(open_err(l2), OpenError::Malformed(Malformed::Range(0)));
}

#[test]
fn a_range_past_the_end_of_its_pack_is_range() {
    // Pack 0 is 2.5 MiB.
    let size = 2 * (1u64 << 20) + (1 << 19);
    let l2 = edited(|l2| {
        p32!(l2, range_at(l2, 0), 0);
        p64!(l2, range_at(l2, 0) + 8, 0);
        p64!(l2, range_at(l2, 0) + 16, size + 1);
    });
    assert_eq!(open_err(l2), OpenError::Malformed(Malformed::Range(0)));
    let l2 = edited(|l2| {
        p32!(l2, range_at(l2, 0), 0);
        p64!(l2, range_at(l2, 0) + 8, size);
        p64!(l2, range_at(l2, 0) + 16, 1);
    });
    assert_eq!(open_err(l2), OpenError::Malformed(Malformed::Range(0)));
}

#[test]
fn a_range_ending_exactly_at_the_end_of_its_pack_is_not_refused() {
    let size = 2 * (1u64 << 20) + (1 << 19);
    let l2 = edited(|l2| {
        p32!(l2, range_at(l2, 0), 0);
        p64!(l2, range_at(l2, 0) + 8, size - 1);
        p64!(l2, range_at(l2, 0) + 16, 1);
    });
    assert!(TreeListL2::open(l2).is_ok());
}

#[test]
fn a_range_whose_offset_plus_length_overflows_is_range() {
    let l2 = edited(|l2| {
        p32!(l2, range_at(l2, 0), 0);
        p64!(l2, range_at(l2, 0) + 8, u64::MAX);
        p64!(l2, range_at(l2, 0) + 16, 2);
    });
    assert_eq!(open_err(l2), OpenError::Malformed(Malformed::Range(0)));
}

#[test]
fn a_bad_range_is_reported_with_its_number() {
    let (_, l2) = small_tree().lists();
    let (_, count) = section(&l2, 6);
    assert!(count >= 2);
    let last = count - 1;
    let l2 = edited(|l2| p64!(l2, range_at(l2, last as usize) + 16, 0));
    assert_eq!(
        open_err(l2),
        OpenError::Malformed(Malformed::Range(last as u32))
    );
}

// ---- extension records -------------------------------------------------------------------------

#[test]
fn an_extension_record_longer_than_the_section_is_truncated() {
    let l2 = with_extensions(&record(1, 100, &[]));
    assert_eq!(
        open_err(l2),
        OpenError::Malformed(Malformed::Extension {
            at: 0,
            fault: ExtensionFault::Truncated
        })
    );
    let l2 = with_extensions(&record(1, u32::MAX, &[]));
    assert_eq!(
        open_err(l2),
        OpenError::Malformed(Malformed::Extension {
            at: 0,
            fault: ExtensionFault::Truncated
        })
    );
}

#[test]
fn an_extension_record_whose_padding_runs_past_the_section_is_truncated() {
    // 9 bytes of payload pad to 16: the record is 24 bytes, the section 16.
    let l2 = with_extensions(&record(1, 9, &[1; 8]));
    assert_eq!(
        open_err(l2),
        OpenError::Malformed(Malformed::Extension {
            at: 0,
            fault: ExtensionFault::Truncated
        })
    );
}

#[test]
fn a_second_extension_record_that_is_cut_short_is_truncated_at_its_offset() {
    let mut payload = record(1, 3, &[1, 2, 3]);
    assert_eq!(payload.len(), 16);
    payload.extend_from_slice(&record(2, 9, &[]));
    let l2 = with_extensions(&payload);
    assert_eq!(
        open_err(l2),
        OpenError::Malformed(Malformed::Extension {
            at: 16,
            fault: ExtensionFault::Truncated
        })
    );
}

#[test]
fn extension_records_that_fit_exactly_and_are_not_required_are_skipped() {
    let mut payload = record(1, 8, &[9; 8]);
    payload.extend_from_slice(&record(0x7fff_ffff, 3, &[1, 2, 3]));
    payload.extend_from_slice(&record(5, 0, &[]));
    assert!(TreeListL2::open(with_extensions(&payload)).is_ok());
}

#[test]
fn a_required_extension_tag_is_refused_with_the_tag() {
    for tag in [0x8000_0000u32, 0x8000_0001, 0xffff_ffff] {
        let l2 = with_extensions(&record(tag, 0, &[]));
        assert_eq!(
            open_err(l2),
            OpenError::Malformed(Malformed::Extension {
                at: 0,
                fault: ExtensionFault::Required(tag)
            }),
            "tag {tag:#x}"
        );
    }
}

#[test]
fn a_required_tag_after_skipped_records_is_refused_at_its_offset() {
    let mut payload = record(1, 3, &[1, 2, 3]);
    payload.extend_from_slice(&record(2, 0, &[]));
    payload.extend_from_slice(&record(0x8000_0007, 4, &[1, 2, 3, 4]));
    assert_eq!(
        open_err(with_extensions(&payload)),
        OpenError::Malformed(Malformed::Extension {
            at: 24,
            fault: ExtensionFault::Required(0x8000_0007)
        })
    );
}

#[test]
fn a_required_record_that_is_also_truncated_is_truncated() {
    let l2 = with_extensions(&record(0x8000_0001, 100, &[]));
    assert_eq!(
        open_err(l2),
        OpenError::Malformed(Malformed::Extension {
            at: 0,
            fault: ExtensionFault::Truncated
        })
    );
}

// ---- the order of the checks -------------------------------------------------------------------

#[test]
fn packs_are_judged_before_ranges_ranges_before_extensions_extensions_before_entries() {
    let entry_broken = |l2: &mut Vec<u8>| {
        let at = section(l2, 0).0;
        p16!(l2, at + 12, 0);
    };
    // Entry broken only.
    let l2 = edited(entry_broken);
    assert_eq!(
        open_err(l2),
        OpenError::Malformed(Malformed::Entry {
            index: 0,
            fault: EntryFault::Root
        })
    );
    // Plus an extension fault: the extension is reported.
    let l2 = edited(|l2| {
        entry_broken(l2);
        let (offset, _) = section(l2, 7);
        l2.truncate(offset);
        l2.extend_from_slice(&record(0x8000_0000, 0, &[]));
        set_count(l2, 7, 8);
    });
    assert_eq!(
        open_err(l2),
        OpenError::Malformed(Malformed::Extension {
            at: 0,
            fault: ExtensionFault::Required(0x8000_0000)
        })
    );
    // Plus a range fault: the range is reported.
    let l2 = edited(|l2| {
        entry_broken(l2);
        let (offset, _) = section(l2, 7);
        l2.truncate(offset);
        l2.extend_from_slice(&record(0x8000_0000, 0, &[]));
        set_count(l2, 7, 8);
        p64!(l2, range_at(l2, 0) + 16, 0);
    });
    assert_eq!(open_err(l2), OpenError::Malformed(Malformed::Range(0)));
    // Plus a pack fault: the pack is reported.
    let l2 = edited(|l2| {
        entry_broken(l2);
        p64!(l2, range_at(l2, 0) + 16, 0);
        p64!(l2, pack_at(l2, 0) + 8, 0);
    });
    assert_eq!(open_err(l2), pack_fault(0, PackFault::Empty));
}

#[test]
fn a_header_fault_is_reported_before_any_pack_fault() {
    let l2 = edited(|l2| {
        l2[21] = 19;
        p64!(l2, pack_at(l2, 0) + 8, 0);
    });
    assert_eq!(open_err(l2), header_fault(HeaderFault::CellShift(19)));
}

// ---- Display text ------------------------------------------------------------------------------

fn text(malformed: Malformed) -> String {
    OpenError::Malformed(malformed).to_string()
}

#[test]
fn display_of_the_header_faults() {
    let header = |fault| text(Malformed::Header(fault));
    assert_eq!(text(Malformed::BadMagic), "tree list L2: bad magic");
    assert_eq!(
        header(HeaderFault::Truncated),
        "tree list L2: header: truncated"
    );
    assert_eq!(
        header(HeaderFault::Length(184)),
        "tree list L2: header: bad header length 184"
    );
    assert_eq!(
        header(HeaderFault::Crc {
            stored: 0x1234,
            computed: 0xabcdef
        }),
        "tree list L2: header: crc32c 0x00abcdef, the header says 0x00001234"
    );
    assert_eq!(
        header(HeaderFault::RequiredFlags(5)),
        "tree list L2: header: unknown required flags 0x5"
    );
    assert_eq!(
        header(HeaderFault::CellShift(19)),
        "tree list L2: header: default cell shift 19, not 20"
    );
    assert_eq!(
        header(HeaderFault::NoEntries),
        "tree list L2: header: no entries"
    );
    assert_eq!(
        header(HeaderFault::DigestCount(1)),
        "tree list L2: header: 1 digests for another count of entries"
    );
    assert_eq!(
        header(HeaderFault::ExtensionLength(4)),
        "tree list L2: header: an extension section of 4 bytes, not a multiple of 8"
    );
}

#[test]
fn display_of_the_section_faults_names_each_section() {
    let names = [
        "entry",
        "sha256",
        "pack",
        "cell",
        "key",
        "name",
        "range",
        "extension",
    ];
    for (section_name, name) in SECTIONS.into_iter().zip(names) {
        assert_eq!(
            text(Malformed::Header(HeaderFault::Misaligned(section_name))),
            format!("tree list L2: header: the {name} section is not 8-byte aligned")
        );
        assert_eq!(
            text(Malformed::Header(HeaderFault::Overlap(section_name))),
            format!("tree list L2: header: the {name} section overlaps the one before it")
        );
        assert_eq!(
            text(Malformed::Header(HeaderFault::PastEnd(section_name))),
            format!("tree list L2: header: the {name} section is past the end")
        );
        assert_eq!(
            text(Malformed::Header(HeaderFault::TooMany(section_name))),
            format!(
                "tree list L2: header: the {name} section has more than 2^32 - 1 records or bytes"
            )
        );
    }
}

#[test]
fn display_of_the_pack_faults() {
    let pack = |index, fault| text(Malformed::Pack { index, fault });
    assert_eq!(
        pack(1, PackFault::KeyStart),
        "tree list L2: pack 1: its key does not begin where the one before it ends"
    );
    assert_eq!(
        pack(0, PackFault::KeyLength(1025)),
        "tree list L2: pack 0: a key of 1025 bytes, not 1 to 1024"
    );
    assert_eq!(
        pack(2, PackFault::KeyBounds),
        "tree list L2: pack 2: its key is past the end of the keys"
    );
    assert_eq!(
        pack(2, PackFault::KeyNotUtf8),
        "tree list L2: pack 2: its key is not UTF-8"
    );
    assert_eq!(
        pack(3, PackFault::KeyNotAscending),
        "tree list L2: pack 3: its key does not sort after the one before it"
    );
    assert_eq!(pack(4, PackFault::Empty), "tree list L2: pack 4: size 0");
    assert_eq!(
        pack(4, PackFault::CellShift(21)),
        "tree list L2: pack 4: cell shift 21, not 20"
    );
    assert_eq!(
        pack(
            5,
            PackFault::CellCount {
                expected: 3,
                found: 4
            }
        ),
        "tree list L2: pack 5: 4 cells, its size has 3"
    );
    assert_eq!(
        pack(
            6,
            PackFault::CellFirst {
                expected: 3,
                found: 0
            }
        ),
        "tree list L2: pack 6: first cell 0, the packs before it end at 3"
    );
}

#[test]
fn display_of_the_totals_ranges_extensions_and_orphans() {
    assert_eq!(
        text(Malformed::KeysLength),
        "tree list L2: the keys do not end where the last pack's key ends"
    );
    assert_eq!(
        text(Malformed::CellTotal {
            expected: 4,
            found: 3
        }),
        "tree list L2: 3 cells for packs of 4"
    );
    assert_eq!(
        text(Malformed::Range(2)),
        "tree list L2: range 2: no such pack, empty, or past the end of its pack"
    );
    assert_eq!(
        text(Malformed::Extension {
            at: 8,
            fault: ExtensionFault::Truncated
        }),
        "tree list L2: extension at byte 8: truncated"
    );
    assert_eq!(
        text(Malformed::Extension {
            at: 0,
            fault: ExtensionFault::Required(0x8000_0001)
        }),
        "tree list L2: extension at byte 0: unknown required tag 0x80000001"
    );
    assert_eq!(
        text(Malformed::Orphans(3)),
        "tree list L2: entries from 3 on are in no directory"
    );
}

#[test]
fn display_of_the_entry_faults() {
    let entry = |index, fault| text(Malformed::Entry { index, fault });
    assert_eq!(
        entry(7, EntryFault::Parent(3)),
        "tree list L2: entry 7: parent 3 is not the directory that holds it"
    );
    assert_eq!(
        entry(0, EntryFault::Root),
        "tree list L2: entry 0: the root is not a directory with no name and parent 0"
    );
    assert_eq!(
        entry(1, EntryFault::UnknownKind(3)),
        "tree list L2: entry 1: unknown kind 3"
    );
    assert_eq!(
        entry(1, EntryFault::Unreachable),
        "tree list L2: entry 1: a directory before the children of the directories before it"
    );
    assert_eq!(
        entry(1, EntryFault::Children),
        "tree list L2: entry 1: children not where the breadth-first order puts them"
    );
    assert_eq!(
        entry(2, EntryFault::NameBounds),
        "tree list L2: entry 2: name past the end of the names"
    );
    assert_eq!(
        entry(2, EntryFault::BadName),
        "tree list L2: entry 2: bad name"
    );
    assert_eq!(
        entry(2, EntryFault::NotAscending),
        "tree list L2: entry 2: name not after its sibling's"
    );
    assert_eq!(
        entry(2, EntryFault::PathTooLong),
        "tree list L2: entry 2: path over 4096 bytes"
    );
    assert_eq!(
        entry(2, EntryFault::Pack(9)),
        "tree list L2: entry 2: no pack 9"
    );
    assert_eq!(
        entry(2, EntryFault::OutsidePack),
        "tree list L2: entry 2: bytes past the end of its pack"
    );
    assert_eq!(
        entry(2, EntryFault::EmptyDigest),
        "tree list L2: entry 2: a file of size 0 has the digest of no bytes"
    );
    assert_eq!(
        entry(2, EntryFault::Target),
        "tree list L2: entry 2: bad target"
    );
}

#[test]
fn display_of_the_tree_id_faults_prints_the_ids_in_lowercase_hex() {
    let expected = [0xab; 32];
    let mut found = [0u8; 32];
    found[0] = 0x01;
    found[31] = 0xfe;
    assert_eq!(
        text(Malformed::WrongTree { expected, found }),
        format!(
            "tree list L2: the list is of tree 01{}fe, not {}",
            "00".repeat(30),
            "ab".repeat(32)
        )
    );
    assert_eq!(
        text(Malformed::Projection { found }),
        format!(
            "tree list L2: the version 2 list it holds hashes to 01{}fe, not to its tree id",
            "00".repeat(30)
        )
    );
}

#[test]
fn display_of_a_refusal_read_from_real_bytes_is_the_text_of_its_value() {
    let err = open_err(edited(|l2| p64!(l2, pack_at(l2, 1) + 16, 0)));
    assert_eq!(
        err.to_string(),
        "tree list L2: pack 1: first cell 0, the packs before it end at 3"
    );
}
