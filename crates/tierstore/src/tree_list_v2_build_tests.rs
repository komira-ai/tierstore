//! The walk of a version 2 tree list: every refusal of a record, of the tree and of the name and
//! target tables, the spans handed out, the tree built (equal to a version 1 list's), and hostile
//! bytes.

use std::collections::HashMap;
use std::sync::Arc;

use crate::tree_list::{
    EMPTY_SHA256, ListEntry, ListKind, MAX_NAME_BYTES, MAX_TARGET_BYTES, TreeList,
};
use crate::tree_list_v2::{
    HeaderFault, KIND_DIR, KIND_FILE, KIND_SYMLINK, NONE, RecordFault, Section, StringFault, Table,
    TreeListV2, TreeListV2Error,
};
use crate::tree_list_v2_build::ListSpan;
use crate::tree_record::{BuiltTree, RecordError};
use crate::{BlobSource, Mode, TreeDigest, TreeLimits, build_tree};

// ----- helpers -----

fn sha(byte: u8) -> TreeDigest {
    TreeDigest::from_bytes([byte; 32])
}

fn mode(bits: u16) -> Mode {
    Mode::new(bits).unwrap()
}

fn file(
    path: &str,
    size: u64,
    source: Option<(&str, u64)>,
    digest: Option<TreeDigest>,
) -> ListEntry {
    ListEntry {
        path: path.into(),
        kind: ListKind::File {
            mode: mode(0o644),
            size,
            source: source.map(|(key, offset)| BlobSource {
                key: key.into(),
                offset,
            }),
            sha256: digest,
        },
    }
}

fn file_mode(path: &str, bits: u16, size: u64, source: (&str, u64), digest: u8) -> ListEntry {
    let mut entry = file(path, size, Some(source), Some(sha(digest)));
    if let ListKind::File { mode: m, .. } = &mut entry.kind {
        *m = mode(bits);
    }
    entry
}

fn dir(path: &str, bits: u16) -> ListEntry {
    ListEntry {
        path: path.into(),
        kind: ListKind::Dir { mode: mode(bits) },
    }
}

fn link(path: &str, target: &str) -> ListEntry {
    ListEntry {
        path: path.into(),
        kind: ListKind::Symlink {
            target: target.into(),
        },
    }
}

/// Eleven entries: nested directories, an explicit one, links, files of shared digests and files
/// of size 0. The records are, in order: directories `b`, `b/e`, `empty` (0 to 2); links `b/ln2`,
/// `ln` (3, 4); files `a.txt`, `b/c.txt`, `b/d.txt`, `b/e/f` (5 to 8); empty files `b/zz`, `z`
/// (9, 10). The keys are `k1`, `k2`; the targets `../a.txt`, `a.txt`.
fn sample_list() -> TreeList {
    TreeList {
        entries: vec![
            dir("empty", 0o700),
            file("a.txt", 5, Some(("k1", 0)), Some(sha(0xa))),
            file("b/c.txt", 5, Some(("k1", 5)), Some(sha(0xb))),
            file("b/d.txt", 5, Some(("k2", 0)), Some(sha(0xa))),
            file_mode("b/e/f", 0o755, 3, ("k2", 8), 0xc),
            link("ln", "a.txt"),
            link("b/ln2", "../a.txt"),
            file("z", 0, None, None),
            file("b/zz", 0, None, None),
        ],
    }
}

