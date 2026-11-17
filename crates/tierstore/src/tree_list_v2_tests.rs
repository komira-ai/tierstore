//! The version 2 tree list reader's `open`: every refusal of the object, header, keys and ranges,
//! the limits at their exact values, and the text of every error. The writer's tests are in
//! `encode`.

use crate::tree_list::MAX_TREE_LIST_BYTES;
use crate::tree_list_v2::{
    EntryFault, HeaderFault, KeyFault, ListRange, MAX_KEY_BYTES, Section, TREE_LIST_V2_MAGIC,
    TreeListV2, TreeListV2Error, is_tree_list_v2, le_u16, le_u32, le_u64,
};
use crate::tree_record::RecordError;

mod encode;

/// Where the offset of section `at` is in the header.
const fn offset_at(at: usize) -> usize {
    16 + 8 * at
}

/// Where the count of section `at` is in the header.
const fn count_at(at: usize) -> usize {
    20 + 8 * at
}

const ENTRIES: usize = 0;
const DIGESTS: usize = 1;
const KEY_OFFSETS: usize = 2;
const KEYS: usize = 3;
const NAMES: usize = 4;
const RANGES: usize = 5;

/// The sections of an object, as bytes; [`Parts::object`] lays them out after a header, each at the
/// next multiple of 8, so that a test changes one part and nothing else.
#[derive(Clone)]
struct Parts {
    entries: Vec<u8>,
    digests: Vec<u8>,
    key_offsets: Vec<u8>,
    keys: Vec<u8>,
    names: Vec<u8>,
    ranges: Vec<u8>,
}

/// One range record.
fn range_record(key: u32, offset: u64, len: u64) -> Vec<u8> {
    let mut record = key.to_le_bytes().to_vec();
    record.extend_from_slice(&[0; 4]);
    record.extend_from_slice(&offset.to_le_bytes());
    record.extend_from_slice(&len.to_le_bytes());
    record
}

/// Key offsets, little-endian.
fn offsets(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

impl Parts {
    /// A well formed object: the root alone, keys `a` and `bc`, three bytes of names and two
    /// ranges.
    fn valid() -> Self {
        let mut root = [0u8; 32];
        root[12..14].copy_from_slice(&0x11edu16.to_le_bytes());
        Self {
            entries: root.to_vec(),
            digests: vec![0; 32],
            key_offsets: offsets(&[0, 1, 3]),
            keys: b"abc".to_vec(),
            names: b"xyz".to_vec(),
            ranges: [range_record(0, 0, 4), range_record(1, 10, 5)].concat(),
        }
    }

    /// The same, with the key offsets and key bytes given and no ranges, which name keys.
    fn with_keys(values: &[u32], keys: &[u8]) -> Self {
        Self {
            key_offsets: offsets(values),
            keys: keys.to_vec(),
            ranges: Vec::new(),
            ..Self::valid()
        }
    }

    fn counts(&self) -> [u32; 6] {
        [
            (self.entries.len() / 32) as u32,
            (self.digests.len() / 32) as u32,
            (self.key_offsets.len() / 4 - 1) as u32,
            self.keys.len() as u32,
            self.names.len() as u32,
            (self.ranges.len() / 24) as u32,
        ]
    }

    /// The object: the header, then the sections each at the next multiple of 8.
    fn object(&self) -> Vec<u8> {
        let sections = [
            &self.entries,
            &self.digests,
            &self.key_offsets,
            &self.keys,
            &self.names,
            &self.ranges,
        ];
        let counts = self.counts();
        let mut out = Vec::new();
        out.extend_from_slice(&TREE_LIST_V2_MAGIC);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&[0; 6]);
        let mut at = 64usize;
        let mut places = Vec::new();
        for bytes in sections {
            at = at.next_multiple_of(8);
            places.push(at);
            at += bytes.len();
        }
        for (place, count) in places.iter().zip(counts) {
            out.extend_from_slice(&(*place as u32).to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
        }
        for (place, bytes) in places.iter().zip(sections) {
            out.resize(*place, 0);
            out.extend_from_slice(bytes);
        }
        out
    }
}

fn set_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn set_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

/// The fault of opening `bytes`, which must be one.
fn fault(bytes: &[u8]) -> TreeListV2Error {
    TreeListV2::open(bytes).expect_err("the object must be refused")
}

fn header(fault: HeaderFault) -> TreeListV2Error {
    TreeListV2Error::Header(fault)
}

const SECTION_NAMES: [(usize, Section, &str); 6] = [
    (ENTRIES, Section::Entries, "entry"),
    (DIGESTS, Section::Digests, "sha256"),
    (KEY_OFFSETS, Section::KeyOffsets, "key offset"),
    (KEYS, Section::Keys, "key"),
    (NAMES, Section::Names, "name"),
    (RANGES, Section::Ranges, "range"),
];

/// Asserts that the first eight bytes are what tells a version 2 list from a version 1 list, from
/// anything else, and from an object too short to say.
#[test]
fn is_tree_list_v2_tells_the_magic() {
    assert!(is_tree_list_v2(&Parts::valid().object()));
    assert!(is_tree_list_v2(&TREE_LIST_V2_MAGIC));
    assert!(!is_tree_list_v2(b"{\"format\":\"tierstore.tree-list\""));
    assert!(!is_tree_list_v2(b"TSTREE\0"));
    assert!(!is_tree_list_v2(b""));
    assert!(!is_tree_list_v2(b"TSTREE\0\x01"));
    assert_eq!(&TREE_LIST_V2_MAGIC, b"TSTREE\0\0");
}

/// Asserts that a well formed object opens, with the root alone it is empty and has one entry, and
/// that it opens whatever the minor version.
#[test]
fn a_well_formed_object_opens() {
    let mut bytes = Parts::valid().object();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.len(), 1);
    assert!(list.is_empty());
    set_u16(&mut bytes, 10, 9);
    assert_eq!(TreeListV2::open(&bytes).unwrap().len(), 1);
}

