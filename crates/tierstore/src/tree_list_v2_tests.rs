//! The version 2 tree list: the reader's open and every fault it gives, the strings and limits it
//! judges, the messages of its errors, and the bytes the writer makes and the tree they read as.

use crate::tree_list::{MAX_TARGET_BYTES, MAX_TREE_LIST_BYTES};
use crate::tree_list_v2::{
    HEADER_LEN, HeaderFault, KIND_DIR, KIND_FILE, MAX_KEY_BYTES, NONE, RECORD_LEN, RecordFault,
    Section, StringFault, TREE_LIST_V2_MAGIC, Table, TreeListV2, TreeListV2Error, is_tree_list_v2,
    le_u64, read_u32, valid_key, valid_name, valid_target,
};
use crate::tree_record::RecordError;

// ---------------------------------------------------------------------------------------------
// Hand-built objects
// ---------------------------------------------------------------------------------------------

/// A string table of `strings`: the count, the ends, the bytes.
fn table(strings: &[&[u8]]) -> Vec<u8> {
    let mut ends = Vec::new();
    let mut data = Vec::new();
    for string in strings {
        data.extend_from_slice(string);
        ends.push(data.len() as u32);
    }
    table_raw(&ends, &data)
}

/// A string table with the given `ends` as written and the given bytes.
fn table_raw(ends: &[u32], data: &[u8]) -> Vec<u8> {
    let mut out = (ends.len() as u32).to_le_bytes().to_vec();
    for end in ends {
        out.extend_from_slice(&end.to_le_bytes());
    }
    out.extend_from_slice(data);
    out
}

/// A record of 64 bytes.
fn rec(kind: u8, mode: u16, name: u32, parent: u32, key: u32, size: u64, offset: u64) -> Vec<u8> {
    let mut out = vec![kind, 0];
    out.extend_from_slice(&mode.to_le_bytes());
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&parent.to_le_bytes());
    out.extend_from_slice(&key.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&[9; 32]);
    out
}

/// The parts of an object; each test changes one.
struct Parts {
    version: u32,
    header_len: u32,
    record_len: u32,
    /// The record count; the number of records in `records` when none.
    count: Option<u32>,
    /// The key, name and target tables.
    tables: [Vec<u8>; 3],
    records: Vec<u8>,
}

impl Parts {
    /// A good list: one directory `d` holding one file `f` of key `k`.
    fn good() -> Self {
        let mut records = rec(KIND_DIR, 0o755, 0, NONE, NONE, 0, 0);
        records.extend(rec(KIND_FILE, 0o644, 1, 0, 0, 5, 0));
        Self {
            version: 2,
            header_len: HEADER_LEN,
            record_len: RECORD_LEN,
            count: None,
            tables: [
                table(&[b"k"]),
                table(
                    &[b"f", b"d"].map(|s| s as &[u8])[..]
                        .iter()
                        .rev()
                        .copied()
                        .collect::<Vec<_>>(),
                ),
                table(&[]),
            ],
            records,
        }
    }