fn sample() -> Vec<u8> {
    sample_list().encode_v2().unwrap()
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// Where record `index` of an `encode_v2` output begins.
fn record_at(bytes: &[u8], index: usize) -> usize {
    u32_at(bytes, 36) as usize + 64 * index
}

/// `bytes` with `with` written at `at`.
fn patched(bytes: &[u8], at: usize, with: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out[at..at + with.len()].copy_from_slice(with);
    out
}

/// `sample()` with `with` written at `field` of record `index`.
fn patched_record(index: usize, field: usize, with: &[u8]) -> Vec<u8> {
    let bytes = sample();
    let at = record_at(&bytes, index) + field;
    patched(&bytes, at, with)
}

const F_KIND: usize = 0;
const F_MODE: usize = 2;
const F_NAME: usize = 4;
const F_PARENT: usize = 8;
const F_KEY: usize = 12;
const F_SIZE: usize = 16;
const F_OFFSET: usize = 24;
const F_SHA: usize = 32;

/// The result of opening and building `bytes` with `limits`, and the spans handed out.
fn run_with(
    bytes: &[u8],
    limits: &TreeLimits,
) -> (Result<BuiltTree, TreeListV2Error>, Vec<ListSpan>) {
    let mut spans = Vec::new();
    let result =
        TreeListV2::open(bytes).and_then(|list| list.build(limits, |span| spans.push(span)));
    (result, spans)
}

fn build(bytes: &[u8]) -> Result<BuiltTree, TreeListV2Error> {
    run_with(bytes, &TreeLimits::default()).0
}

fn build_err(bytes: &[u8]) -> TreeListV2Error {
    build(bytes).unwrap_err()
}

/// Asserts that building `bytes` is refused as `fault` of record `index`.
fn assert_fault(bytes: &[u8], index: u32, fault: RecordFault) {
    assert_eq!(build_err(bytes), record_err(index, fault));
}

/// Asserts that building `bytes` is refused as `fault` of string `index` of `table`.
fn assert_table(bytes: &[u8], table: Table, index: u32, fault: StringFault) {
    assert_eq!(build_err(bytes), table_err(table, index, fault));
}

fn record_err(index: u32, fault: RecordFault) -> TreeListV2Error {
    TreeListV2Error::Record { index, fault }
}

fn table_err(table: Table, index: u32, fault: StringFault) -> TreeListV2Error {
    TreeListV2Error::Table {
        table,
        index,
        fault,
    }
}

/// One record of a hand-made list.
#[derive(Clone, Copy)]
struct Rec {
    kind: u8,
    mode: u16,
    name: u32,
    parent: u32,
    key: u32,
    size: u64,
    offset: u64,
    sha: [u8; 32],
}

impl Rec {
    fn dir(name: u32, parent: u32) -> Self {
        Self {
            kind: KIND_DIR,
            mode: 0o755,
            name,
            parent,
            key: NONE,
            size: 0,
            offset: 0,
            sha: [0; 32],
        }
    }

    fn link(name: u32, parent: u32, target: u32) -> Self {
        Self {
            kind: KIND_SYMLINK,
            mode: 0,
            key: target,
            ..Self::dir(name, parent)
        }
    }

    fn file(name: u32, parent: u32, key: u32, size: u64, offset: u64, sha: u8) -> Self {
        Self {
            kind: KIND_FILE,
            mode: 0o644,
            key,
            size,
            offset,
            sha: [sha; 32],
            ..Self::dir(name, parent)
        }
    }

    fn empty_file(name: u32, parent: u32) -> Self {
        Self {
            sha: EMPTY_SHA256,
            ..Self::file(name, parent, NONE, 0, 0, 0)
        }
    }
}

/// The bytes of a string table of `strings`, in the order given.
fn table<S: AsRef<[u8]>>(strings: &[S]) -> Vec<u8> {
    let mut ends = Vec::new();
    let mut data = Vec::new();
    for text in strings {
        data.extend_from_slice(text.as_ref());
        ends.extend_from_slice(&(data.len() as u32).to_le_bytes());
    }
    let mut out = (strings.len() as u32).to_le_bytes().to_vec();
    out.extend(ends);
    out.extend(data);
    out
}

/// A table of `count` strings with the `ends` and `data` as given, however wrong.
fn table_raw(ends: &[u32], data: &[u8]) -> Vec<u8> {
    let mut out = (ends.len() as u32).to_le_bytes().to_vec();
    for end in ends {
        out.extend_from_slice(&end.to_le_bytes());
    }
    out.extend_from_slice(data);
    out
}

/// A list assembled by hand, its tables as bytes, with room to make the header and the records
/// longer than the writer's.
struct Spec {
    keys: Vec<u8>,
    names: Vec<u8>,
    targets: Vec<u8>,
    records: Vec<Rec>,
    header_extra: usize,
    record_extra: usize,
}

impl Spec {
    fn new(names: &[&str], records: Vec<Rec>) -> Self {
        let strs = |s: &[&str]| s.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>();
        let names = strs(names);
        Self {
            keys: table(&["k1", "k2"]),
            names: table(&names.iter().map(Vec::as_slice).collect::<Vec<_>>()),
            targets: table(&["t1", "t2"]),
            records,
            header_extra: 0,
            record_extra: 0,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let header_len = 40 + self.header_extra;
        let record_len = 64 + self.record_extra;
        let keys_at = header_len;
        let names_at = keys_at + self.keys.len();
        let targets_at = names_at + self.names.len();
        let records_at = targets_at + self.targets.len();
        let mut out = b"TSLIST\r\n".to_vec();
        for field in [
            2,
            header_len,
            record_len,
            self.records.len(),
            keys_at,
            names_at,
            targets_at,
            records_at,
        ] {
            out.extend_from_slice(&(field as u32).to_le_bytes());
        }
        out.resize(header_len, 0xee);
        out.extend_from_slice(&self.keys);
        out.extend_from_slice(&self.names);
        out.extend_from_slice(&self.targets);
        for r in &self.records {
            let mut rec = vec![r.kind, 0];
            rec.extend_from_slice(&r.mode.to_le_bytes());
            for word in [r.name, r.parent, r.key] {
                rec.extend_from_slice(&word.to_le_bytes());
            }
            rec.extend_from_slice(&r.size.to_le_bytes());
            rec.extend_from_slice(&r.offset.to_le_bytes());
            rec.extend_from_slice(&r.sha);
            rec.resize(record_len, 0xee);
            out.extend(rec);
        }
        out
    }
}

/// Names `a`, `b`, `c`, `d`.
fn spec(records: Vec<Rec>) -> Spec {
    Spec::new(&["a", "b", "c", "d"], records)
}

fn limits(max_entries: u64, max_depth: usize, max_dir_entries: usize) -> TreeLimits {
    TreeLimits {
        max_entries,
        max_depth,
        max_dir_entries,
    }
}

// ----- the sample builds -----

/// Asserts that a list from `encode_v2` builds, its spans are those of the first file of each
/// digest in record order, and its files are in record order with full paths and their sources.
#[test]
fn the_sample_builds_with_its_spans_and_files_in_record_order() {
    let bytes = sample();
    let (built, spans) = run_with(&bytes, &TreeLimits::default());
    let built = built.unwrap();
    let key = |k: &str| -> Arc<str> { Arc::from(k) };
    assert_eq!(
        spans,
        vec![
            ListSpan {
                key: key("k1"),
                offset: 0,
                size: 5,
                sha256: sha(0xa)
            },
            ListSpan {
                key: key("k1"),
                offset: 5,
                size: 5,
                sha256: sha(0xb)
            },
            ListSpan {
                key: key("k2"),
                offset: 8,
                size: 3,
                sha256: sha(0xc)
            },
        ]
    );
    let src = |k: &str, offset| {
        Some(BlobSource {
            key: key(k),
            offset,
        })
    };
    let empty = TreeDigest::from_bytes(EMPTY_SHA256);
    let got: Vec<_> = built
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.size, f.sha256, f.source.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("a.txt", 5, sha(0xa), src("k1", 0)),
            ("b/c.txt", 5, sha(0xb), src("k1", 5)),
            ("b/d.txt", 5, sha(0xa), src("k2", 0)),
            ("b/e/f", 3, sha(0xc), src("k2", 8)),
            ("b/zz", 0, empty, None),
            ("z", 0, empty, None),
        ]
    );
}

