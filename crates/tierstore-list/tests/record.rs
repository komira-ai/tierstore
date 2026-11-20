//! `EntryRecord` and `TreeListL2::section_range`: a reader that copies each entry record out of
//! the list at the entries' range and decodes it alone gets every node `TreeListL2::node` gives,
//! its name and a link's target read at the names' range; and the ranges are the header's.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{section, small_tree};
use tierstore_fetch_api::list::NodeIndex;
use tierstore_list::{ENTRY_LEN, EntryRecord, NodeKind, RecordKind, Section, TreeListL2};

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

/// The unit of each section's count in the header: a record's length, or 1 for a blob.
const UNITS: [u64; 8] = [32, 32, 48, 32, 1, 1, 24, 1];

#[test]
fn each_section_range_is_where_the_header_puts_it_and_as_long_as_its_count() {
    let l2 = small_tree().lists().1;
    let list = TreeListL2::open(l2.clone()).unwrap();
    for (index, which) in SECTIONS.into_iter().enumerate() {
        let (offset, count) = section(&l2, index);
        let range = list.section_range(which);
        assert_eq!(range.start, offset as u64, "{which}");
        assert_eq!(range.end - range.start, count * UNITS[index], "{which}");
        assert!(range.end <= l2.len() as u64, "{which}");
    }
}

#[test]
fn every_record_copied_out_and_decoded_is_the_node_the_reader_gives() {
    let l2 = small_tree().lists().1;
    let list = TreeListL2::open(l2.clone()).unwrap();
    let entries = list.section_range(Section::Entries);
    let names = list.section_range(Section::Names);
    let names = &l2[names.start as usize..names.end as usize];
    let mut kinds = [0usize; 3];
    for index in 0..u64::from(list.len()) {
        let at = (entries.start + index * ENTRY_LEN as u64) as usize;
        let bytes: [u8; ENTRY_LEN] = l2[at..at + ENTRY_LEN].try_into().unwrap();
        let record = EntryRecord::decode(&bytes);
        let node = list.node(NodeIndex(index)).unwrap();
        assert_eq!(record.parent, node.parent, "entry {index}");
        assert_eq!(record.mode, node.mode, "entry {index}");
        let name = &names[record.name_offset as usize..][..usize::from(record.name_len)];
        assert_eq!(name, node.name, "entry {index}");
        match (record.kind, node.kind) {
            (RecordKind::Dir { first, count }, NodeKind::Dir { first: f, count: c }) => {
                assert_eq!((first, count), (f, c));
                kinds[0] += 1;
            }
            (
                RecordKind::File { pack, offset, size },
                NodeKind::File {
                    pack: p,
                    offset: o,
                    size: s,
                },
            ) => {
                assert_eq!((pack, offset, size), (p, o, s));
                kinds[1] += 1;
            }
            (RecordKind::Symlink { offset, len }, NodeKind::Symlink { target }) => {
                assert_eq!(&names[offset as usize..(offset + len) as usize], target);
                kinds[2] += 1;
            }
            (record, node) => panic!("entry {index}: record {record:?}, node {node:?}"),
        }
    }
    // The small tree has directories, files (an empty one among them) and a link.
    assert!(kinds.iter().all(|&n| n > 0), "{kinds:?}");
}

#[test]
fn a_kind_the_release_does_not_know_decodes_as_unknown_and_an_empty_file_has_no_pack() {
    let mut bytes = [0u8; ENTRY_LEN];
    // kind_mode at 12..14: kind 3 in the top four bits, mode 0o644.
    bytes[12..14].copy_from_slice(&((3u16 << 12) | 0o644).to_le_bytes());
    let record = EntryRecord::decode(&bytes);
    assert_eq!(record.kind, RecordKind::Unknown(3));
    assert_eq!(record.mode, 0o644);

    // A file (kind 0) of size 0 names no pack, whatever its key number and offset say.
    let mut file = [0u8; ENTRY_LEN];
    file[8..12].copy_from_slice(&7u32.to_le_bytes());
    file[16..24].copy_from_slice(&99u64.to_le_bytes());
    assert_eq!(
        EntryRecord::decode(&file).kind,
        RecordKind::File {
            pack: None,
            offset: 0,
            size: 0
        }
    );
}