    /// An object with no strings and no records.
    fn empty() -> Self {
        Self {
            records: Vec::new(),
            tables: [table(&[]), table(&[]), table(&[])],
            ..Self::good()
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let [keys, names, targets] = &self.tables;
        let keys_at = self.header_len;
        let names_at = keys_at + keys.len() as u32;
        let targets_at = names_at + names.len() as u32;
        let records_at = targets_at + targets.len() as u32;
        let count = self
            .count
            .unwrap_or((self.records.len() / self.record_len.max(1) as usize) as u32);
        let mut out = TREE_LIST_V2_MAGIC.to_vec();
        for field in [
            self.version,
            self.header_len,
            self.record_len,
            count,
            keys_at,
            names_at,
            targets_at,
            records_at,
        ] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        out.resize(self.header_len as usize, 0);
        out.extend_from_slice(keys);
        out.extend_from_slice(names);
        out.extend_from_slice(targets);
        out.extend_from_slice(&self.records);
        out
    }
}

/// Overwrites the `u32` at `at` of `bytes`.
fn patch(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// The error opening `bytes` gives.
fn open_err(bytes: &[u8]) -> TreeListV2Error {
    TreeListV2::open(bytes).unwrap_err()
}

fn section(table: Table) -> TreeListV2Error {
    TreeListV2Error::Header(HeaderFault::Section(Section::Table(table)))
}

fn key_fault(index: u32, fault: StringFault) -> TreeListV2Error {
    TreeListV2Error::Table {
        table: Table::Keys,
        index,
        fault,
    }
}

// ---------------------------------------------------------------------------------------------
// is_tree_list_v2, open, len
// ---------------------------------------------------------------------------------------------

/// Asserts that the magic tells a version 2 list from a version 1 list and from anything shorter.
#[test]
fn the_magic_tells_version_2_from_other_bytes() {
    assert_eq!(&TREE_LIST_V2_MAGIC, b"TSLIST\r\n");
    assert!(is_tree_list_v2(b"TSLIST\r\n"));
    assert!(is_tree_list_v2(&Parts::good().bytes()));
    assert!(!is_tree_list_v2(b"TSLIST\r"));
    assert!(!is_tree_list_v2(b"{\"format\":\"tierstore.tree-list\"}\n"));
    assert!(!is_tree_list_v2(b""));
}

/// Asserts that a good object opens, `len` is its record count and `is_empty` is false.
#[test]
fn a_good_list_opens_with_its_record_count() {
    let bytes = Parts::good().bytes();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.len(), 2);
    assert!(!list.is_empty());
    assert_eq!(&*list.keys[0], "k");
}

/// Asserts that a list with no records is empty and opens with empty tables.
#[test]
fn a_list_with_no_records_is_empty() {
    let bytes = Parts::empty().bytes();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.len(), 0);
    assert!(list.is_empty());
    assert!(list.keys.is_empty());
}

/// Asserts that an object of exactly the largest size gets past the size check (and is then
/// refused for its magic), and one byte more is `TooLarge` with its size and the bound.
#[test]
fn an_object_over_the_largest_size_is_too_large() {
    let at_most = vec![0u8; MAX_TREE_LIST_BYTES];
    assert_eq!(open_err(&at_most), TreeListV2Error::BadMagic);
    let over = vec![0u8; MAX_TREE_LIST_BYTES + 1];
    assert_eq!(
        open_err(&over),
        TreeListV2Error::TooLarge {
            size: MAX_TREE_LIST_BYTES + 1,
            max: MAX_TREE_LIST_BYTES,
        }
    );
}

/// Asserts that bytes that do not begin with the magic are `BadMagic`, a version 1 list included.
#[test]
fn bytes_without_the_magic_are_bad_magic() {
    assert_eq!(open_err(b""), TreeListV2Error::BadMagic);
    assert_eq!(open_err(b"TSLIST\r"), TreeListV2Error::BadMagic);
    assert_eq!(
        open_err(b"{\"format\":\"tierstore.tree-list\",\"version\":1}\n"),
        TreeListV2Error::BadMagic
    );
    let mut bytes = Parts::good().bytes();
    bytes[7] = b'\r';
    assert_eq!(open_err(&bytes), TreeListV2Error::BadMagic);
}

/// Asserts that a version other than 2 is refused with that version, whatever follows.
#[test]
fn a_version_other_than_2_is_unsupported() {
    for version in [0, 1, 3, u32::MAX] {
        let bytes = Parts {
            version,
            ..Parts::good()
        }
        .bytes();
        assert_eq!(
            open_err(&bytes),
            TreeListV2Error::UnsupportedVersion(version)
        );
    }
    // The version is read first: a magic and a version alone give it, not a truncated header.
    let mut short = TREE_LIST_V2_MAGIC.to_vec();
    short.extend_from_slice(&7u32.to_le_bytes());
    assert_eq!(open_err(&short), TreeListV2Error::UnsupportedVersion(7));
}

/// Asserts that an object cut anywhere between its magic and the end of its 40-byte header is a
/// truncated header, whichever field it cuts.
#[test]
fn a_cut_header_is_truncated() {
    let bytes = Parts::good().bytes();
    for len in 8..12 {
        assert_eq!(
            open_err(&bytes[..len]),
            TreeListV2Error::Header(HeaderFault::Truncated),
            "length {len}"
        );
    }
    for len in 12..40 {
        let error = open_err(&bytes[..len]);
        assert_eq!(
            error,
            TreeListV2Error::Header(HeaderFault::Truncated),
            "length {len}"
        );
    }
}