/// Asserts that a list of more than the root is not empty, and that `len` counts the entries.
#[test]
fn a_list_of_two_entries_is_not_empty() {
    let parts = Parts {
        entries: vec![0; 64],
        digests: vec![0; 64],
        ..Parts::valid()
    };
    let bytes = parts.object();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.len(), 2);
    assert!(!list.is_empty());
}

/// Asserts that an object with no keys and no ranges opens, with no ranges to give.
#[test]
fn an_object_with_no_keys_and_no_ranges_opens() {
    let parts = Parts {
        key_offsets: offsets(&[0]),
        keys: Vec::new(),
        ranges: Vec::new(),
        ..Parts::valid()
    };
    let bytes = parts.object();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.ranges().count(), 0);
}

/// Asserts that an object one byte over the bound is refused as too large before anything else is
/// read, with the size and the bound.
#[test]
fn an_object_over_the_bound_is_too_large() {
    let bytes = vec![0u8; MAX_TREE_LIST_BYTES + 1];
    let error = fault(&bytes);
    assert_eq!(
        error,
        TreeListV2Error::TooLarge {
            size: MAX_TREE_LIST_BYTES + 1,
            max: MAX_TREE_LIST_BYTES,
        }
    );
    assert_eq!(
        error.to_string(),
        "tree list too large: 268435457 bytes, at most 268435456"
    );
}

/// Asserts that an object that does not begin with the magic is refused as having a bad magic,
/// whatever its length, a version 1 list and an empty object included.
#[test]
fn an_object_without_the_magic_is_refused() {
    for bytes in [
        &b""[..],
        &b"TSTREE"[..],
        &b"{\"format\":\"tierstore.tree-list\",\"version\":1}\n"[..],
        &[0u8; 100][..],
    ] {
        assert_eq!(fault(bytes), TreeListV2Error::BadMagic);
    }
    let mut bytes = Parts::valid().object();
    bytes[7] = 1;
    assert_eq!(fault(&bytes), TreeListV2Error::BadMagic);
    assert_eq!(
        TreeListV2Error::BadMagic.to_string(),
        "not a version 2 tree list: bad magic"
    );
}

