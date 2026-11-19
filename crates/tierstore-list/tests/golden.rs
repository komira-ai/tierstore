//! The L2 tree list (the `TSL2TREE` binary format): the bytes of the first release's example, their
//! SHA-256, the header, every PACKS record and every cell digest decoded field by field against a
//! table written by hand, the projection that names the tree, and the same list as a later writer
//! may write it.
//!
//! The golden is `sidecar_from_v2` of tierstore's own version 2 golden
//! (`crates/tierstore/tests/stored_formats/fixtures/tree_list_v2.bin`) with the two packs whose
//! bytes `pack_0` and `pack_1` below state. The newer fixture is the golden as `docs/tree-list-l2.md`
//! ("Changing the format") lets a later writer write it: minor version 3, a header of 200 bytes (the
//! sections 8 bytes later), the ignored header and PACKS fields set, `pack_flags` bits above 1 set,
//! one extension record without the required bit, and bytes after the last section. This reader
//! must read it to the same tree, packs, cells and projection.
//!
//! This is the format's first release, so there is no earlier reader to run on newer lists; the
//! newer fixture stands for the lists of a later writer. The test that runs the previous release's
//! reader on a newer list and the newer reader on the previous release's arrives with the first
//! change of the format, and starts from these fixtures.
//!
//! To regenerate `fixtures/tree_list_l2.bin` and `fixtures/tree_list_l2_newer.bin` (only while the
//! format's first release is unreleased): run
//! `cargo test -p tierstore-list --test golden -- --ignored regenerate`, set `GOLDEN_DIGEST` and
//! `GOLDEN_CRC` to the `sha256sum` of the new golden and the CRC-32C in its header (bytes
//! 188..192), and update the
//! tables below.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod support;

use support::{crc32c, recrc, section, sha256};
use tierstore_fetch_api::ids::{PackKey, PackRoot};
use tierstore_fetch_api::list::{ListL2, ListVersionUnsupported, NodeIndex, PackRecord, Refusal};
use tierstore_list::{
    Node, NodeKind, OpenError, PackCells, PackInfo, PackRange, Shape, TreeListL2, sidecar_from_v2,
    sidecar_key,
};

const V2: &[u8] = include_bytes!("../../tierstore/tests/stored_formats/fixtures/tree_list_v2.bin");
const GOLDEN: &[u8] = include_bytes!("fixtures/tree_list_l2.bin");
const NEWER: &[u8] = include_bytes!("fixtures/tree_list_l2_newer.bin");

/// The digest of the golden's bytes: a change of one byte of the fixture, which would be a change of
/// what the format is, fails here first.
const GOLDEN_DIGEST: &str = "89b51ed94139d6c0118ae75b5c803f69f7ec6681cc0559057a42118ad605e618";
/// The CRC-32C the golden's header holds.
const GOLDEN_CRC: u32 = 2_995_993_191;
/// The SHA-256 of the version 2 golden, which is the tree's id.
const TREE_ID: &str = "fec08016bb468cc39dc5155cada5ebfe6806cdb10b5d0cb58b7df724f6ac72dc";

const MIB: usize = 1 << 20;
const KEY_0: &str = "cas/l/3333333333333333333333333333333333333333333333333333333333333333";
const KEY_1: &str = "out/a1/0";

/// Pack 0 (`cas/l/3..3`): 12 bytes, one cell.
fn pack_0() -> Vec<u8> {
    b"hello, world".to_vec()
}

/// Pack 1 (`out/a1/0`): 1 572 864 + 100 bytes, byte `i` being `(i * 131 + 7) mod 256`: two cells,
/// the first 1 MiB and the last 524 388 bytes.
fn pack_1() -> Vec<u8> {
    (0..MIB * 3 / 2 + 100)
        .map(|i| (i * 131 + 7) as u8)
        .collect()
}

fn info(key: &str) -> Option<PackInfo> {
    let (bytes, part, shared) = match key {
        KEY_0 => (pack_0(), 0, true),
        KEY_1 => (pack_1(), 8 << 20, false),
        _ => return None,
    };
    Some(PackInfo {
        cells: PackCells::of(&bytes),
        upload_part_size: part,
        shared,
    })
}