/// Asserts that the key of a span is the same allocation as the key of the file's source, and
/// that files sharing a key share it.
#[test]
fn a_span_key_is_the_string_of_its_key_and_is_shared_with_the_sources() {
    let bytes = sample();
    let (built, spans) = run_with(&bytes, &TreeLimits::default());
    let built = built.unwrap();
    assert_eq!(&*spans[0].key, "k1");
    let source_key = &built.files[0].source.as_ref().unwrap().key;
    assert!(Arc::ptr_eq(&spans[0].key, source_key));
    assert!(Arc::ptr_eq(
        &built.files[0].source.as_ref().unwrap().key,
        &built.files[1].source.as_ref().unwrap().key
    ));
}

/// Asserts that two files of one digest hand out one span, the first record's, and a file of size
/// 0 hands out none.
#[test]
fn a_digest_is_handed_out_once_and_a_file_of_size_0_never() {
    let list = TreeList {
        entries: vec![
            file("x", 4, Some(("k1", 10)), Some(sha(1))),
            file("y", 4, Some(("k1", 20)), Some(sha(1))),
            file("z", 4, Some(("k2", 0)), Some(sha(1))),
            file("e", 0, None, None),
        ],
    };
    let (built, spans) = run_with(&list.encode_v2().unwrap(), &TreeLimits::default());
    assert_eq!(built.unwrap().files.len(), 4);
    assert_eq!(
        spans,
        vec![ListSpan {
            key: Arc::from("k1"),
            offset: 10,
            size: 4,
            sha256: sha(1)
        }]
    );
}