/// Asserts that an object with the magic and fewer than 64 bytes, from 8 to 63, is refused as a
/// truncated header, and that 64 bytes of it are read as a header.
#[test]
fn a_header_shorter_than_64_bytes_is_truncated() {
    let object = Parts::valid().object();
    for len in 8..64 {
        assert_eq!(
            fault(&object[..len]),
            header(HeaderFault::Truncated),
            "{len} bytes"
        );
    }
    assert_ne!(fault(&object[..64]), header(HeaderFault::Truncated));
    assert_eq!(
        header(HeaderFault::Truncated).to_string(),
        "tree list header: truncated"
    );
}

/// Asserts that a major version other than 1, either side of it, is refused with the version it
/// names.
#[test]
fn a_major_version_other_than_1_is_unsupported() {
    for major in [0u16, 2, 0xffff] {
        let mut bytes = Parts::valid().object();
        set_u16(&mut bytes, 8, major);
        assert_eq!(fault(&bytes), TreeListV2Error::UnsupportedVersion(major));
    }
    assert_eq!(
        TreeListV2Error::UnsupportedVersion(2).to_string(),
        "unsupported tree list version 2"
    );
}

/// Asserts that a required flag set, any bit, is refused with all the flags the header holds.
#[test]
fn a_required_flag_is_refused() {
    for flags in [1u32, 0x10, 0x8000_0000, 0xffff_ffff] {
        let mut bytes = Parts::valid().object();
        set_u32(&mut bytes, 12, flags);
        assert_eq!(fault(&bytes), header(HeaderFault::RequiredFlags(flags)));
    }
    assert_eq!(
        header(HeaderFault::RequiredFlags(0x10)).to_string(),
        "tree list header: unknown required flags 0x10"
    );
}

/// Asserts that for each section an offset that is not a multiple of 8 is refused as misaligned,
/// naming the section.
#[test]
fn a_misaligned_section_is_refused() {
    for (at, section, _) in SECTION_NAMES {
        for skew in [1u32, 4, 7] {
            let mut bytes = Parts::valid().object();
            let offset = le_u32(&bytes, offset_at(at));
            set_u32(&mut bytes, offset_at(at), offset + skew);
            assert_eq!(fault(&bytes), header(HeaderFault::Misaligned(section)));
        }
    }
    assert_eq!(
        header(HeaderFault::Misaligned(Section::Keys)).to_string(),
        "tree list header: the key section is not 8-byte aligned"
    );
}

/// Asserts that for each section an offset before the end of the section before it (the header,
/// for the first) is refused as an overlap, naming the section.
#[test]
fn an_overlapping_section_is_refused() {
    for (at, section, _) in SECTION_NAMES {
        let mut bytes = Parts::valid().object();
        let before = if at == 0 {
            56
        } else {
            le_u32(&bytes, offset_at(at - 1))
        };
        set_u32(&mut bytes, offset_at(at), before);
        assert_eq!(fault(&bytes), header(HeaderFault::Overlap(section)));
    }
    assert_eq!(
        header(HeaderFault::Overlap(Section::Names)).to_string(),
        "tree list header: the name section overlaps the one before it"
    );
}

/// Asserts that a section that begins exactly at the end of the one before it is not an overlap
/// (the digests at 96, where the 32 bytes of entries end), the boundary of the check.
#[test]
fn a_section_that_begins_where_the_one_before_ends_is_not_an_overlap() {
    let mut bytes = Parts::valid().object();
    assert_eq!(le_u32(&bytes, offset_at(ENTRIES)), 64);
    set_u32(&mut bytes, offset_at(DIGESTS), 96);
    assert_eq!(TreeListV2::open(&bytes).unwrap().len(), 1);
    set_u32(&mut bytes, offset_at(DIGESTS), 88);
    assert_eq!(
        fault(&bytes),
        header(HeaderFault::Overlap(Section::Digests))
    );
}