fn fresh() -> Vec<u8> {
    sidecar_from_v2(V2, info).unwrap()
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn tree_id() -> [u8; 32] {
    sha256(V2)
}

/// The newer list: the golden with a header of 200 bytes and the other things a later writer may
/// do (see the file header), the CRC set again.
fn newer_from(golden: &[u8]) -> Vec<u8> {
    let mut out = golden[..192].to_vec();
    out.extend_from_slice(&[0xee; 8]);
    out.extend_from_slice(&golden[192..]);
    out[10..12].copy_from_slice(&3u16.to_le_bytes());
    out[16..20].copy_from_slice(&200u32.to_le_bytes());
    out[22..24].copy_from_slice(&[0x5a, 0x5a]);
    out[184..188].copy_from_slice(&[0xa5; 4]);
    for index in 0..8 {
        let at = 56 + 16 * index;
        let offset = u64_at(&out, at) + 8;
        out[at..at + 8].copy_from_slice(&offset.to_le_bytes());
    }
    // The extension section: one record, tag 1 (no required bit), 5 bytes padded to 8.
    out[56 + 16 * 7 + 8..56 + 16 * 7 + 16].copy_from_slice(&16u64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&5u32.to_le_bytes());
    out.extend_from_slice(b"abcde\0\0\0");
    // After the last section.
    out.extend_from_slice(&[0xf0; 8]);
    let (packs, count) = section(&out, 2);
    for index in 0..count as usize {
        let record = packs + 48 * index;
        out[record + 29] |= 0xfc;
        out[record + 30..record + 32].copy_from_slice(&[0xbe, 0xef]);
        out[record + 40..record + 48].copy_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
    }
    recrc(&mut out);
    out
}

/// Writes the two fixtures: `cargo test -p tierstore-list --test golden -- --ignored regenerate`.
#[test]
#[ignore = "writes the fixtures; run by hand when the first release's format changes"]
fn regenerate() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let golden = fresh();
    std::fs::write(format!("{dir}/tree_list_l2.bin"), &golden).unwrap();
    std::fs::write(format!("{dir}/tree_list_l2_newer.bin"), newer_from(&golden)).unwrap();
}

/// Asserts that a fresh sidecar of the version 2 golden and the stated packs is the fixture's bytes,
/// that the fixture hashes to the pinned digest, that it is the same every time, and that it is
/// stored at `cas/l2/` of the tree's id.
#[test]
fn the_example_is_written_byte_for_byte_and_named_by_its_tree() {
    assert_eq!(fresh(), GOLDEN);
    assert_eq!(fresh(), fresh());
    assert_eq!(hex(&sha256(GOLDEN)), GOLDEN_DIGEST);
    assert_eq!(GOLDEN.len(), 1128);
    assert_eq!(hex(&tree_id()), TREE_ID);
    assert_eq!(sidecar_key(&tree_id()), format!("cas/l2/{TREE_ID}"));
}

/// Asserts that the writer asks for the packs of the version 2 golden by key, in the sorted order of
/// the keys, once each: `cas/l/3..3` (12 bytes) and `out/a1/0`.
#[test]
fn the_writer_asks_for_each_pack_of_the_list_once_in_key_order() {
    let mut asked = Vec::new();
    let made = sidecar_from_v2(V2, |key| {
        asked.push(key.to_owned());
        info(key)
    })
    .unwrap();
    assert_eq!(asked, vec![KEY_0.to_owned(), KEY_1.to_owned()]);
    assert_eq!(made, GOLDEN);
}