/// Asserts that spans handed out before a fault stay handed out: a bad record after two files
/// still reports both spans.
#[test]
fn spans_before_a_fault_were_handed_out() {
    let mut records = vec![
        Rec::file(0, NONE, 0, 3, 0, 1),
        Rec::file(1, NONE, 0, 3, 3, 2),
        Rec::file(2, NONE, 0, 3, 6, 3),
    ];
    records[2].mode = 0o10000;
    let bytes = spec(records).bytes();
    let (result, spans) = run_with(&bytes, &TreeLimits::default());
    assert_eq!(
        result.unwrap_err(),
        record_err(2, RecordFault::BadMode(0o10000))
    );
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[1].offset, 3);
}

// ----- the same tree as version 1 -----

/// The tree of a version 2 build and of `build_tree` over the same entries.
fn same_tree(list: TreeList) {
    let bytes = list.encode_v2().unwrap();
    let v2 = build(&bytes).unwrap();
    let v1 = build_tree(None, list.into_entries().unwrap(), &TreeLimits::default()).unwrap();
    assert_eq!(v2.root, v1.root);
    assert_eq!(v2.records.len(), v1.records.len());
    let by_digest = |t: &BuiltTree| {
        t.records
            .iter()
            .map(|(d, r)| (*d, r.clone()))
            .collect::<HashMap<_, _>>()
    };
    assert_eq!(by_digest(&v2), by_digest(&v1));
    let mut v2_files = v2.files;
    v2_files.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(v2_files, v1.files);
}

/// Asserts that an empty list has the root digest and the one record `build_tree` makes.
#[test]
fn an_empty_list_is_the_tree_of_no_entries() {
    same_tree(TreeList::default());
    let built = build(&TreeList::default().encode_v2().unwrap()).unwrap();
    assert_eq!(built.records.len(), 1);
    assert!(built.files.is_empty());
}

/// Asserts that files at the root only make the tree `build_tree` makes.
#[test]
fn root_level_files_are_the_tree_of_build_tree() {
    same_tree(TreeList {
        entries: vec![
            file("b", 2, Some(("k", 0)), Some(sha(2))),
            file("a", 2, Some(("k", 2)), Some(sha(3))),
            file("c", 0, None, None),
        ],
    });
}