/// Asserts that a header length below 40 is refused with that length, and 40 is taken.
#[test]
fn a_header_length_below_40_is_refused() {
    for header_len in [0u32, 8, 39] {
        let mut bytes = Parts::good().bytes();
        patch(&mut bytes, 12, header_len);
        assert_eq!(
            open_err(&bytes),
            TreeListV2Error::Header(HeaderFault::HeaderLength(header_len))
        );
    }
}

/// Asserts that a header length past the end of the object is refused with that length, and one
/// that reaches exactly the end is taken.
#[test]
fn a_header_length_past_the_end_is_refused() {
    let mut bytes = Parts::good().bytes();
    let len = bytes.len() as u32;
    patch(&mut bytes, 12, len + 1);
    assert_eq!(
        open_err(&bytes),
        TreeListV2Error::Header(HeaderFault::HeaderLength(len + 1))
    );
    patch(&mut bytes, 12, u32::MAX);
    assert_eq!(
        open_err(&bytes),
        TreeListV2Error::Header(HeaderFault::HeaderLength(u32::MAX))
    );
    patch(&mut bytes, 12, len);
    TreeListV2::open(&bytes).unwrap();
}

/// Asserts that a header length is judged before a record length: both bad gives the header's.
#[test]
fn the_header_length_is_judged_before_the_record_length() {
    let mut bytes = Parts::good().bytes();
    patch(&mut bytes, 12, 3);
    patch(&mut bytes, 16, 3);
    assert_eq!(
        open_err(&bytes),
        TreeListV2Error::Header(HeaderFault::HeaderLength(3))
    );
}

/// Asserts that a record length below 64 is refused with that length.
#[test]
fn a_record_length_below_64_is_refused() {
    for record_len in [0u32, 1, 63] {
        let bytes = Parts {
            record_len,
            count: Some(0),
            ..Parts::empty()
        }
        .bytes();
        assert_eq!(
            open_err(&bytes),
            TreeListV2Error::Header(HeaderFault::RecordLength(record_len))
        );
    }
}

/// Asserts that a header and records longer than the least are taken, and each record is read at
/// the longer stride.
#[test]
fn a_longer_header_and_longer_records_are_taken() {
    let mut records = rec(KIND_DIR, 0o755, 0, NONE, NONE, 0, 0);
    records.extend_from_slice(&[0xee; 8]);
    records.extend(rec(KIND_DIR, 0o755, 0, 0, NONE, 0, 0));
    records.extend_from_slice(&[0xee; 8]);
    let bytes = Parts {
        header_len: 48,
        record_len: 72,
        records,
        ..Parts::good()
    }
    .bytes();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list.record_len, 72);
    assert_eq!(list.records.len(), 144);
}

/// Asserts that a record count whose records do not fit in the object is a records section past
/// the end, one record more than fits included, and a count that fits exactly is taken.
#[test]
fn records_past_the_end_are_refused() {
    let past = TreeListV2Error::Header(HeaderFault::Section(Section::Records));
    let fits = Parts::good().bytes();
    let mut bytes = fits.clone();
    patch(&mut bytes, 20, 3);
    assert_eq!(open_err(&bytes), past);
    patch(&mut bytes, 20, u32::MAX);
    assert_eq!(open_err(&bytes), past);
    patch(&mut bytes, 20, 2);
    TreeListV2::open(&bytes).unwrap();
    // The records begin past the end, or so late that the last does not fit.
    let mut late = fits.clone();
    patch(&mut late, 36, fits.len() as u32 + 1);
    assert_eq!(open_err(&late), past);
    patch(&mut late, 36, u32::MAX);
    assert_eq!(open_err(&late), past);
    patch(&mut late, 36, fits.len() as u32 - 127);
    assert_eq!(open_err(&late), past);
    // A record length that makes the product huge.
    let mut huge = fits;
    patch(&mut huge, 16, u32::MAX);
    patch(&mut huge, 20, u32::MAX);
    assert_eq!(open_err(&huge), past);
}