/// Asserts that the header of the golden is, field by field: the magic, major 1, minor 0, no flags,
/// a header of 192 bytes, SHA-256 cells of 2^20 bytes, zero in the ignored bytes, the tree id (the
/// SHA-256 of the version 2 golden), the eight sections at the offsets and counts the reference
/// gives, zero after them, and the CRC-32C of the header with its own four bytes as zero.
#[test]
fn the_header_of_the_example_is_pinned_field_by_field() {
    assert_eq!(&GOLDEN[..8], b"TSL2TREE");
    assert_eq!(
        &GOLDEN[..8],
        &[0x54, 0x53, 0x4c, 0x32, 0x54, 0x52, 0x45, 0x45]
    );
    assert_eq!(u16_at(GOLDEN, 8), 1);
    assert_eq!(u16_at(GOLDEN, 10), 0);
    assert_eq!(u32_at(GOLDEN, 12), 0);
    assert_eq!(u32_at(GOLDEN, 16), 192);
    assert_eq!(GOLDEN[20], 1);
    assert_eq!(GOLDEN[21], 20);
    assert_eq!(&GOLDEN[22..24], &[0, 0]);
    assert_eq!(&GOLDEN[24..56], &tree_id());
    let sections: Vec<(u64, u64)> = (0..8)
        .map(|index| {
            (
                u64_at(GOLDEN, 56 + 16 * index),
                u64_at(GOLDEN, 64 + 16 * index),
            )
        })
        .collect();
    assert_eq!(
        sections,
        vec![
            (192, 9),
            (480, 9),
            (768, 2),
            (864, 3),
            (960, 78),
            (1040, 40),
            (1080, 2),
            (1128, 0),
        ]
    );
    assert_eq!(&GOLDEN[184..188], &[0, 0, 0, 0]);
    assert_eq!(u32_at(GOLDEN, 188), GOLDEN_CRC);
    let mut header = GOLDEN[..192].to_vec();
    header[188..192].fill(0);
    assert_eq!(crc32c(&header), GOLDEN_CRC);
}

/// Asserts that the entries, sha256s, keys, names and ranges sections of the golden are the version
/// 2 golden's, byte for byte (at 64, 352, 656, 736 and 776 there), that the two bytes between the
/// keys and the names are zero, and that the object ends with the ranges and an empty extension
/// section.
#[test]
fn the_shared_sections_are_those_of_the_version_2_example_byte_for_byte() {
    assert_eq!(&GOLDEN[192..480], &V2[64..352]);
    assert_eq!(&GOLDEN[480..768], &V2[352..640]);
    assert_eq!(&GOLDEN[960..1038], &V2[656..734]);
    assert_eq!(&GOLDEN[1038..1040], &[0, 0]);
    assert_eq!(&GOLDEN[1040..1080], &V2[736..776]);
    assert_eq!(&GOLDEN[1080..1128], &V2[776..824]);
    assert_eq!(&GOLDEN[960..1038], format!("{KEY_0}{KEY_1}").as_bytes());
}

/// One PACKS record, field by field.
#[derive(Debug, PartialEq, Eq)]
struct Pack {
    key_offset: u32,
    key_length: u32,
    pack_size: u64,
    cell_first: u64,
    cell_count: u32,
    cell_shift: u8,
    pack_flags: u8,
    reserved_a: [u8; 2],
    upload_part_size: u64,
    reserved_b: u64,
}

fn pack_at(bytes: &[u8], at: usize) -> Pack {
    Pack {
        key_offset: u32_at(bytes, at),
        key_length: u32_at(bytes, at + 4),
        pack_size: u64_at(bytes, at + 8),
        cell_first: u64_at(bytes, at + 16),
        cell_count: u32_at(bytes, at + 24),
        cell_shift: bytes[at + 28],
        pack_flags: bytes[at + 29],
        reserved_a: [bytes[at + 30], bytes[at + 31]],
        upload_part_size: u64_at(bytes, at + 32),
        reserved_b: u64_at(bytes, at + 40),
    }
}

