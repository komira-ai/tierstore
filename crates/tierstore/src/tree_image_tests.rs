//! Tests of the tree image: the entries, the lookups and the counts it reads in place from the
//! bytes of a version 2 tree list, what it refuses, and the spans the walk hands out for it.

use super::*;

use crate::tree_list::{EMPTY_SHA256, ListEntry, ListKind, TreeList};
use crate::tree_list_v2::EntryFault;
use crate::{BlobSource, RecordError};

/// A digest whose bytes differ from one another, which `byte` names.
fn digest(byte: u8) -> TreeDigest {
    let mut bytes = [0u8; 32];
    for (at, value) in bytes.iter_mut().enumerate() {
        *value = byte.wrapping_add((at as u8).wrapping_mul(17));
    }
    TreeDigest::from_bytes(bytes)
}

fn file(path: &str, mode: u16, size: u64, key: &str, offset: u64, sha: u8) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::new(mode).unwrap(),
            size,
            source: Some(BlobSource {
                key: Arc::from(key),
                offset,
            }),
            sha256: Some(digest(sha)),
        },
    }
}

fn empty_file(path: &str) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::FILE,
            size: 0,
            source: None,
            sha256: None,
        },
    }
}

fn dir(path: &str, mode: u16) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::Dir {
            mode: Mode::new(mode).unwrap(),
        },
    }
}

fn link(path: &str, target: &str) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::Symlink {
            target: target.to_owned(),
        },
    }
}

fn encode(entries: Vec<ListEntry>) -> Vec<u8> {
    TreeList { entries }.encode_v2().unwrap()
}

/// The list most tests read. Entries: 0 root, 1 `d` (0700), 2 `f` (04755, 3 bytes, `k2` at 10),
/// 3 `g` (empty), 4 `h` (5 bytes, `k1` at 0), 5 `i` (5 bytes, `k1` at 100, the digest of `h`),
/// 6 `l` (a link to `d/x`), 7 `d/x` (the span of `h`), 8 `d/y` (empty), 9 `d/z` (a link to `x`).
fn sample() -> Vec<u8> {
    encode(vec![
        dir("d", 0o700),
        file("f", 0o4755, 3, "k2", 10, 2),
        empty_file("g"),
        file("h", 0o644, 5, "k1", 0, 1),
        file("i", 0o644, 5, "k1", 100, 1),
        link("l", "d/x"),
        file("d/x", 0o644, 5, "k1", 0, 1),
        empty_file("d/y"),
        link("d/z", "x"),
    ])
}

fn image() -> TreeImage {
    TreeImage::open(Bytes::from(sample()), &TreeLimits::default(), |_| {}).unwrap()
}