/// Asserts that nested directories with explicit modes, links and empty directories make the
/// tree `build_tree` makes.
#[test]
fn nested_directories_modes_and_links_are_the_tree_of_build_tree() {
    same_tree(sample_list());
    same_tree(TreeList {
        entries: vec![
            dir("a", 0o1777),
            dir("a/b", 0o700),
            dir("a/b/c", 0o755),
            dir("empty", 0o755),
            link("a/b/l", "../../x"),
            file_mode("a/b/c/f", 0o4755, 1, ("p", 0), 9),
        ],
    });
}

/// Asserts that directories with the same contents share one record, as in `build_tree`, and that
/// files of one digest at many paths keep every path.
#[test]
fn shared_digests_and_equal_directories_make_the_tree_of_build_tree() {
    let list = TreeList {
        entries: vec![
            file("p/x", 3, Some(("k", 0)), Some(sha(7))),
            file("q/x", 3, Some(("k", 3)), Some(sha(7))),
            file("r/s/x", 3, Some(("k", 6)), Some(sha(7))),
        ],
    };
    same_tree(list.clone());
    let built = build(&list.encode_v2().unwrap()).unwrap();
    // p, q and r/s hold the same file: one record for the three, and the root, and r.
    assert_eq!(built.records.len(), 3);
    assert_eq!(built.files.len(), 3);
}

// ----- longer header and records -----

/// Asserts that a header length above 40 and a record length above 64 are read, the bytes beyond
/// the fields skipped.
#[test]
fn a_longer_header_and_longer_records_are_read() {
    let records = vec![
        Rec::dir(0, NONE),
        Rec::link(1, 0, 1),
        Rec::file(2, 0, 1, 4, 0, 5),
        Rec::empty_file(3, NONE),
    ];
    let plain = spec(records.clone());
    let mut longer = spec(records);
    longer.header_extra = 24;
    longer.record_extra = 17;
    let (a, b) = (
        build(&plain.bytes()).unwrap(),
        build(&longer.bytes()).unwrap(),
    );
    assert_eq!(a, b);
    assert_eq!(a.files.len(), 2);
}

// ----- record faults -----

/// Asserts that kind 0 and kinds above 3 are refused with the record's number.
#[test]
fn an_unknown_kind_is_refused() {
    for kind in [0u8, 4, 255] {
        assert_fault(
            &patched_record(2, F_KIND, &[kind]),
            2,
            RecordFault::UnknownKind(kind),
        );
    }
}

/// Asserts that a directory after a link, a link after a file and a directory after a file are
/// refused as out of order.
#[test]
fn a_kind_before_the_kind_before_it_is_refused() {
    // Record 5 is the first file, after a link.
    assert_fault(
        &patched_record(5, F_KIND, &[KIND_DIR]),
        5,
        RecordFault::KindOrder,
    );
    // Record 6 is a file after a file.
    assert_fault(
        &patched_record(6, F_KIND, &[KIND_SYMLINK]),
        6,
        RecordFault::KindOrder,
    );
    assert_fault(
        &patched_record(6, F_KIND, &[KIND_DIR]),
        6,
        RecordFault::KindOrder,
    );
}

/// Asserts that a mode with a bit above 07777 is refused in a directory and in a file, and 07777
/// is taken; the mode of a link is not read.
#[test]
fn a_mode_above_07777_is_refused_in_a_directory_and_a_file() {
    assert_fault(
        &patched_record(0, F_MODE, &0o10000u16.to_le_bytes()),
        0,
        RecordFault::BadMode(0o10000),
    );
    assert_fault(
        &patched_record(5, F_MODE, &0xffffu16.to_le_bytes()),
        5,
        RecordFault::BadMode(0xffff),
    );
    build(&patched_record(5, F_MODE, &0o7777u16.to_le_bytes())).unwrap();
    build(&patched_record(3, F_MODE, &0xffffu16.to_le_bytes())).unwrap();
}