/// The two PACKS records of the golden, by hand. Pack 0 (`cas/l/3..3`, 12 bytes, key 70 bytes at 0)
/// has one cell, is covered whole by `share/doc.txt` (bit 0) and is shared (bit 1): flags 3. Pack 1
/// (`out/a1/0`, key 8 bytes at 70) has 1 572 964 bytes, two cells at cell 1, a file over bytes
/// 524 288..1 572 864 only (not covered: flags 0), and was uploaded in parts of 8 MiB.
fn expected_packs() -> Vec<Pack> {
    vec![
        Pack {
            key_offset: 0,
            key_length: 70,
            pack_size: 12,
            cell_first: 0,
            cell_count: 1,
            cell_shift: 20,
            pack_flags: 3,
            reserved_a: [0, 0],
            upload_part_size: 0,
            reserved_b: 0,
        },
        Pack {
            key_offset: 70,
            key_length: 8,
            pack_size: 1_572_964,
            cell_first: 1,
            cell_count: 2,
            cell_shift: 20,
            pack_flags: 0,
            reserved_a: [0, 0],
            upload_part_size: 8_388_608,
            reserved_b: 0,
        },
    ]
}

/// Asserts that every PACKS record of the golden decodes, field by field, to the table written by
/// hand, and that the pack sizes are those of the stated pack bytes.
#[test]
fn the_packs_records_of_the_example_are_pinned_field_by_field() {
    let packs: Vec<Pack> = (0..2)
        .map(|index| pack_at(GOLDEN, 768 + 48 * index))
        .collect();
    assert_eq!(packs, expected_packs());
    assert_eq!(pack_0().len() as u64, packs[0].pack_size);
    assert_eq!(pack_1().len() as u64, packs[1].pack_size);
}

/// Asserts that the three cell digests of the golden are, in order, the SHA-256 (by `sha2`, not by
/// the crate's `ring`) of the 12 bytes of pack 0, of the first 1 MiB of pack 1 and of its last
/// 524 388 bytes, and that they fill the cells section exactly.
#[test]
fn every_cell_digest_is_the_independent_sha256_of_its_cell() {
    let (one, two) = (pack_0(), pack_1());
    assert_eq!(two.len() - MIB, 524_388);
    let want = [sha256(&one), sha256(&two[..MIB]), sha256(&two[MIB..])];
    let found: Vec<[u8; 32]> = (0..3)
        .map(|cell| GOLDEN[864 + 32 * cell..896 + 32 * cell].try_into().unwrap())
        .collect();
    assert_eq!(found, want);
    assert_eq!(&GOLDEN[864..960], want.concat().as_slice());
}

/// Asserts that the two ranges of the golden are the version 2 golden's: all 12 bytes of pack 0 and
/// the 1 MiB of `bin/cc` at 524 288 of pack 1, with the four bytes after each pack number zero.
#[test]
fn the_ranges_of_the_example_are_pinned() {
    let records: Vec<(u32, u32, u64, u64)> = (0..2)
        .map(|index| {
            let at = 1080 + 24 * index;
            (
                u32_at(GOLDEN, at),
                u32_at(GOLDEN, at + 4),
                u64_at(GOLDEN, at + 8),
                u64_at(GOLDEN, at + 16),
            )
        })
        .collect();
    assert_eq!(records, vec![(0, 0, 0, 12), (1, 0, 524_288, 1_048_576)]);
    let list = TreeListL2::open(GOLDEN).unwrap();
    assert_eq!(
        list.ranges().collect::<Vec<_>>(),
        vec![
            PackRange {
                pack: 0,
                offset: 0,
                len: 12
            },
            PackRange {
                pack: 1,
                offset: 524_288,
                len: 1_048_576
            },
        ]
    );
}

fn node(parent: u64, name: &'static [u8], mode: u16, kind: NodeKind<'static>) -> Node<'static> {
    Node {
        parent: NodeIndex(parent),
        name,
        mode,
        kind,
    }
}

fn dir(first: u64, count: u64) -> NodeKind<'static> {
    NodeKind::Dir { first, count }
}