/// Asserts that, for each of the three tables, an offset past the end, a count whose ends do not
/// fit and a last end past the end of the object are that table's section past the end.
#[test]
fn a_table_that_does_not_fit_is_a_section_past_the_end() {
    for (n, which) in [Table::Keys, Table::Names, Table::Targets]
        .into_iter()
        .enumerate()
    {
        let good = Parts::good().bytes();
        // The offset of the table is past the end, or its count is cut by the end.
        let mut bytes = good.clone();
        patch(&mut bytes, 24 + n * 4, good.len() as u32);
        assert_eq!(
            open_err(&bytes),
            section(which),
            "{which:?} offset at the end"
        );
        patch(&mut bytes, 24 + n * 4, good.len() as u32 - 3);
        assert_eq!(open_err(&bytes), section(which), "{which:?} count cut");
        patch(&mut bytes, 24 + n * 4, u32::MAX);
        assert_eq!(open_err(&bytes), section(which), "{which:?} offset huge");

        // The count is more than the object has ends for.
        let mut parts = Parts::good();
        parts.tables[n] = (0x4000_0000u32).to_le_bytes().to_vec();
        assert_eq!(
            open_err(&parts.bytes()),
            section(which),
            "{which:?} count huge"
        );

        // The ends fit but the last end is past the bytes the object has.
        let mut parts = Parts::good();
        parts.tables[n] = table_raw(&[100_000], b"ab");
        assert_eq!(
            open_err(&parts.bytes()),
            section(which),
            "{which:?} data short"
        );
    }
}

/// Asserts that tables are judged in order keys, names, targets, and all three before the
/// records are: with every section past the end, the keys are named first.
#[test]
fn the_sections_are_judged_in_order() {
    let mut bytes = Parts::good().bytes();
    patch(&mut bytes, 36, u32::MAX);
    patch(&mut bytes, 24, u32::MAX);
    patch(&mut bytes, 28, u32::MAX);
    patch(&mut bytes, 32, u32::MAX);
    // The records are judged before the tables.
    assert_eq!(
        open_err(&bytes),
        TreeListV2Error::Header(HeaderFault::Section(Section::Records))
    );
    patch(&mut bytes, 36, 40);
    assert_eq!(open_err(&bytes), section(Table::Keys));
    patch(&mut bytes, 24, 40);
    assert_eq!(open_err(&bytes), section(Table::Names));
    patch(&mut bytes, 28, 40);
    assert_eq!(open_err(&bytes), section(Table::Targets));
}

/// Asserts that a key table whose end is below the one before it is a bounds fault at that key.
#[test]
fn a_key_end_below_the_end_before_is_a_bounds_fault() {
    let mut parts = Parts::empty();
    parts.tables[0] = table_raw(&[2, 1, 5], b"abcde");
    assert_eq!(open_err(&parts.bytes()), key_fault(1, StringFault::Bounds));
}

/// Asserts that a key end past the bytes of its table, when it is not the last end, is a bounds
/// fault at that key.
#[test]
fn a_key_end_past_the_data_is_a_bounds_fault() {
    let mut parts = Parts::empty();
    parts.tables[0] = table_raw(&[9, 3], b"abc");
    assert_eq!(open_err(&parts.bytes()), key_fault(0, StringFault::Bounds));
}

/// Asserts that a key that is not UTF-8 is refused at its index.
#[test]
fn a_key_that_is_not_utf8_is_refused() {
    let mut parts = Parts::empty();
    parts.tables[0] = table(&[b"a", b"\xff\xfe"]);
    assert_eq!(open_err(&parts.bytes()), key_fault(1, StringFault::NotUtf8));
}

/// Asserts that keys not strictly ascending are refused at the first that is not after the one
/// before: lower and equal neighbours both.
#[test]
fn keys_not_strictly_ascending_are_refused() {
    let mut parts = Parts::empty();
    parts.tables[0] = table(&[b"b", b"a"]);
    assert_eq!(
        open_err(&parts.bytes()),
        key_fault(1, StringFault::NotAscending)
    );
    parts.tables[0] = table(&[b"a", b"b", b"b"]);
    assert_eq!(
        open_err(&parts.bytes()),
        key_fault(2, StringFault::NotAscending)
    );
    // A prefix sorts before what extends it.
    parts.tables[0] = table(&[b"ab", b"a"]);
    assert_eq!(
        open_err(&parts.bytes()),
        key_fault(1, StringFault::NotAscending)
    );
    parts.tables[0] = table(&[b"a", b"ab"]);
    TreeListV2::open(&parts.bytes()).unwrap();
}