/// Asserts that for each section a count that makes it end past the end of the object is refused,
/// naming the section, and so is an offset past the end.
#[test]
fn a_section_past_the_end_is_refused() {
    for (at, section, _) in SECTION_NAMES {
        let mut bytes = Parts::valid().object();
        let count = le_u32(&bytes, count_at(at));
        set_u32(&mut bytes, count_at(at), count + 1000);
        assert_eq!(fault(&bytes), header(HeaderFault::PastEnd(section)));

        let mut bytes = Parts::valid().object();
        set_u32(&mut bytes, count_at(at), u32::MAX);
        assert_eq!(fault(&bytes), header(HeaderFault::PastEnd(section)));

        let mut bytes = Parts::valid().object();
        set_u32(&mut bytes, offset_at(at), 0xffff_fff8);
        assert_eq!(fault(&bytes), header(HeaderFault::PastEnd(section)));
    }
    assert_eq!(
        header(HeaderFault::PastEnd(Section::Ranges)).to_string(),
        "tree list header: the range section is past the end"
    );
}

/// Asserts that a section that ends exactly at the end of the object is within it, and one byte
/// further is past it.
#[test]
fn a_section_may_end_at_the_end_of_the_object() {
    let bytes = Parts::valid().object();
    assert_eq!(bytes.len(), le_u32(&bytes, offset_at(RANGES)) as usize + 48);
    assert!(TreeListV2::open(&bytes).is_ok());
    assert_eq!(
        fault(&bytes[..bytes.len() - 1]),
        header(HeaderFault::PastEnd(Section::Ranges))
    );
}

/// Asserts that an entry count of 0 is refused with the count.
#[test]
fn an_entry_count_of_zero_is_refused() {
    let mut bytes = Parts::valid().object();
    set_u32(&mut bytes, count_at(ENTRIES), 0);
    assert_eq!(fault(&bytes), header(HeaderFault::EntryCount(0)));
    assert_eq!(
        header(HeaderFault::EntryCount(0)).to_string(),
        "tree list header: bad entry count 0"
    );
}

/// Asserts that an entry count of 2^24 + 1 is refused: an object of at most 256 MiB cannot hold
/// that many 32-byte records, so it is refused as a section past the end.
#[test]
fn an_entry_count_over_the_most_is_refused() {
    let mut bytes = Parts::valid().object();
    set_u32(&mut bytes, count_at(ENTRIES), (1 << 24) + 1);
    assert_eq!(
        fault(&bytes),
        header(HeaderFault::PastEnd(Section::Entries))
    );
}

/// Asserts that a digest count that is not the entry count, fewer or more, is refused with the
/// digest count.
#[test]
fn a_digest_count_other_than_the_entry_count_is_refused() {
    let mut bytes = Parts::valid().object();
    set_u32(&mut bytes, count_at(DIGESTS), 0);
    assert_eq!(fault(&bytes), header(HeaderFault::DigestCount(0)));

    let parts = Parts {
        digests: vec![0; 64],
        ..Parts::valid()
    };
    assert_eq!(fault(&parts.object()), header(HeaderFault::DigestCount(2)));
    assert_eq!(
        header(HeaderFault::DigestCount(2)).to_string(),
        "tree list header: 2 digests for another count of entries"
    );
}

/// Asserts that a first key offset that is not 0 is refused as key 0 not beginning at 0.
#[test]
fn the_first_key_must_begin_at_0() {
    let bytes = Parts::with_keys(&[1, 3], b"abc").object();
    let error = fault(&bytes);
    assert_eq!(
        error,
        TreeListV2Error::Key {
            index: 0,
            fault: KeyFault::Start,
        }
    );
    assert_eq!(error.to_string(), "tree list key 0: does not begin at 0");
}

/// Asserts that a key whose end is its start (equal neighbouring offsets) is refused as empty,
/// naming the key, whether it is the first, a middle one or the last.
#[test]
fn an_empty_key_is_refused() {
    let first = Parts::with_keys(&[0, 0, 3], b"abc").object();
    let middle = Parts::with_keys(&[0, 1, 1, 3], b"abc").object();
    let last = Parts::with_keys(&[0, 3, 3], b"abc").object();
    for (bytes, index) in [(first, 0), (middle, 1), (last, 1)] {
        let error = fault(&bytes);
        assert_eq!(
            error,
            TreeListV2Error::Key {
                index,
                fault: KeyFault::Empty,
            }
        );
    }
    assert_eq!(
        TreeListV2Error::Key {
            index: 1,
            fault: KeyFault::Empty,
        }
        .to_string(),
        "tree list key 1: empty"
    );
}

