//! Every way `TreeListL2::open` and `open_sidecar` refuse before the packs: the magic (a version 1
//! or 2 list is a list of another release, anything else is no list), each header field and each
//! section's place, and the tree id of `open_sidecar` (a missing sidecar, another tree, a
//! projection that is not the tree). Each test patches the bytes of a valid list (the header's CRC
//! set again where the test is about a later field) and asserts the whole error value; two tests
//! assert the `Display` text of the unsupported-list refusals and their labels. The refusals of
//! the packs, ranges and extensions and the `Display` text of every other error are in
//! `refuse_pack.rs`, those of the entries in `refuse_entries.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{crc32c, recrc, section, sha256, small_tree};
use tierstore_fetch_api::list::{ListVersionUnsupported, Refusal};
use tierstore_list::{HeaderFault, Malformed, OpenError, Section, TreeListL2};

const V1_LIST: &[u8] =
    include_bytes!("../../tierstore/tests/stored_formats/fixtures/tree_list_v1.jsonl");
const V2_LIST: &[u8] =
    include_bytes!("../../tierstore/tests/stored_formats/fixtures/tree_list_v2.bin");

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

fn get_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// The L2 list of `small_tree()`, edited by `edit`, its header CRC set again.
fn edited(edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let (_, mut l2) = small_tree().lists();
    edit(&mut l2);
    recrc(&mut l2);
    l2
}