/// The nine entries of the golden, breadth first, as the reader gives them.
fn expected_nodes() -> Vec<Node<'static>> {
    vec![
        node(0, b"", 0o755, dir(1, 5)),
        node(0, b"bin", 0o755, dir(6, 1)),
        node(0, b"include", 0o755, dir(7, 1)),
        node(0, b"lib", 0, NodeKind::Symlink { target: b"bin" }),
        node(0, b"share", 0o755, dir(8, 1)),
        node(0, b"tmp", 0o1777, dir(9, 0)),
        node(
            1,
            b"cc",
            0o755,
            NodeKind::File {
                pack: Some(1),
                offset: 524_288,
                size: 1_048_576,
            },
        ),
        node(
            2,
            b"empty.h",
            0o644,
            NodeKind::File {
                pack: None,
                offset: 0,
                size: 0,
            },
        ),
        node(
            4,
            b"doc.txt",
            0o644,
            NodeKind::File {
                pack: Some(0),
                offset: 0,
                size: 12,
            },
        ),
    ]
}

fn nodes_of(list: &TreeListL2<&[u8]>) -> Vec<Node<'static>> {
    // The reader's nodes borrow the list; the table's are 'static, so compare after mapping the
    // borrowed names and targets to owned leaks of the few bytes of a test.
    (0..u64::from(list.len()))
        .map(|index| {
            let found = list.node(NodeIndex(index)).unwrap();
            let leak =
                |bytes: &[u8]| -> &'static [u8] { Box::leak(bytes.to_vec().into_boxed_slice()) };
            Node {
                parent: found.parent,
                name: leak(found.name),
                mode: found.mode,
                kind: match found.kind {
                    NodeKind::Symlink { target } => NodeKind::Symlink {
                        target: leak(target),
                    },
                    NodeKind::Dir { first, count } => NodeKind::Dir { first, count },
                    NodeKind::File { pack, offset, size } => NodeKind::File { pack, offset, size },
                },
            }
        })
        .collect()
}

/// Asserts that the golden opens to the nine entries of the table written by hand, with the shape
/// of the tree (nine entries, three files of 1 048 588 bytes, depth 2, five children at most, 55
/// bytes of paths), the digests of its files (the version 2 golden's: `2..2`, the digest of no
/// bytes, `4..4`; zero for the rest), and the sorted lookups of the root's children.
#[test]
fn the_example_opens_to_the_tree_of_the_version_2_example() {
    let list = TreeListL2::open(GOLDEN).unwrap();
    assert_eq!(nodes_of(&list), expected_nodes());
    assert_eq!(list.len(), 9);
    assert!(!list.is_empty());
    assert_eq!(list.tree_id(), &tree_id());
    assert_eq!(
        list.shape(),
        Shape {
            entries: 9,
            files: 3,
            file_bytes: 1_048_588,
            max_depth: 2,
            max_children: 5,
            path_bytes: 55,
        }
    );
    let digests: Vec<Option<[u8; 32]>> = (0..9)
        .map(|index| list.file_sha256(NodeIndex(index)))
        .collect();
    assert_eq!(
        digests,
        vec![
            None,
            None,
            None,
            None,
            None,
            None,
            Some([0x22; 32]),
            Some(sha256(b"")),
            Some([0x44; 32]),
        ]
    );
    let lookups: Vec<Option<NodeIndex>> =
        [&b"bin"[..], b"include", b"lib", b"share", b"tmp", b"zzz"]
            .iter()
            .map(|name| list.dir_lookup(NodeIndex(0), name))
            .collect();
    assert_eq!(
        lookups,
        vec![
            Some(NodeIndex(1)),
            Some(NodeIndex(2)),
            Some(NodeIndex(3)),
            Some(NodeIndex(4)),
            Some(NodeIndex(5)),
            None,
        ]
    );
    assert_eq!(list.children(NodeIndex(0)), Some(1..6));
    assert_eq!(list.pack_key(0), Some(KEY_0));
    assert_eq!(list.pack_key(1), Some(KEY_1));
    assert_eq!(list.pack_key(2), None);
}

fn expected_records() -> Vec<PackRecord> {
    vec![
        PackRecord {
            key: PackKey(KEY_0.to_owned()),
            pack_size: 12,
            cell_first: 0,
            cell_count: 1,
            cell_shift: 20,
            pack_flags: 3,
        },
        PackRecord {
            key: PackKey(KEY_1.to_owned()),
            pack_size: 1_572_964,
            cell_first: 1,
            cell_count: 2,
            cell_shift: 20,
            pack_flags: 0,
        },
    ]
}