/// Asserts that an empty key is a bad key, and so is one byte over the longest, while one of
/// exactly the longest is taken.
#[test]
fn an_empty_or_overlong_key_is_a_bad_key() {
    let mut parts = Parts::empty();
    parts.tables[0] = table(&[b""]);
    assert_eq!(open_err(&parts.bytes()), key_fault(0, StringFault::BadKey));
    parts.tables[0] = table(&[b"a", &vec![b'z'; MAX_KEY_BYTES + 1]]);
    assert_eq!(open_err(&parts.bytes()), key_fault(1, StringFault::BadKey));
    let longest = vec![b'z'; MAX_KEY_BYTES];
    parts.tables[0] = table(&[b"a", &longest]);
    let bytes = parts.bytes();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.keys.len(), 2);
    assert_eq!(list.keys[1].len(), MAX_KEY_BYTES);
}

/// Asserts that a key fault is judged in the order bounds, UTF-8, ascending, key: a key that is
/// out of order and bad is out of order.
#[test]
fn a_string_is_judged_for_utf8_then_order_then_its_own_rule() {
    let mut parts = Parts::empty();
    parts.tables[0] = table(&[b"b", b""]);
    assert_eq!(
        open_err(&parts.bytes()),
        key_fault(1, StringFault::NotAscending)
    );
    parts.tables[0] = table(&[b"b", b"\xff"]);
    assert_eq!(open_err(&parts.bytes()), key_fault(1, StringFault::NotUtf8));
}

/// Asserts that the names and targets are not judged at open (their faults are the walk's), only
/// that they lie within the object.
#[test]
fn open_does_not_judge_the_name_and_target_strings() {
    let mut parts = Parts::empty();
    parts.tables[1] = table(&[b"b", b"a", b"/"]);
    parts.tables[2] = table(&[b"", b"\xff"]);
    let bytes = parts.bytes();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.names.len(), 3);
    assert_eq!(list.targets.len(), 2);
}

// ---------------------------------------------------------------------------------------------
// The string rules and readers
// ---------------------------------------------------------------------------------------------

/// Asserts that a key is 1 to 1024 bytes.
#[test]
fn a_key_is_one_to_the_longest_bytes() {
    assert!(!valid_key(""));
    assert!(valid_key("k"));
    assert!(valid_key(&"k".repeat(MAX_KEY_BYTES)));
    assert!(!valid_key(&"k".repeat(MAX_KEY_BYTES + 1)));
    assert_eq!(MAX_KEY_BYTES, 1024);
}

/// Asserts that a name is refused when it is empty, `.`, `..`, over 255 bytes, has a `/` or a
/// NUL or begins `.wh.`, and taken at 255 bytes.
#[test]
fn a_name_follows_the_name_rules() {
    assert!(valid_name("a"));
    assert!(valid_name(&"n".repeat(255)));
    assert!(!valid_name(&"n".repeat(256)));
    for bad in ["", ".", "..", "a/b", "a\0b", ".wh.x"] {
        assert!(!valid_name(bad), "{bad:?}");
    }
    assert!(valid_name(".hidden"));
}

/// Asserts that a target is 1 to 4096 bytes with no NUL.
#[test]
fn a_target_is_one_to_the_longest_bytes_with_no_nul() {
    assert!(!valid_target(""));
    assert!(valid_target("t"));
    assert!(valid_target(&"t".repeat(MAX_TARGET_BYTES)));
    assert!(!valid_target(&"t".repeat(MAX_TARGET_BYTES + 1)));
    assert!(!valid_target("a\0b"));
    assert!(valid_target("../a/b"));
}

/// Asserts that `read_u32` reads little-endian, gives nothing past the end, and gives nothing
/// rather than overflowing for an offset near the largest.
#[test]
fn read_u32_stays_inside_its_bytes() {
    let bytes = [1, 0, 0, 0, 2, 0, 0, 0];
    assert_eq!(read_u32(&bytes, 0), Some(1));
    assert_eq!(read_u32(&bytes, 4), Some(2));
    assert_eq!(read_u32(&bytes, 5), None);
    assert_eq!(read_u32(&bytes, 8), None);
    assert_eq!(read_u32(&bytes, usize::MAX), None);
    assert_eq!(read_u32(&[0x78, 0x56, 0x34, 0x12], 0), Some(0x1234_5678));
}