/// Sets the offset and count of section `index` in the header.
fn set_section(l2: &mut [u8], index: usize, offset: u64, count: u64) {
    p64!(l2, 56 + 16 * index, offset);
    p64!(l2, 64 + 16 * index, count);
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

fn unsupported(reason: Refusal) -> OpenError {
    OpenError::Unsupported(ListVersionUnsupported { reason })
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

// ---- the magic and the lists of other releases -------------------------------------------------

#[test]
fn a_version_1_json_list_is_refused_as_v1_list() {
    assert_eq!(open_err(V1_LIST.to_vec()), unsupported(Refusal::V1List));
}

#[test]
fn a_version_2_tstree_list_is_refused_as_v1_list() {
    assert_eq!(open_err(V2_LIST.to_vec()), unsupported(Refusal::V1List));
}

#[test]
fn a_lone_brace_and_a_lone_v2_magic_are_refused_as_v1_list() {
    assert_eq!(open_err(b"{".to_vec()), unsupported(Refusal::V1List));
    assert_eq!(
        open_err(b"TSTREE\0\0".to_vec()),
        unsupported(Refusal::V1List)
    );
}

#[test]
fn a_tstree_prefix_that_is_not_the_v2_magic_is_bad_magic() {
    let bad = OpenError::Malformed(Malformed::BadMagic);
    for bytes in [
        &b"TSTREE"[..],
        b"TSTREE\0",
        b"TSTREE\0\x01",
        b"TSTREE\x01\0",
        b"TSTREF\0\0",
        b"tstree\0\0",
        b" TSTREE\0\0",
    ] {
        assert_eq!(open_err(bytes.to_vec()), bad, "{bytes:?}");
    }
}

#[test]
fn an_l2_magic_with_a_wrong_byte_or_cut_short_is_bad_magic() {
    let bad = OpenError::Malformed(Malformed::BadMagic);
    for bytes in [
        &b"TSL2TREF"[..],
        b"TSL2TRE",
        b"tsl2tree",
        b"XSL2TREE",
        b"TSL2",
    ] {
        assert_eq!(open_err(bytes.to_vec()), bad, "{bytes:?}");
    }
    // The right magic with nothing after it is a header fault, not a bad magic.
    assert_eq!(
        open_err(b"TSL2TREE".to_vec()),
        header_fault(HeaderFault::Truncated)
    );
}

#[test]
fn an_empty_object_is_bad_magic() {
    assert_eq!(
        open_err(Vec::new()),
        OpenError::Malformed(Malformed::BadMagic)
    );
}

#[test]
fn zeros_and_pseudo_random_bytes_are_bad_magic() {
    let bad = OpenError::Malformed(Malformed::BadMagic);
    assert_eq!(open_err(vec![0; 4096]), bad);
    assert_eq!(open_err(support::pack_bytes(9, 4096)), bad);
    assert_eq!(open_err(b"hello, not a list".to_vec()), bad);
    assert_eq!(open_err(vec![0xff; 192]), bad);
}

// ---- open_sidecar ------------------------------------------------------------------------------

#[test]
fn open_sidecar_of_no_object_is_refused_as_missing_sidecar() {
    let (v2, _) = small_tree().lists();
    assert_eq!(
        TreeListL2::<Vec<u8>>::open_sidecar(None, &sha256(&v2)).unwrap_err(),
        unsupported(Refusal::MissingSidecar)
    );
}

#[test]
fn open_sidecar_refuses_a_v1_or_v2_list_as_v1_list() {
    let id = [7; 32];
    assert_eq!(
        TreeListL2::open_sidecar(Some(V1_LIST.to_vec()), &id).unwrap_err(),
        unsupported(Refusal::V1List)
    );
    assert_eq!(
        TreeListL2::open_sidecar(Some(V2_LIST.to_vec()), &id).unwrap_err(),
        unsupported(Refusal::V1List)
    );
}

#[test]
fn open_sidecar_of_another_tree_is_wrong_tree_with_both_ids() {
    let (v2, l2) = small_tree().lists();
    let other = [0x5a; 32];
    assert_eq!(
        TreeListL2::open_sidecar(Some(l2), &other).unwrap_err(),
        OpenError::Malformed(Malformed::WrongTree {
            expected: other,
            found: sha256(&v2),
        })
    );
}

#[test]
fn open_sidecar_refuses_a_list_open_refuses_before_it_looks_at_the_id() {
    let (v2, mut l2) = small_tree().lists();
    p16!(&mut l2, 8, 2);
    // Unsupported version, and an id that is not the list's: the version is what is reported.
    assert_eq!(
        TreeListL2::open_sidecar(Some(l2.clone()), &[1; 32]).unwrap_err(),
        unsupported(Refusal::UnknownVersion(2))
    );
    assert_eq!(
        TreeListL2::open_sidecar(Some(l2), &sha256(&v2)).unwrap_err(),
        unsupported(Refusal::UnknownVersion(2))
    );
}

#[test]
fn open_sidecar_names_the_wrong_tree_before_it_hashes_the_projection() {
    // The header names tree X and the projection is of another tree; asked for tree Y, the list
    // is of the wrong tree (the cheap check), not a bad projection.
    let x = [0x11; 32];
    let l2 = edited(|l2| l2[24..56].copy_from_slice(&x));
    let y = [0x22; 32];
    assert_eq!(
        TreeListL2::open_sidecar(Some(l2), &y).unwrap_err(),
        OpenError::Malformed(Malformed::WrongTree {
            expected: y,
            found: x,
        })
    );
}

#[test]
fn a_header_naming_another_tree_than_its_entries_is_a_projection_fault() {
    let (v2, _) = small_tree().lists();
    let x = [0x11; 32];
    let l2 = edited(|l2| l2[24..56].copy_from_slice(&x));
    assert!(TreeListL2::open(l2.clone()).is_ok());
    assert_eq!(
        TreeListL2::open_sidecar(Some(l2), &x).unwrap_err(),
        OpenError::Malformed(Malformed::Projection { found: sha256(&v2) })
    );
}

/// The offset of section `index` in a version 2 list's header (entries, sha256s, key offsets, keys,
/// names, ranges).
fn v2_section(v2: &[u8], index: usize) -> usize {
    get_u32(v2, 16 + 8 * index) as usize
}

/// The L2 list and the version 2 list of `small_tree()`, both edited by `edit_l2` and `edit_v2`
/// (which make the same change), the L2 list's header naming the edited version 2 list.
fn consistent_edit(
    edit_l2: impl FnOnce(&mut [u8]),
    edit_v2: impl FnOnce(&mut [u8]),
) -> (Vec<u8>, [u8; 32], [u8; 32]) {
    let (mut v2, mut l2) = small_tree().lists();
    let before = sha256(&v2);
    edit_l2(&mut l2);
    edit_v2(&mut v2);
    let after = sha256(&v2);
    assert_ne!(before, after);
    recrc(&mut l2);
    (l2, before, after)
}

#[test]
fn a_file_name_edited_in_the_names_is_a_projection_fault_with_the_digest_of_the_edit() {
    let (v2, l2) = small_tree().lists();
    let find = |bytes: &[u8], from: usize| {
        from + bytes[from..]
            .windows(5)
            .position(|w| w == b"a.txt")
            .unwrap()
    };
    let (l2_names, _) = section(&l2, 5);
    let l2_at = find(&l2, l2_names);
    let v2_at = find(&v2, v2_section(&v2, 4));
    let (edited_l2, id, after) = consistent_edit(
        |l2| l2[l2_at..l2_at + 5].copy_from_slice(b"a.tyt"),
        |v2| v2[v2_at..v2_at + 5].copy_from_slice(b"a.tyt"),
    );
    // The names are consistent with the entries, so the edited list opens, but it is not the tree.
    assert!(TreeListL2::open(edited_l2.clone()).is_ok());
    assert_eq!(
        TreeListL2::open_sidecar(Some(edited_l2), &id).unwrap_err(),
        OpenError::Malformed(Malformed::Projection { found: after })
    );
}

#[test]
fn a_file_digest_edited_in_the_digests_is_a_projection_fault() {
    let (l2, id, after) = consistent_edit(
        |l2| {
            let at = section(l2, 1).0 + 32 * 4;
            l2[at] ^= 0x80;
        },
        |v2| {
            let at = v2_section(v2, 1) + 32 * 4;
            v2[at] ^= 0x80;
        },
    );
    assert_eq!(
        TreeListL2::open_sidecar(Some(l2), &id).unwrap_err(),
        OpenError::Malformed(Malformed::Projection { found: after })
    );
}

#[test]
fn a_pack_key_edited_in_the_keys_is_a_projection_fault() {
    let (l2, id, after) = consistent_edit(
        |l2| {
            let at = section(l2, 4).0 + 6;
            assert_eq!(l2[at], b'0');
            l2[at] = b'1';
        },
        |v2| {
            let at = v2_section(v2, 3) + 6;
            assert_eq!(v2[at], b'0');
            v2[at] = b'1';
        },
    );
    assert_eq!(
        TreeListL2::open_sidecar(Some(l2), &id).unwrap_err(),
        OpenError::Malformed(Malformed::Projection { found: after })
    );
}

#[test]
fn a_range_edited_in_the_ranges_is_a_projection_fault() {
    let (l2, id, after) = consistent_edit(
        |l2| {
            let at = section(l2, 6).0 + 16;
            let len = u64::from_le_bytes(l2[at..at + 8].try_into().unwrap());
            assert!(len > 1);
            p64!(l2, at, len - 1);
        },
        |v2| {
            let at = v2_section(v2, 5) + 16;
            let len = u64::from_le_bytes(v2[at..at + 8].try_into().unwrap());
            p64!(v2, at, len - 1);
        },
    );
    assert_eq!(
        TreeListL2::open_sidecar(Some(l2), &id).unwrap_err(),
        OpenError::Malformed(Malformed::Projection { found: after })
    );
}

#[test]
fn a_mode_edited_in_an_entry_is_a_projection_fault() {
    let (l2, id, after) = consistent_edit(
        |l2| {
            let at = section(l2, 0).0 + 32 * 3 + 12;
            l2[at] ^= 1;
        },
        |v2| {
            let at = v2_section(v2, 0) + 32 * 3 + 12;
            v2[at] ^= 1;
        },
    );
    assert_eq!(
        TreeListL2::open_sidecar(Some(l2), &id).unwrap_err(),
        OpenError::Malformed(Malformed::Projection { found: after })
    );
}

#[test]
fn a_cell_digest_and_a_pack_flag_are_not_covered_by_the_tree_id() {
    let (v2, _) = small_tree().lists();
    let id = sha256(&v2);
    let cell = edited(|l2| {
        let at = section(l2, 3).0;
        l2[at] ^= 0xff;
    });
    assert!(TreeListL2::open_sidecar(Some(cell), &id).is_ok());
    let flags = edited(|l2| {
        let at = pack_at(l2, 1) + 29;
        l2[at] ^= 0x02;
    });
    assert!(TreeListL2::open_sidecar(Some(flags), &id).is_ok());
}

// ---- the header --------------------------------------------------------------------------------

#[test]
fn major_version_other_than_1_is_unknown_version() {
    for major in [0u16, 2, 3, 0xffff] {
        let (_, mut l2) = small_tree().lists();
        p16!(&mut l2, 8, major);
        recrc(&mut l2);
        assert_eq!(
            open_err(l2),
            unsupported(Refusal::UnknownVersion(u32::from(major))),
            "major {major}"
        );
    }
}

#[test]
fn the_major_version_is_judged_before_the_crc_and_the_length() {
    let (_, mut l2) = small_tree().lists();
    p16!(&mut l2, 8, 2);
    // The CRC is now stale and the header length absurd: the version is still what is reported.
    p32!(&mut l2, 16, 3);
    assert_eq!(open_err(l2), unsupported(Refusal::UnknownVersion(2)));
}

#[test]
fn a_header_shorter_than_192_bytes_is_truncated() {
    let (_, l2) = small_tree().lists();
    assert_eq!(
        open_err(l2[..191].to_vec()),
        header_fault(HeaderFault::Truncated)
    );
    assert_eq!(
        open_err(l2[..8].to_vec()),
        header_fault(HeaderFault::Truncated)
    );
    // A full header with nothing behind it is not truncated: its first section is past the end.
    assert_eq!(
        open_err(l2[..192].to_vec()),
        header_fault(HeaderFault::PastEnd(Section::Entries))
    );
}

#[test]
fn a_header_length_below_192_or_not_a_multiple_of_8_or_past_the_end_is_length() {
    let (_, l2) = small_tree().lists();
    let too_long = l2.len().next_multiple_of(8) as u32 + 8;
    for length in [0u32, 8, 184, 191, 196, 201, too_long, 0xffff_fff8, u32::MAX] {
        let mut bytes = l2.clone();
        p32!(&mut bytes, 16, length);
        assert_eq!(
            open_err(bytes),
            header_fault(HeaderFault::Length(length)),
            "length {length}"
        );
    }
}

#[test]
fn a_flipped_header_byte_is_a_crc_fault_with_the_stored_and_computed_values() {
    let (_, l2) = small_tree().lists();
    let stored = get_u32(&l2, 188);
    // A byte of the tree id, of the version fields, of a section's count and of the zero field.
    for at in [24usize, 40, 10, 22, 56, 70, 184] {
        let mut bytes = l2.clone();
        bytes[at] ^= 0x01;
        let mut header = bytes[..192].to_vec();
        header[188..192].fill(0);
        assert_eq!(
            open_err(bytes),
            header_fault(HeaderFault::Crc {
                stored,
                computed: crc32c(&header),
            }),
            "byte {at}"
        );
    }
}

#[test]
fn a_flipped_crc_byte_is_a_crc_fault() {
    let (_, l2) = small_tree().lists();
    let stored = get_u32(&l2, 188);
    let mut bytes = l2;
    bytes[188] ^= 0x10;
    assert_eq!(
        open_err(bytes),
        header_fault(HeaderFault::Crc {
            stored: stored ^ 0x10,
            computed: stored,
        })
    );
}

#[test]
fn the_crc_covers_a_header_as_long_as_its_length_field_says() {
    let (_, mut l2) = small_tree().lists();
    let stored = get_u32(&l2, 188);
    // A length of 200 makes the first section's first 8 bytes part of the header: the CRC, over
    // 200 bytes, is not the one stored for 192.
    p32!(&mut l2, 16, 200);
    let mut header = l2[..200].to_vec();
    header[188..192].fill(0);
    assert_eq!(
        open_err(l2),
        header_fault(HeaderFault::Crc {
            stored,
            computed: crc32c(&header),
        })
    );
}

#[test]
fn required_flags_are_refused_with_the_flags() {
    for flags in [1u32, 2, 0x8000_0000, 0xffff_ffff] {
        let l2 = edited(|l2| p32!(l2, 12, flags));
        assert_eq!(
            open_err(l2),
            header_fault(HeaderFault::RequiredFlags(flags)),
            "flags {flags:#x}"
        );
    }
}

#[test]
fn the_crc_is_judged_before_the_flags() {
    let (_, mut l2) = small_tree().lists();
    p32!(&mut l2, 12, 1);
    assert!(matches!(
        open_err(l2),
        OpenError::Malformed(Malformed::Header(HeaderFault::Crc { .. }))
    ));
}

#[test]
fn an_unknown_cell_digest_is_refused_as_unknown_cell_digest() {
    for digest in [0u8, 2, 3, 255] {
        let l2 = edited(|l2| l2[20] = digest);
        assert_eq!(
            open_err(l2),
            unsupported(Refusal::UnknownCellDigest(digest)),
            "digest {digest}"
        );
    }
}

#[test]
fn flags_are_judged_before_the_cell_digest_and_the_cell_digest_before_the_shift() {
    let flags_and_digest = edited(|l2| {
        p32!(l2, 12, 1);
        l2[20] = 2;
    });
    assert_eq!(
        open_err(flags_and_digest),
        header_fault(HeaderFault::RequiredFlags(1))
    );
    let digest_and_shift = edited(|l2| {
        l2[20] = 2;
        l2[21] = 19;
    });
    assert_eq!(
        open_err(digest_and_shift),
        unsupported(Refusal::UnknownCellDigest(2))
    );
}

#[test]
fn a_default_cell_shift_other_than_20_is_refused_with_the_shift() {
    for shift in [0u8, 12, 19, 21, 64, 255] {
        let l2 = edited(|l2| l2[21] = shift);
        assert_eq!(
            open_err(l2),
            header_fault(HeaderFault::CellShift(shift)),
            "shift {shift}"
        );
    }
}

#[test]
fn a_misaligned_section_is_refused_with_its_name() {
    for (index, section_name) in SECTIONS.into_iter().enumerate() {
        let l2 = edited(|l2| {
            let (offset, count) = section(l2, index);
            set_section(l2, index, offset as u64 + 1, count);
        });
        assert_eq!(
            open_err(l2),
            header_fault(HeaderFault::Misaligned(section_name)),
            "section {index}"
        );
    }
}

#[test]
fn a_section_before_the_end_of_the_one_before_is_an_overlap() {
    for (index, section_name) in SECTIONS.into_iter().enumerate() {
        let l2 = edited(|l2| {
            let (_, count) = section(l2, index);
            // Entries: inside the header. Any other: 8 bytes before the one before it begins.
            let offset = if index == 0 {
                184
            } else {
                section(l2, index - 1).0 as u64 - 8
            };
            set_section(l2, index, offset, count);
        });
        assert_eq!(
            open_err(l2),
            header_fault(HeaderFault::Overlap(section_name)),
            "section {index}"
        );
    }
}

#[test]
fn a_section_that_begins_inside_the_previous_sections_records_is_an_overlap() {
    // Keys and names are one byte per unit: the names, moved to the keys' last 8 bytes, overlap.
    let l2 = edited(|l2| {
        let (keys, count) = section(l2, 4);
        let (_, names) = section(l2, 5);
        assert!(count > 8);
        set_section(l2, 5, (keys as u64 + count) / 8 * 8 - 8, names);
    });
    assert_eq!(
        open_err(l2),
        header_fault(HeaderFault::Overlap(Section::Names))
    );
}

#[test]
fn a_count_of_2_to_the_32_or_more_is_too_many_in_every_section() {
    for (index, section_name) in SECTIONS.into_iter().enumerate() {
        for count in [1u64 << 32, (1 << 32) + 1, u64::MAX] {
            let l2 = edited(|l2| set_count(l2, index, count));
            assert_eq!(
                open_err(l2),
                header_fault(HeaderFault::TooMany(section_name)),
                "section {index}, count {count}"
            );
        }
    }
}

#[test]
fn a_count_of_2_to_the_32_minus_1_is_past_the_end_in_every_section() {
    for (index, section_name) in SECTIONS.into_iter().enumerate() {
        let l2 = edited(|l2| set_count(l2, index, u64::from(u32::MAX)));
        assert_eq!(
            open_err(l2),
            header_fault(HeaderFault::PastEnd(section_name)),
            "section {index}"
        );
    }
}

#[test]
fn a_section_one_record_past_the_end_is_past_the_end_and_one_that_fits_is_not() {
    let (_, l2) = small_tree().lists();
    let (offset, count) = section(&l2, 6);
    let room = (l2.len() - offset) as u64 / 24;
    assert!(room >= count);
    let fits = edited(|l2| set_count(l2, 6, room));
    // It overlaps no section after it only if the extensions begin after it; either way it is not
    // past the end of the object.
    assert_ne!(
        TreeListL2::open(fits).err(),
        Some(header_fault(HeaderFault::PastEnd(Section::Ranges)))
    );
    let past = edited(|l2| set_count(l2, 6, room + 1));
    assert_eq!(
        open_err(past),
        header_fault(HeaderFault::PastEnd(Section::Ranges))
    );
}

#[test]
fn a_section_whose_offset_plus_size_overflows_is_past_the_end() {
    for (index, section_name) in SECTIONS.into_iter().enumerate() {
        let l2 = edited(|l2| set_section(l2, index, u64::MAX - 7, 1));
        assert_eq!(
            open_err(l2),
            header_fault(HeaderFault::PastEnd(section_name)),
            "section {index}"
        );
    }
}

#[test]
fn an_empty_section_past_the_end_is_past_the_end() {
    let (_, l2) = small_tree().lists();
    let beyond = l2.len().next_multiple_of(8) as u64 + 8;
    let bytes = edited(|l2| set_section(l2, 7, beyond, 0));
    assert_eq!(
        open_err(bytes),
        header_fault(HeaderFault::PastEnd(Section::Extensions))
    );
}

#[test]
fn no_entries_is_refused() {
    let l2 = edited(|l2| set_count(l2, 0, 0));
    assert_eq!(open_err(l2), header_fault(HeaderFault::NoEntries));
}

#[test]
fn a_digest_count_other_than_the_entry_count_is_refused_with_the_digest_count() {
    let l2 = edited(|l2| set_count(l2, 1, 1));
    assert_eq!(open_err(l2), header_fault(HeaderFault::DigestCount(1)));
    let l2 = edited(|l2| set_count(l2, 1, 0));
    assert_eq!(open_err(l2), header_fault(HeaderFault::DigestCount(0)));
    let l2 = edited(|l2| {
        let (_, count) = section(l2, 0);
        set_count(l2, 0, count - 1);
    });
    assert_eq!(open_err(l2), header_fault(HeaderFault::DigestCount(10)));
}

#[test]
fn an_extension_count_that_is_not_a_multiple_of_8_is_refused() {
    for count in [1usize, 4, 7, 9, 15] {
        let l2 = with_extensions(&[0; 16]);
        let mut bytes = l2;
        set_count(&mut bytes, 7, count as u64);
        recrc(&mut bytes);
        assert_eq!(
            open_err(bytes),
            header_fault(HeaderFault::ExtensionLength(count as u64)),
            "count {count}"
        );
    }
}

// ---- the refusals and their labels -------------------------------------------------------------

#[test]
fn each_refusal_has_its_label_and_each_unsupported_error_prints_it() {
    let table = [
        (Refusal::V1List, "v1_list"),
        (Refusal::MissingSidecar, "missing_sidecar"),
        (Refusal::UnknownCellDigest(2), "unknown_cell_digest"),
        (Refusal::UnknownVersion(2), "unknown_version"),
    ];
    for (reason, label) in table {
        assert_eq!(reason.label(), label);
        let error = unsupported(reason);
        assert_eq!(
            error.to_string(),
            format!("tree list not supported: {label}")
        );
    }
}

#[test]
fn the_refusals_of_each_unsupported_list_print_their_label() {
    assert_eq!(
        open_err(V1_LIST.to_vec()).to_string(),
        "tree list not supported: v1_list"
    );
    assert_eq!(
        open_err(V2_LIST.to_vec()).to_string(),
        "tree list not supported: v1_list"
    );
    let (v2, l2) = small_tree().lists();
    assert_eq!(
        TreeListL2::<Vec<u8>>::open_sidecar(None, &sha256(&v2))
            .unwrap_err()
            .to_string(),
        "tree list not supported: missing_sidecar"
    );
    let mut bytes = l2.clone();
    p16!(&mut bytes, 8, 2);
    assert_eq!(
        open_err(bytes).to_string(),
        "tree list not supported: unknown_version"
    );
    let bytes = edited(|l2| l2[20] = 2);
    assert_eq!(
        open_err(bytes).to_string(),
        "tree list not supported: unknown_cell_digest"
    );
}