/// The `pack_root` of a pack by the reference: SHA-256 of `TSPACK1`, the size (8 bytes), the cell
/// shift (1 byte) and the cell digests.
fn root_of(bytes: &[u8]) -> PackRoot {
    let mut preimage = b"TSPACK1".to_vec();
    preimage.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    preimage.push(20);
    for cell in bytes.chunks(MIB) {
        preimage.extend_from_slice(&sha256(cell));
    }
    PackRoot::Digest(sha256(&preimage))
}

/// Asserts that the `ListL2` view of the golden gives the two pack records of the table, each cell
/// digest the independent SHA-256 of its cell (and `None` past the last cell and the last pack), and
/// each `pack_root` the reference's hash of the pack's size, shift and digests, the same on a
/// second ask.
#[test]
fn the_example_reads_as_a_list_l2_with_the_independent_roots_and_cells() {
    let list = TreeListL2::open(GOLDEN).unwrap();
    assert_eq!(list.pack_count(), 2);
    let records: Vec<PackRecord> = (0..2).map(|pack| list.pack_record(pack).unwrap()).collect();
    assert_eq!(records, expected_records());
    assert_eq!(list.pack_record(2), None);
    let (one, two) = (pack_0(), pack_1());
    assert_eq!(list.cell_digest(0, 0), Some(sha256(&one)));
    assert_eq!(list.cell_digest(0, 1), None);
    assert_eq!(list.cell_digest(1, 0), Some(sha256(&two[..MIB])));
    assert_eq!(list.cell_digest(1, 1), Some(sha256(&two[MIB..])));
    assert_eq!(list.cell_digest(1, 2), None);
    assert_eq!(list.cell_digest(2, 0), None);
    assert_eq!(list.pack_root(0), Some(root_of(&one)));
    assert_eq!(list.pack_root(1), Some(root_of(&two)));
    assert_eq!(list.pack_root(1), Some(root_of(&two)));
    assert_eq!(list.pack_root(2), None);
    assert_eq!(list.upload_part_size(0), Some(0));
    assert_eq!(list.upload_part_size(1), Some(8 << 20));
    assert_eq!(list.upload_part_size(2), None);
}

/// Asserts that the version 2 list the golden holds hashes to the SHA-256 of the version 2 golden,
/// that `check_tree_id` accepts it, and that the sidecar opens for the tree id and is refused, with
/// the full message, for another id and when absent.
#[test]
fn the_projection_of_the_example_is_the_version_2_example_that_names_the_tree() {
    let list = TreeListL2::open(GOLDEN).unwrap();
    assert_eq!(list.projection_digest(), sha256(V2));
    assert_eq!(list.check_tree_id(), Ok(()));
    let opened = TreeListL2::open_sidecar(Some(GOLDEN), &tree_id()).unwrap();
    assert_eq!(opened.bytes(), GOLDEN);

    let other = [9u8; 32];
    let wrong = TreeListL2::open_sidecar(Some(GOLDEN), &other).unwrap_err();
    assert_eq!(
        wrong.to_string(),
        format!(
            "tree list L2: the list is of tree {TREE_ID}, not {}",
            hex(&other)
        )
    );
    assert_eq!(
        TreeListL2::<&[u8]>::open_sidecar(None, &tree_id()).unwrap_err(),
        OpenError::Unsupported(ListVersionUnsupported {
            reason: Refusal::MissingSidecar
        })
    );
}