/// Asserts that `le_u64` reads the first eight bytes little-endian.
#[test]
fn le_u64_reads_eight_bytes() {
    assert_eq!(le_u64(&[1, 2, 3, 4, 5, 6, 7, 8, 9]), 0x0807_0605_0403_0201);
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

/// Asserts the exact message of every `TreeListV2Error`, and of the header, string and record
/// faults, tables and sections inside them.
#[test]
fn every_error_says_what_is_wrong() {
    let cases: Vec<(TreeListV2Error, &str)> = vec![
        (
            TreeListV2Error::TooLarge { size: 9, max: 5 },
            "tree list too large: 9 bytes, at most 5",
        ),
        (
            TreeListV2Error::BadMagic,
            "not a version 2 tree list: bad magic",
        ),
        (
            TreeListV2Error::UnsupportedVersion(3),
            "unsupported tree list version 3",
        ),
        (
            TreeListV2Error::Header(HeaderFault::Truncated),
            "tree list header: truncated",
        ),
        (
            TreeListV2Error::Header(HeaderFault::HeaderLength(7)),
            "tree list header: bad header length 7",
        ),
        (
            TreeListV2Error::Header(HeaderFault::RecordLength(9)),
            "tree list header: bad record length 9",
        ),
        (
            TreeListV2Error::Header(HeaderFault::Section(Section::Table(Table::Keys))),
            "tree list header: the key section is past the end",
        ),
        (
            TreeListV2Error::Header(HeaderFault::Section(Section::Table(Table::Names))),
            "tree list header: the name section is past the end",
        ),
        (
            TreeListV2Error::Header(HeaderFault::Section(Section::Table(Table::Targets))),
            "tree list header: the target section is past the end",
        ),
        (
            TreeListV2Error::Header(HeaderFault::Section(Section::Records)),
            "tree list header: the record section is past the end",
        ),
        (
            TreeListV2Error::Table {
                table: Table::Names,
                index: 3,
                fault: StringFault::NotAscending,
            },
            "tree list name 3: not strictly ascending",
        ),
        (
            TreeListV2Error::Table {
                table: Table::Targets,
                index: 0,
                fault: StringFault::BadTarget,
            },
            "tree list target 0: bad target",
        ),
        (
            TreeListV2Error::Record {
                index: 7,
                fault: RecordFault::Parent(9),
            },
            "tree list record 7: parent 9 is not a directory before it",
        ),
        (
            TreeListV2Error::Tree(RecordError::TooManyEntries(4)),
            "the tree is refused: tree too large: more than 4 entries",
        ),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message);
    }
}

/// Asserts the exact message of every string fault.
#[test]
fn every_string_fault_says_what_is_wrong() {
    let cases = [
        (StringFault::Bounds, "past the end of its table"),
        (StringFault::NotUtf8, "not UTF-8"),
        (StringFault::NotAscending, "not strictly ascending"),
        (StringFault::BadKey, "bad key"),
        (StringFault::BadName, "bad name"),
        (StringFault::BadTarget, "bad target"),
    ];
    for (fault, message) in cases {
        assert_eq!(fault.to_string(), message);
    }
}

/// Asserts the exact message of every record fault.
#[test]
fn every_record_fault_says_what_is_wrong() {
    let cases = [
        (RecordFault::UnknownKind(9), "unknown kind 9"),
        (
            RecordFault::KindOrder,
            "out of order: the directories come first, then the links, then the files",
        ),
        (RecordFault::BadMode(0o17777), "bad mode 17777"),
        (RecordFault::Name(4), "no name 4"),
        (
            RecordFault::Parent(5),
            "parent 5 is not a directory before it",
        ),
        (RecordFault::Key(6), "no key 6"),
        (RecordFault::Target(8), "no target 8"),
        (
            RecordFault::KeyAndSize,
            "a file of size 0 has no key, and a larger one has one",
        ),
        (RecordFault::RangeOverflow, "range overflow"),
        (RecordFault::Unsorted, "files not sorted by key and offset"),
        (
            RecordFault::EmptyDigest,
            "a file of size 0 has the digest of no bytes",
        ),
    ];
    for (fault, message) in cases {
        assert_eq!(fault.to_string(), message);
    }
}

/// Asserts that a table and a section print as their singular noun.
#[test]
fn tables_and_sections_print_their_noun() {
    assert_eq!(Table::Keys.to_string(), "key");
    assert_eq!(Table::Names.to_string(), "name");
    assert_eq!(Table::Targets.to_string(), "target");
    assert_eq!(Section::Table(Table::Names).to_string(), "name");
    assert_eq!(Section::Records.to_string(), "record");
}

mod encode;