/// Asserts that a name number at or past the name table is refused in each kind, and the last
/// name is taken.
#[test]
fn a_name_out_of_range_is_refused() {
    let bytes = sample();
    let names = u32_at(&bytes, 28) as usize;
    let count = u32_at(&bytes, names);
    for record in [0, 3, 5] {
        for name in [count, count + 1, NONE] {
            assert_fault(
                &patched_record(record, F_NAME, &name.to_le_bytes()),
                record as u32,
                RecordFault::Name(name),
            );
        }
    }
    assert_eq!(count, 11);
}

/// Asserts that a parent that is the record itself, a later directory, a link record's number or
/// a file record's number is refused, and NONE and an earlier directory are taken.
#[test]
fn a_parent_that_is_not_a_directory_before_the_record_is_refused() {
    let set = |index, parent: u32| patched_record(index, F_PARENT, &parent.to_le_bytes());
    // The directory itself, and a later directory.
    assert_fault(&set(0, 0), 0, RecordFault::Parent(0));
    assert_fault(&set(1, 2), 1, RecordFault::Parent(2));
    // The file after the three directories names a link, a file and a record past the end.
    assert_fault(&set(5, 3), 5, RecordFault::Parent(3));
    assert_fault(&set(5, 7), 5, RecordFault::Parent(7));
    assert_fault(&set(5, 99), 5, RecordFault::Parent(99));
    // A link names a link.
    assert_fault(&set(4, 3), 4, RecordFault::Parent(3));
    // Directory 1 may be a child of directory 0, and a file of directory 2.
    build(&set(1, 0)).unwrap();
    build(&set(5, 2)).unwrap();
    build(&set(5, NONE)).unwrap();
}

/// Asserts that a key number at or past the key table is refused.
#[test]
fn a_key_out_of_range_is_refused() {
    for key in [2u32, 3, NONE - 1] {
        assert_fault(
            &patched_record(5, F_KEY, &key.to_le_bytes()),
            5,
            RecordFault::Key(key),
        );
    }
}

/// Asserts that a target number at or past the target table is refused, and the last is taken.
#[test]
fn a_target_out_of_range_is_refused() {
    for target in [2u32, 5, NONE] {
        assert_fault(
            &patched_record(3, F_KEY, &target.to_le_bytes()),
            3,
            RecordFault::Target(target),
        );
    }
    build(&patched_record(3, F_KEY, &1u32.to_le_bytes())).unwrap();
}

/// Asserts that a file with no key and a size above 0 is refused.
#[test]
fn a_file_with_no_key_and_bytes_is_refused() {
    assert_fault(
        &patched_record(5, F_KEY, &NONE.to_le_bytes()),
        5,
        RecordFault::KeyAndSize,
    );
}

/// Asserts that a file with a key and a size of 0 is refused.
#[test]
fn a_file_with_a_key_and_no_bytes_is_refused() {
    assert_fault(
        &patched_record(5, F_SIZE, &0u64.to_le_bytes()),
        5,
        RecordFault::KeyAndSize,
    );
}

/// Asserts that an offset and a size whose sum passes 64 bits are refused, and a sum of exactly
/// 2^64 - 1 is taken.
#[test]
fn a_range_that_overflows_is_refused() {
    let at_edge = u64::MAX - 5;
    assert_fault(
        &patched_record(5, F_OFFSET, &(at_edge + 1).to_le_bytes()),
        5,
        RecordFault::RangeOverflow,
    );
    // The records after it are out of order; only this one is judged here.
    let edge = patched_record(5, F_OFFSET, &at_edge.to_le_bytes());
    assert_fault(&edge, 6, RecordFault::Unsorted);
}