/// Asserts that an offset that goes back (an end before its start) is refused as an empty key.
#[test]
fn a_key_whose_end_is_before_its_start_is_refused() {
    let bytes = Parts::with_keys(&[0, 2, 1], b"abc").object();
    assert_eq!(
        fault(&bytes),
        TreeListV2Error::Key {
            index: 1,
            fault: KeyFault::Empty,
        }
    );
}

/// Asserts that a key of 1024 bytes is read and one of 1025 is refused as too long, naming it.
#[test]
fn a_key_over_1024_bytes_is_refused() {
    assert_eq!(MAX_KEY_BYTES, 1024);
    let long = vec![b'k'; 1025];
    let ok = Parts::with_keys(&[0, 1024], &long[..1024]).object();
    let list = TreeListV2::open(&ok).unwrap();
    assert_eq!(list.keys[0].len(), 1024);

    let bytes = Parts::with_keys(&[0, 1025], &long).object();
    let error = fault(&bytes);
    assert_eq!(
        error,
        TreeListV2Error::Key {
            index: 0,
            fault: KeyFault::TooLong,
        }
    );
    assert_eq!(error.to_string(), "tree list key 0: over 1024 bytes");
}

/// Asserts that a key that is not UTF-8 is refused, naming it, and that a multi-byte UTF-8 key is
/// read.
#[test]
fn a_key_that_is_not_utf8_is_refused() {
    let bytes = Parts::with_keys(&[0, 1, 3], &[b'a', 0xff, 0xfe]).object();
    let error = fault(&bytes);
    assert_eq!(
        error,
        TreeListV2Error::Key {
            index: 1,
            fault: KeyFault::NotUtf8,
        }
    );
    assert_eq!(error.to_string(), "tree list key 1: not UTF-8");

    let utf8 = Parts::with_keys(&[0, 2], "é".as_bytes()).object();
    assert_eq!(&*TreeListV2::open(&utf8).unwrap().keys[0], "é");
}

/// Asserts that a last key offset that is not the length of the key bytes, short of it or past
/// it, is refused as key offsets that do not end at the end of the keys.
#[test]
fn key_offsets_that_do_not_end_at_the_end_of_the_keys_are_refused() {
    let short = Parts::with_keys(&[0, 1, 2], b"abc").object();
    assert_eq!(fault(&short), header(HeaderFault::KeysLength));
    let past = Parts::with_keys(&[0, 1, 9], b"abc").object();
    assert_eq!(fault(&past), header(HeaderFault::KeysLength));
    let nothing = Parts::with_keys(&[0], b"abc").object();
    assert_eq!(fault(&nothing), header(HeaderFault::KeysLength));
    assert_eq!(
        header(HeaderFault::KeysLength).to_string(),
        "tree list header: the key offsets do not end at the end of the keys"
    );
}

/// Asserts that a range that names a key the table lacks is refused with its number, though the
/// ranges before it are fine.
#[test]
fn a_range_of_a_key_past_the_table_is_refused() {
    let parts = Parts {
        ranges: [range_record(0, 0, 4), range_record(2, 0, 4)].concat(),
        ..Parts::valid()
    };
    let error = fault(&parts.object());
    assert_eq!(error, TreeListV2Error::Range(1));
    assert_eq!(
        error.to_string(),
        "tree list range 1: no such key, empty, or past 2^64"
    );
}

/// Asserts that a range of length 0 is refused with its number.
#[test]
fn a_range_of_length_0_is_refused() {
    let parts = Parts {
        ranges: range_record(1, 5, 0),
        ..Parts::valid()
    };
    assert_eq!(fault(&parts.object()), TreeListV2Error::Range(0));
}