/// Asserts that the newer fixture is the golden as the later writer's changes make it: minor
/// version 3, a header of 200 bytes holding nonzero in every ignored byte, the sections 8 bytes
/// later and the extensions 16 bytes long, PACKS records with `pack_flags` bits 2..=7 set and
/// nonzero in their ignored bytes, one extension record without the required bit, eight bytes
/// after the last section, and a valid CRC-32C over the 200 bytes.
#[test]
fn the_newer_list_is_the_example_as_a_later_writer_writes_it() {
    assert_eq!(newer_from(GOLDEN), NEWER);
    assert_eq!(NEWER.len(), GOLDEN.len() + 8 + 16 + 8);
    assert_eq!(&NEWER[..10], &GOLDEN[..10]);
    assert_eq!(u16_at(NEWER, 10), 3);
    assert_eq!(u32_at(NEWER, 16), 200);
    assert_eq!(&NEWER[22..24], &[0x5a, 0x5a]);
    assert_eq!(&NEWER[184..188], &[0xa5; 4]);
    assert_eq!(&NEWER[192..200], &[0xee; 8]);
    let sections: Vec<(u64, u64)> = (0..8)
        .map(|index| section(NEWER, index))
        .map(|(at, n)| (at as u64, n))
        .collect();
    assert_eq!(
        sections,
        vec![
            (200, 9),
            (488, 9),
            (776, 2),
            (872, 3),
            (968, 78),
            (1048, 40),
            (1088, 2),
            (1136, 16),
        ]
    );
    for index in 0..2 {
        let record = pack_at(NEWER, 776 + 48 * index);
        let golden = pack_at(GOLDEN, 768 + 48 * index);
        assert_eq!(record.pack_flags, golden.pack_flags | 0xfc);
        assert_eq!(record.reserved_a, [0xbe, 0xef]);
        assert_eq!(record.reserved_b, 0x0123_4567_89ab_cdef);
    }
    assert_eq!(
        &NEWER[1136..1152],
        &[
            1, 0, 0, 0, 5, 0, 0, 0, b'a', b'b', b'c', b'd', b'e', 0, 0, 0
        ]
    );
    assert_eq!(&NEWER[1152..], &[0xf0; 8]);
    let mut header = NEWER[..200].to_vec();
    let stored = u32_at(&header, 188);
    header[188..192].fill(0);
    assert_eq!(crc32c(&header), stored);
    assert_ne!(sha256(NEWER), sha256(GOLDEN));
}

/// Asserts that the reader reads the newer list to the same tree as the golden: the same tree id,
/// shape, nodes, file digests, ranges, pack records (but `pack_flags`, which carry the later bits),
/// cell digests and pack roots, the same projection digest, and `open_sidecar` accepts it for the
/// same tree id.
#[test]
fn the_newer_list_reads_to_the_same_tree_packs_and_cells_as_the_example() {
    let golden = TreeListL2::open(GOLDEN).unwrap();
    let newer = TreeListL2::open(NEWER).unwrap();
    assert_eq!(newer.tree_id(), golden.tree_id());
    assert_eq!(newer.shape(), golden.shape());
    assert_eq!(newer.len(), golden.len());
    assert_eq!(nodes_of(&newer), expected_nodes());
    for index in 0..9 {
        assert_eq!(
            newer.file_sha256(NodeIndex(index)),
            golden.file_sha256(NodeIndex(index))
        );
    }
    assert_eq!(
        newer.ranges().collect::<Vec<_>>(),
        golden.ranges().collect::<Vec<_>>()
    );
    assert_eq!(newer.pack_count(), golden.pack_count());
    for pack in 0..2 {
        let mut want = golden.pack_record(pack).unwrap();
        want.pack_flags |= 0xfc;
        assert_eq!(newer.pack_record(pack), Some(want));
        assert_eq!(newer.pack_key(pack), golden.pack_key(pack));
        assert_eq!(newer.pack_root(pack), golden.pack_root(pack));
        assert_eq!(newer.upload_part_size(pack), golden.upload_part_size(pack));
        for cell in 0..3 {
            assert_eq!(
                newer.cell_digest(pack, cell),
                golden.cell_digest(pack, cell)
            );
        }
    }
    assert_eq!(newer.projection_digest(), sha256(V2));
    assert_eq!(newer.check_tree_id(), Ok(()));
    let opened = TreeListL2::open_sidecar(Some(NEWER), &tree_id()).unwrap();
    assert_eq!(opened.bytes(), NEWER);
}