/// Asserts that a file whose offset is below the offset of the file before it in one key is
/// refused.
#[test]
fn an_offset_going_down_in_one_key_is_refused() {
    assert_fault(
        &patched_record(5, F_OFFSET, &9u64.to_le_bytes()),
        6,
        RecordFault::Unsorted,
    );
}

/// Asserts that a key going down is refused, and equal places (two files at one key and offset)
/// are taken.
#[test]
fn a_key_going_down_is_refused() {
    assert_fault(
        &patched_record(8, F_KEY, &0u32.to_le_bytes()),
        8,
        RecordFault::Unsorted,
    );
    // b/c.txt at the place of a.txt.
    build(&patched_record(6, F_OFFSET, &0u64.to_le_bytes())).unwrap();
}

/// Asserts that a file of size 0 with a digest other than the SHA-256 of no bytes is refused.
#[test]
fn an_empty_file_with_another_digest_is_refused() {
    for at in [0usize, 31] {
        let bytes = sample();
        let sha_at = record_at(&bytes, 9) + F_SHA + at;
        let bad = patched(&bytes, sha_at, &[bytes[sha_at] ^ 1]);
        assert_fault(&bad, 9, RecordFault::EmptyDigest);
    }
}

/// Asserts that every record fault has its message, in the message of the list's error.
#[test]
fn a_record_fault_says_what_is_wrong() {
    let cases = [
        (
            RecordFault::UnknownKind(9),
            "tree list record 3: unknown kind 9",
        ),
        (
            RecordFault::KindOrder,
            "tree list record 3: out of order: the directories come first, then the links, then the files",
        ),
        (
            RecordFault::BadMode(0o10000),
            "tree list record 3: bad mode 10000",
        ),
        (RecordFault::Name(7), "tree list record 3: no name 7"),
        (
            RecordFault::Parent(4),
            "tree list record 3: parent 4 is not a directory before it",
        ),
        (RecordFault::Key(2), "tree list record 3: no key 2"),
        (RecordFault::Target(5), "tree list record 3: no target 5"),
        (
            RecordFault::KeyAndSize,
            "tree list record 3: a file of size 0 has no key, and a larger one has one",
        ),
        (
            RecordFault::RangeOverflow,
            "tree list record 3: range overflow",
        ),
        (
            RecordFault::Unsorted,
            "tree list record 3: files not sorted by key and offset",
        ),
        (
            RecordFault::EmptyDigest,
            "tree list record 3: a file of size 0 has the digest of no bytes",
        ),
    ];
    for (fault, text) in cases {
        assert_eq!(record_err(3, fault).to_string(), text);
    }
}

/// Asserts that the first fault in record order is the one reported, with its number from 0.
#[test]
fn the_first_faulty_record_is_reported() {
    let bytes = patched_record(7, F_KIND, &[0]);
    let bytes = patched(
        &bytes,
        record_at(&bytes, 2) + F_MODE,
        &0xffffu16.to_le_bytes(),
    );
    assert_fault(&bytes, 2, RecordFault::BadMode(0xffff));
}

// ----- limits and the tree -----

/// Asserts that more records than `max_entries` are refused before any record is read, and a
/// count equal to it is taken.
#[test]
fn too_many_entries_is_refused_before_the_walk() {
    // Every record is garbage: a fault in the walk would be a Record error.
    let garbage = Rec {
        kind: 0,
        ..Rec::dir(0, NONE)
    };
    let bytes = spec(vec![garbage, garbage, garbage]).bytes();
    let (result, spans) = run_with(&bytes, &limits(2, 10, 10));
    assert_eq!(
        result.unwrap_err(),
        TreeListV2Error::Tree(RecordError::TooManyEntries(2))
    );
    assert!(spans.is_empty());
    assert_eq!(
        run_with(&bytes, &limits(3, 10, 10)).0.unwrap_err(),
        record_err(0, RecordFault::UnknownKind(0))
    );
    assert_eq!(
        TreeListV2Error::Tree(RecordError::TooManyEntries(2)).to_string(),
        "the tree is refused: tree too large: more than 2 entries"
    );
}