fn le32(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

/// Where entry `index` begins: the records are at the offset the header holds at 16.
fn rec(bytes: &[u8], index: usize) -> usize {
    le32(bytes, 16) + index * 32
}

fn set_name(bytes: &mut [u8], index: usize, offset: u32, len: u8) {
    let at = rec(bytes, index);
    bytes[at + 4..at + 8].copy_from_slice(&offset.to_le_bytes());
    bytes[at + 14] = len;
}

fn set_kind(bytes: &mut [u8], index: usize, kind: u8) {
    let at = rec(bytes, index);
    bytes[at + 13] = (kind << 4) | (bytes[at + 13] & 0x0f);
}

fn set_size(bytes: &mut [u8], index: usize, size: u64) {
    let at = rec(bytes, index);
    bytes[at + 24..at + 32].copy_from_slice(&size.to_le_bytes());
}

/// Asserts that an image of a tree of every kind has the entries the list holds, each read from
/// its record: the root with no parent and no name, a directory with its mode and children, a
/// file with its mode, size, digest and span, an empty file with no span, and links with targets.
#[test]
fn an_entry_of_each_kind_is_read_from_its_record() {
    let image = image();
    let k1 = Arc::<str>::from("k1");
    let k2 = Arc::<str>::from("k2");
    let entry = |index| image.entry(index).unwrap();

    assert_eq!(
        entry(0),
        ImageEntry {
            index: 0,
            parent: None,
            name: "",
            kind: ImageKind::Dir {
                mode: Mode::DIR,
                children: 1..7
            },
        }
    );
    assert_eq!(
        entry(1),
        ImageEntry {
            index: 1,
            parent: Some(0),
            name: "d",
            kind: ImageKind::Dir {
                mode: Mode::new(0o700).unwrap(),
                children: 7..10
            },
        }
    );
    assert_eq!(
        entry(2),
        ImageEntry {
            index: 2,
            parent: Some(0),
            name: "f",
            kind: ImageKind::File {
                mode: Mode::new(0o4755).unwrap(),
                size: 3,
                sha256: digest(2),
                span: Some((&k2, 10)),
            },
        }
    );
    assert_eq!(
        entry(3),
        ImageEntry {
            index: 3,
            parent: Some(0),
            name: "g",
            kind: ImageKind::File {
                mode: Mode::FILE,
                size: 0,
                sha256: TreeDigest::from_bytes(EMPTY_SHA256),
                span: None,
            },
        }
    );
    assert_eq!(
        entry(5).kind,
        ImageKind::File {
            mode: Mode::FILE,
            size: 5,
            sha256: digest(1),
            span: Some((&k1, 100)),
        }
    );
    assert_eq!(
        entry(6),
        ImageEntry {
            index: 6,
            parent: Some(0),
            name: "l",
            kind: ImageKind::Symlink { target: "d/x" },
        }
    );
    assert_eq!(
        entry(9),
        ImageEntry {
            index: 9,
            parent: Some(1),
            name: "z",
            kind: ImageKind::Symlink { target: "x" },
        }
    );
}

/// Asserts that an index past the last entry, and the largest index, are no entry, and that the
/// lookups by index of such an entry answer as for nothing.
#[test]
fn an_index_past_the_end_is_no_entry() {
    let image = image();
    assert_eq!(image.len(), 10);
    assert_eq!(image.entry(10), None);
    assert_eq!(image.entry(u32::MAX), None);
    assert_eq!(image.name_of(10), "");
    assert_eq!(image.name_of(u32::MAX), "");
    assert!(!image.is_dir(10));
    assert_eq!(image.children(10), 0..0);
}

/// Asserts that the counts, keys, ranges, bytes and length of an image are those of its list:
/// every file counted whatever its size, the sizes summed, the keys once each in order, the ranges
/// the list's writer gave, and the bytes the very bytes handed in.
#[test]
fn the_counts_keys_ranges_and_bytes_are_those_of_the_list() {
    let bytes = Bytes::from(sample());
    let image = TreeImage::open(bytes.clone(), &TreeLimits::default(), |_| {}).unwrap();
    assert_eq!(image.files(), 6);
    assert_eq!(image.file_bytes(), 18);
    assert_eq!(image.len(), 10);
    assert!(!image.is_empty());
    assert_eq!(image.keys(), [Arc::<str>::from("k1"), Arc::from("k2")]);
    let list = TreeListV2::open(&bytes).unwrap();
    let ranges: Vec<_> = image.ranges().collect();
    assert_eq!(ranges, list.ranges().collect::<Vec<_>>());
    assert!(!ranges.is_empty());
    assert_eq!(image.bytes(), &bytes);
    assert_eq!(image.bytes().as_ptr(), bytes.as_ptr());
}

/// Asserts that a list of no entries is an image of the root alone, which is empty and has no
/// children, no files and no bytes.
#[test]
fn a_list_of_no_entries_is_the_root_alone() {
    let image = TreeImage::open(
        Bytes::from(encode(Vec::new())),
        &TreeLimits::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(image.len(), 1);
    assert!(image.is_empty());
    assert!(image.children(0).is_empty());
    assert_eq!((image.files(), image.file_bytes()), (0, 0));
    assert_eq!(image.find(""), Ok(0));
    assert_eq!(image.find("a"), Err(ImagePathError::NotFound));
}

/// Asserts that the sizes of files are summed with saturation at `u64::MAX`, the files still
/// counted one by one.
#[test]
fn the_bytes_of_files_saturate_at_the_largest_u64() {
    let mut bytes = encode(vec![
        file("a", 0o644, 1, "k", 0, 1),
        file("b", 0o644, 1, "k", 0, 2),
        file("c", 0o644, 1, "k", 0, 3),
    ]);
    for index in 1..=3 {
        set_size(&mut bytes, index, u64::MAX);
    }
    let image = TreeImage::checked(Bytes::from(bytes)).unwrap();
    assert_eq!(image.files(), 3);
    assert_eq!(image.file_bytes(), u64::MAX);
}

/// Asserts that the children of a directory are the range of its record, and that a file, a link
/// and the empty directory have none.
#[test]
fn children_of_a_directory_are_a_range_and_of_anything_else_empty() {
    let image = image();
    assert_eq!(image.children(0), 1..7);
    assert_eq!(image.children(1), 7..10);
    for index in [2, 3, 6, 7, 9] {
        assert_eq!(image.children(index), 0..0, "entry {index}");
    }
    let names: Vec<_> = image.children(1).map(|i| image.name_of(i)).collect();
    assert_eq!(names, ["x", "y", "z"]);
}

/// Asserts that a child is found by its name, and that a name before the first, between two,
/// after the last, a prefix of one, and the name of a grandchild are not found, and a file and
/// an entry past the end have no children to find.
#[test]
fn a_child_is_found_by_binary_search_and_an_absent_name_is_none() {
    let image = image();
    for (name, index) in [("d", 1), ("f", 2), ("g", 3), ("h", 4), ("i", 5), ("l", 6)] {
        assert_eq!(image.child(0, name), Some(index), "{name}");
    }
    for name in ["a", "e", "dd", "", "k", "m", "z", "x", "d/x"] {
        assert_eq!(image.child(0, name), None, "{name:?}");
    }
    assert_eq!(image.child(1, "x"), Some(7));
    assert_eq!(image.child(1, "z"), Some(9));
    assert_eq!(image.child(1, "w"), None);
    assert_eq!(image.child(1, "zz"), None);
    assert_eq!(image.child(2, "x"), None);
    assert_eq!(image.child(6, "x"), None);
    assert_eq!(image.child(10, "x"), None);
}

/// Asserts that a listing that goes on after a name begins at the first child that sorts after
/// it: after each child's own name, at the one after it; after a name before the first, at the
/// first; after a name between two, at the next; after the last or past it, at the end; and a
/// file or an entry past the end lists nothing.
#[test]
fn children_after_a_name_begin_at_the_next_name() {
    let image = image();
    assert_eq!(image.children_after(0, ""), 1..7);
    assert_eq!(image.children_after(0, "a"), 1..7);
    for (at, name) in ["d", "f", "g", "h", "i", "l"].into_iter().enumerate() {
        let next = at as u32 + 2;
        assert_eq!(image.children_after(0, name), next..7, "after {name}");
    }
    assert_eq!(image.children_after(0, "e"), 2..7);
    assert_eq!(image.children_after(0, "dd"), 2..7);
    assert_eq!(image.children_after(0, "z"), 7..7);
    assert_eq!(image.children_after(1, "x"), 8..10);
    assert_eq!(image.children_after(1, "z"), 10..10);
    assert_eq!(image.children_after(2, "a"), 0..0);
    assert_eq!(image.children_after(10, "a"), 0..0);
}

/// Asserts that a path leads to its entry: the empty path to the root, a name and a nested path
/// to theirs.
#[test]
fn a_path_leads_to_its_entry() {
    let image = image();
    assert_eq!(image.find(""), Ok(0));
    assert_eq!(image.find("d"), Ok(1));
    assert_eq!(image.find("l"), Ok(6));
    assert_eq!(image.find("d/x"), Ok(7));
    assert_eq!(image.find("d/z"), Ok(9));
}

/// Asserts that a name a directory lacks is `NotFound`, and a name below a file or a link, even
/// one that does not exist, is `NotADirectory`: nothing below it is followed.
#[test]
fn a_path_that_leads_nowhere_says_why() {
    let image = image();
    assert_eq!(image.find("e"), Err(ImagePathError::NotFound));
    assert_eq!(image.find("d/q"), Err(ImagePathError::NotFound));
    assert_eq!(image.find("d/"), Err(ImagePathError::NotFound));
    assert_eq!(image.find("e/x"), Err(ImagePathError::NotFound));
    assert_eq!(image.find("f/x"), Err(ImagePathError::NotADirectory));
    assert_eq!(image.find("l/x"), Err(ImagePathError::NotADirectory));
    assert_eq!(image.find("d/x/y"), Err(ImagePathError::NotADirectory));
    assert_eq!(image.find("g/q/r"), Err(ImagePathError::NotADirectory));
}

/// Asserts that `is_dir` and `name_of` answer for every entry: the directories are the root and
/// `d`, and each name is the entry's own.
#[test]
fn is_dir_and_name_of_answer_for_each_entry() {
    let image = image();
    let dirs: Vec<_> = (0..10).filter(|&i| image.is_dir(i)).collect();
    assert_eq!(dirs, [0, 1]);
    let names: Vec<_> = (0..10).map(|i| image.name_of(i)).collect();
    assert_eq!(names, ["", "d", "f", "g", "h", "i", "l", "x", "y", "z"]);
}

/// Asserts that a name two directories share is stored once and read for both, and found in each.
#[test]
fn a_name_shared_by_two_directories_is_read_in_both() {
    let bytes = encode(vec![
        file("d/x", 0o644, 1, "k", 0, 1),
        file("e/x", 0o644, 1, "k", 1, 2),
        link("e/y", "x"),
    ]);
    let image = TreeImage::checked(Bytes::from(bytes)).unwrap();
    assert_eq!(image.find("d/x"), Ok(3));
    assert_eq!(image.find("e/x"), Ok(4));
    assert_eq!(image.name_of(3), "x");
    assert_eq!(image.name_of(4), "x");
    assert_eq!(image.find("e/y"), Ok(5));
}

/// Asserts that a name already checked at one length is checked again at another: two siblings
/// whose names begin at one place, `a` and `ab`, are both accepted and found.
#[test]
fn a_name_met_again_with_another_length_is_checked_again() {
    let mut bytes = encode(vec![
        file("ab", 0o644, 1, "k", 0, 1),
        file("zz", 0o644, 1, "k", 1, 2),
    ]);
    let at = le32(&bytes, rec(&bytes, 1) + 4) as u32;
    set_name(&mut bytes, 1, at, 1);
    set_name(&mut bytes, 2, at, 2);
    let image = TreeImage::checked(Bytes::from(bytes)).unwrap();
    assert_eq!(image.name_of(1), "a");
    assert_eq!(image.name_of(2), "ab");
    assert_eq!(image.find("a"), Ok(1));
    assert_eq!(image.find("ab"), Ok(2));
}

/// Asserts that a name checked at one length does not vouch for the same start at another: a
/// two-byte character, then the same start cut after its first byte, is refused as `BadName`.
#[test]
fn a_name_checked_at_one_length_does_not_vouch_for_another() {
    let mut bytes = encode(vec![
        file("\u{e9}", 0o644, 1, "k", 0, 1),
        file("\u{ea}", 0o644, 1, "k", 1, 2),
    ]);
    let at = le32(&bytes, rec(&bytes, 1) + 4) as u32;
    set_name(&mut bytes, 2, at, 1);
    assert_eq!(
        TreeImage::checked(Bytes::from(bytes)).unwrap_err(),
        TreeListV2Error::Entry {
            index: 2,
            fault: EntryFault::BadName
        }
    );
}

/// Asserts that names with a `/` or a NUL byte are refused as `BadName` by the walk, the
/// slash and the NUL being tested by their bytes.
#[test]
fn a_name_with_a_slash_or_a_nul_is_refused() {
    for name in [b"a/b", b"a\0b"] {
        let mut bytes = encode(vec![file("abc", 0o644, 1, "k", 0, 1)]);
        let at = le32(&bytes, 48) + le32(&bytes, rec(&bytes, 1) + 4);
        bytes[at..at + 3].copy_from_slice(name);
        assert_eq!(
            TreeImage::checked(Bytes::from(bytes)).unwrap_err(),
            TreeListV2Error::Entry {
                index: 1,
                fault: EntryFault::BadName
            },
            "{name:?}"
        );
    }
}

/// Asserts that an open hands out the span of each file of size above 0 as the walk meets it,
/// in the order of the list: two files of one span, and two of one digest at two spans, all
/// handed out; and none for an empty file or a link.
#[test]
fn open_hands_out_a_span_for_every_file_of_bytes() {
    let mut spans = Vec::new();
    let image = TreeImage::open(Bytes::from(sample()), &TreeLimits::default(), |span| {
        spans.push(span)
    })
    .unwrap();
    assert_eq!(image.files(), 6);
    let span = |key, offset, size, sha| ListSpan {
        key,
        offset,
        size,
        sha256: digest(sha),
    };
    assert_eq!(
        spans,
        [
            span(1, 10, 3, 2),
            span(0, 0, 5, 1),
            span(0, 100, 5, 1),
            span(0, 0, 5, 1),
        ]
    );
}

/// Asserts that the walk by span mode hands out what each mode says: a span per file and no names
/// for `PerFile`, a span per digest and every name for `PerDigest`, nothing for `None`, and the
/// same count of files and bytes in all.
#[test]
fn each_span_mode_hands_out_its_spans_and_keeps_its_names() {
    let bytes = sample();
    let list = TreeListV2::open(&bytes).unwrap();
    let limits = TreeLimits::default();
    let counted = Counted {
        files: 6,
        bytes: 18,
    };

    let mut each = Vec::new();
    let ((names, depths), got) = list
        .walk(&limits, Spans::PerFile, |span| each.push(span))
        .unwrap();
    assert_eq!(
        (each.len(), names.len(), depths.len(), got),
        (4, 1, 10, counted)
    );

    let mut digests = Vec::new();
    let ((names, depths), got) = list
        .walk(&limits, Spans::PerDigest, |span| digests.push(span))
        .unwrap();
    assert_eq!(digests, [each[0].clone(), each[1].clone()]);
    assert_eq!(depths.len(), 10);
    assert_eq!(got, counted);
    assert_eq!(names, ["", "d", "f", "g", "h", "i", "l", "x", "y", "z"]);

    let ((names, depths), got) = list
        .walk(&limits, Spans::None, |_| panic!("a span was handed out"))
        .unwrap();
    assert_eq!((names.len(), depths.len(), got), (1, 10, counted));
}

/// Asserts that an open refuses a list whose entry is bad, as the walk does, with the entry's
/// index and fault, and that `checked` refuses it too.
#[test]
fn a_bad_entry_is_refused_by_open_and_by_checked() {
    let mut bytes = sample();
    set_kind(&mut bytes, 3, 3);
    let error =
        TreeImage::open(Bytes::from(bytes.clone()), &TreeLimits::default(), |_| {}).unwrap_err();
    assert_eq!(
        error,
        TreeListV2Error::Entry {
            index: 3,
            fault: EntryFault::UnknownKind(3)
        }
    );
    assert_eq!(TreeImage::checked(Bytes::from(bytes)).unwrap_err(), error);
}

/// Asserts that bytes that are not a version 2 list are refused by both opens as `BadMagic`.
#[test]
fn bytes_that_are_not_a_list_are_refused() {
    let junk = Bytes::from_static(b"not a tree list at all, but long enough to hold a header....");
    assert_eq!(
        TreeImage::open(junk.clone(), &TreeLimits::default(), |_| {}).unwrap_err(),
        TreeListV2Error::BadMagic
    );
    assert_eq!(
        TreeImage::checked(junk).unwrap_err(),
        TreeListV2Error::BadMagic
    );
}

/// Asserts that `open` refuses a tree over each limit, as the walk does, and that `checked`
/// accepts the same bytes, whose shape it checks and no limit.
#[test]
fn open_holds_the_limits_and_checked_does_not() {
    let bytes = Bytes::from(sample());
    let open = |limits: TreeLimits| TreeImage::open(bytes.clone(), &limits, |_| {}).unwrap_err();
    let wide = TreeLimits::default();

    assert_eq!(
        open(TreeLimits {
            max_entries: 8,
            ..wide
        }),
        TreeListV2Error::Tree(RecordError::TooManyEntries(8))
    );
    assert_eq!(
        open(TreeLimits {
            max_depth: 1,
            ..wide
        }),
        TreeListV2Error::Tree(RecordError::TooDeep("d/x".to_owned()))
    );
    assert_eq!(
        open(TreeLimits {
            max_dir_entries: 2,
            ..wide
        }),
        TreeListV2Error::Tree(RecordError::DirectoryTooLarge(String::new()))
    );

    let image = TreeImage::checked(bytes).unwrap();
    assert_eq!(image.len(), 10);
    assert_eq!(image.files(), 6);
    assert_eq!(image.find("d/x"), Ok(7));
}

/// Asserts that a tree at exactly each limit is accepted by `open`.
#[test]
fn open_accepts_a_tree_at_its_limits() {
    let limits = TreeLimits {
        max_entries: 9,
        max_depth: 2,
        max_dir_entries: 6,
    };
    let image = TreeImage::open(Bytes::from(sample()), &limits, |_| {}).unwrap();
    assert_eq!(image.len(), 10);
}

/// Asserts that `checked` hands out no span: it walks with none, so a list of files reads the
/// same as an `open` of it.
#[test]
fn checked_reads_the_same_tree_as_open() {
    let checked = TreeImage::checked(Bytes::from(sample())).unwrap();
    let opened = image();
    assert_eq!(checked.len(), opened.len());
    assert_eq!(checked.files(), opened.files());
    assert_eq!(checked.file_bytes(), opened.file_bytes());
    for index in 0..10 {
        assert_eq!(checked.entry(index), opened.entry(index));
    }
}

/// Asserts that an entry with an empty name is refused as `BadName`, even when its name begins
/// at the end of the names, and that a name running past the names is refused as `NameBounds`.
#[test]
fn an_empty_name_and_a_name_past_the_names_are_refused() {
    let base = encode(vec![file("a", 0o644, 1, "k", 0, 1)]);
    let end = le32(&base, 52) as u32;
    let mut empty = base.clone();
    set_name(&mut empty, 1, end, 0);
    assert_eq!(
        TreeImage::checked(Bytes::from(empty)).unwrap_err(),
        TreeListV2Error::Entry {
            index: 1,
            fault: EntryFault::BadName
        }
    );
    let mut past = base;
    set_name(&mut past, 1, end, 1);
    assert_eq!(
        TreeImage::checked(Bytes::from(past)).unwrap_err(),
        TreeListV2Error::Entry {
            index: 1,
            fault: EntryFault::NameBounds
        }
    );
}

/// Asserts that the debug text of an image gives its size, entries, files and keys and none of
/// its bytes.
#[test]
fn the_debug_text_of_an_image_gives_its_counts() {
    let image = image();
    assert_eq!(
        format!("{image:?}"),
        format!(
            "TreeImage {{ bytes: {}, entries: 10, files: 6, keys: 2, .. }}",
            image.bytes().len()
        )
    );
}