/// Asserts that a range whose offset plus length passes 2^64 is refused, and one that ends exactly
/// at 2^64 - 1 is read.
#[test]
fn a_range_that_overflows_is_refused() {
    let parts = Parts {
        ranges: range_record(0, u64::MAX, 1),
        ..Parts::valid()
    };
    assert_eq!(fault(&parts.object()), TreeListV2Error::Range(0));
    let parts = Parts {
        ranges: range_record(0, u64::MAX - 5, 10),
        ..Parts::valid()
    };
    assert_eq!(fault(&parts.object()), TreeListV2Error::Range(0));
    let parts = Parts {
        ranges: range_record(0, u64::MAX - 5, 5),
        ..Parts::valid()
    };
    let bytes = parts.object();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(
        list.ranges().collect::<Vec<_>>(),
        [ListRange {
            key: "a".into(),
            offset: u64::MAX - 5,
            len: 5,
        }]
    );
}

/// Asserts that `ranges` gives the ranges in the order of the list, each with its key, offset and
/// length, and that a key is shared by the ranges that name it.
#[test]
fn ranges_are_given_in_the_order_of_the_list() {
    let parts = Parts {
        ranges: [
            range_record(1, 10, 5),
            range_record(0, 0, 4),
            range_record(1, 100, 7),
        ]
        .concat(),
        ..Parts::valid()
    };
    let bytes = parts.object();
    let list = TreeListV2::open(&bytes).unwrap();
    let ranges: Vec<ListRange> = list.ranges().collect();
    assert_eq!(
        ranges,
        [
            ListRange {
                key: "bc".into(),
                offset: 10,
                len: 5,
            },
            ListRange {
                key: "a".into(),
                offset: 0,
                len: 4,
            },
            ListRange {
                key: "bc".into(),
                offset: 100,
                len: 7,
            },
        ]
    );
    assert!(std::sync::Arc::ptr_eq(&ranges[0].key, &ranges[2].key));
}

/// Asserts that the little-endian readers read two, four and eight bytes at an offset, the least
/// significant first, and nothing around it.
#[test]
fn the_little_endian_readers_read_at_an_offset() {
    let bytes = [
        0xff, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0xff,
    ];
    assert_eq!(le_u16(&bytes, 1), 0x0201);
    assert_eq!(le_u32(&bytes, 1), 0x0403_0201);
    assert_eq!(le_u64(&bytes, 1), 0x0807_0605_0403_0201);
    assert_eq!(le_u16(&bytes, 9), 0xff09);
    assert_eq!(le_u64(&[0xff; 8], 0), u64::MAX);
}

/// Asserts that every section prints the word the errors use, and that the errors of an entry, of
/// entries no directory holds and of a tree over a limit print as the reference says.
#[test]
fn every_section_and_error_prints() {
    for (_, section, name) in SECTION_NAMES {
        assert_eq!(section.to_string(), name);
    }
    let entry = |fault| TreeListV2Error::Entry { index: 7, fault };
    let faults = [
        (
            EntryFault::Root,
            "the root is not a directory with no name and parent 0",
        ),
        (EntryFault::UnknownKind(9), "unknown kind 9"),
        (
            EntryFault::Unreachable,
            "a directory before the children of the directories before it",
        ),
        (
            EntryFault::Children,
            "children not where the breadth-first order puts them",
        ),
        (
            EntryFault::Parent(3),
            "parent 3 is not the directory that holds it",
        ),
        (EntryFault::NameBounds, "name past the end of the names"),
        (EntryFault::BadName, "bad name"),
        (EntryFault::NotAscending, "name not after its sibling's"),
        (EntryFault::PathTooLong, "path over 4096 bytes"),
        (EntryFault::Key(4), "no key 4"),
        (EntryFault::RangeOverflow, "range overflow"),
        (
            EntryFault::EmptyDigest,
            "a file of size 0 has the digest of no bytes",
        ),
        (EntryFault::Target, "bad target"),
    ];
    for (fault, text) in faults {
        assert_eq!(fault.to_string(), text);
        assert_eq!(
            entry(fault).to_string(),
            format!("tree list entry 7: {text}")
        );
    }
    assert_eq!(
        TreeListV2Error::Orphans(90).to_string(),
        "tree list entries from 90 on are in no directory"
    );
    assert_eq!(
        TreeListV2Error::Tree(RecordError::TooManyEntries(5)).to_string(),
        "the tree is refused: tree too large: more than 5 entries"
    );
}