/// Asserts that a path with more names than `max_depth` is refused at the path of its first
/// child, and a tree of exactly that depth, or an empty one at depth 0, is taken.
#[test]
fn a_tree_deeper_than_the_limit_is_refused() {
    let bytes = sample();
    let (result, _) = run_with(&bytes, &limits(100, 2, 100));
    assert_eq!(
        result.unwrap_err(),
        TreeListV2Error::Tree(RecordError::TooDeep("b/e/f".into()))
    );
    run_with(&bytes, &limits(100, 3, 100)).0.unwrap();
    let (result, _) = run_with(&sample(), &limits(100, 0, 100));
    assert_eq!(
        result.unwrap_err(),
        TreeListV2Error::Tree(RecordError::TooDeep("b/e/f".into()))
    );
    let empty = TreeList::default().encode_v2().unwrap();
    run_with(&empty, &limits(100, 0, 100)).0.unwrap();
    assert_eq!(
        TreeListV2Error::Tree(RecordError::TooDeep("b/e/f".into())).to_string(),
        "the tree is refused: tree too deep at \"b/e/f\""
    );
}

/// Asserts that a directory with more children than `max_dir_entries` is refused at its path, the
/// root's being empty, and a directory of exactly the limit is taken.
#[test]
fn a_directory_over_the_limit_is_refused() {
    let bytes = sample();
    // b holds c.txt, d.txt, e, ln2, zz.
    let (result, _) = run_with(&bytes, &limits(100, 10, 4));
    assert_eq!(
        result.unwrap_err(),
        TreeListV2Error::Tree(RecordError::DirectoryTooLarge("b".into()))
    );
    run_with(&bytes, &limits(100, 10, 5)).0.unwrap();
    // The root holds a.txt, b, empty, ln, z.
    let small = TreeList {
        entries: vec![file("a", 0, None, None), file("b", 0, None, None)],
    };
    let (result, _) = run_with(&small.encode_v2().unwrap(), &limits(100, 10, 1));
    assert_eq!(
        result.unwrap_err(),
        TreeListV2Error::Tree(RecordError::DirectoryTooLarge(String::new()))
    );
    assert_eq!(
        TreeListV2Error::Tree(RecordError::DirectoryTooLarge("b".into())).to_string(),
        "the tree is refused: directory too large at \"b\""
    );
}

/// Asserts that two files of one parent and name are refused at their path.
#[test]
fn two_files_of_one_name_in_one_directory_are_refused() {
    let list = TreeList {
        entries: vec![
            file("d/x", 1, Some(("k", 0)), Some(sha(1))),
            file("d/x", 1, Some(("k", 1)), Some(sha(2))),
        ],
    };
    assert_eq!(
        build_err(&list.encode_v2().unwrap()),
        TreeListV2Error::Tree(RecordError::Duplicate("d/x".into()))
    );
    assert_eq!(
        build_err(&list.encode_v2().unwrap()).to_string(),
        "the tree is refused: duplicate path \"d/x\""
    );
}

/// Asserts that a directory and a file of one name in one directory are refused.
#[test]
fn a_directory_and_a_file_of_one_name_are_refused() {
    let list = TreeList {
        entries: vec![file("a", 0, None, None), file("a/x", 0, None, None)],
    };
    assert_eq!(
        build_err(&list.encode_v2().unwrap()),
        TreeListV2Error::Tree(RecordError::Duplicate("a".into()))
    );
    // The same name in two directories is not a duplicate.
    let apart = TreeList {
        entries: vec![file("a/x", 0, None, None), file("b/x", 0, None, None)],
    };
    build(&apart.encode_v2().unwrap()).unwrap();
}

#[path = "tree_list_v2_build_tables_tests.rs"]
mod tables;
