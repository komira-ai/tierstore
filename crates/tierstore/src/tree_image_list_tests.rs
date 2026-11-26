//! Tests of a list read in place: the entries, the lookups and the listings of a version 2 tree
//! list on the heap and mapped from its file, each case on both backings; that the mapping is of
//! the file and takes whole pages; what an image file that cannot be read gives; and that bytes
//! changed after the walk, before the map and (on Linux) after it, read as absent, never as a
//! panic.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt as _;
use std::path::Path;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;

use super::*;
use crate::tree_image::tests::{
    digest, dir, empty_file, encode, file, le32, link, rec, sample, set_name,
};
use crate::tree_list::{EMPTY_SHA256, ListEntry};
use crate::{TreeLimits, TreeListV2Error};

/// A scratch directory for the image files of one test.
fn scratch() -> TestDir {
    TestDir::under(std::env::temp_dir(), "tree-image-list")
}

/// The list `bytes` on the heap, and mapped from a file of `dir` that holds them: the two
/// backings every case is asserted on.
fn both(dir: &TestDir, bytes: &[u8]) -> [ListImage; 2] {
    let path = dir.join("image");
    std::fs::write(&path, bytes).unwrap();
    let heap = ListImage::heap(&TreeImage::checked(Bytes::copy_from_slice(bytes)).unwrap());
    let mapped = ListImage::open_file(&path).unwrap();
    assert!(!heap.is_mapped());
    assert!(mapped.is_mapped());
    [heap, mapped]
}

/// Asserts that a list of a tree of every kind, on the heap and mapped, has the entries the list
/// holds, each read from its record: the root with no parent and no name, a directory with its
/// mode and children, a file with its mode, size, digest and span, an empty file with no span, and
/// links with targets.
#[test]
fn an_entry_of_each_kind_is_read_from_its_record() {
    let dir = scratch();
    let k1 = Arc::<str>::from("k1");
    let k2 = Arc::<str>::from("k2");
    for image in both(&dir, &sample()) {
        let entry = |index| image.entry(index).unwrap();
        assert_eq!(
            entry(0),
            ImageEntry {
                index: 0,
                parent: None,
                name: String::new(),
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
                name: "d".to_owned(),
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
                name: "f".to_owned(),
                kind: ImageKind::File {
                    mode: Mode::new(0o4755).unwrap(),
                    size: 3,
                    sha256: digest(2),
                    span: Some((Arc::clone(&k2), 10)),
                },
            }
        );
        assert_eq!(
            entry(3),
            ImageEntry {
                index: 3,
                parent: Some(0),
                name: "g".to_owned(),
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
                span: Some((Arc::clone(&k1), 100)),
            }
        );
        assert_eq!(
            entry(6),
            ImageEntry {
                index: 6,
                parent: Some(0),
                name: "l".to_owned(),
                kind: ImageKind::Symlink {
                    target: "d/x".to_owned()
                },
            }
        );
        assert_eq!(
            entry(9),
            ImageEntry {
                index: 9,
                parent: Some(1),
                name: "z".to_owned(),
                kind: ImageKind::Symlink {
                    target: "x".to_owned()
                },
            }
        );
    }
}

/// Asserts that the mode of each entry is read from its record alone: a directory's and a file's
/// stored mode, a link's `SYMLINK`, and none past the last entry.
#[test]
fn the_mode_of_each_entry_is_its_records() {
    let dir = scratch();
    for image in both(&dir, &sample()) {
        assert_eq!(image.mode(0), Some(Mode::DIR));
        assert_eq!(image.mode(1), Some(Mode::new(0o700).unwrap()));
        assert_eq!(image.mode(2), Some(Mode::new(0o4755).unwrap()));
        assert_eq!(image.mode(6), Some(Mode::SYMLINK));
        assert_eq!(image.mode(10), None);
    }
}

/// Asserts that an index past the last entry, and the largest index, are no entry, and that the
/// lookups by index of such an entry answer as for nothing.
#[test]
fn an_index_past_the_end_is_no_entry() {
    let dir = scratch();
    for image in both(&dir, &sample()) {
        assert_eq!(image.len(), 10);
        assert_eq!(image.entry(10), None);
        assert_eq!(image.entry(u32::MAX), None);
        assert_eq!(image.name_of(10), "");
        assert_eq!(image.name_of(u32::MAX), "");
        assert!(!image.is_dir(10));
        assert_eq!(image.children(10), 0..0);
    }
}

/// Asserts that the children of a directory are the range of its record, and that a file, a link
/// and the empty directory have none.
#[test]
fn children_of_a_directory_are_a_range_and_of_anything_else_empty() {
    let dir = scratch();
    for image in both(&dir, &sample()) {
        assert_eq!(image.children(0), 1..7);
        assert_eq!(image.children(1), 7..10);
        for index in [2, 3, 6, 7, 9] {
            assert_eq!(image.children(index), 0..0, "entry {index}");
        }
        let names: Vec<_> = image.children(1).map(|i| image.name_of(i)).collect();
        assert_eq!(names, ["x", "y", "z"]);
    }
}

/// Asserts that a child is found by its name, and that a name before the first, between two,
/// after the last, a prefix of one, and the name of a grandchild are not found, and a file and
/// an entry past the end have no children to find.
#[test]
fn a_child_is_found_by_binary_search_and_an_absent_name_is_none() {
    let dir = scratch();
    for image in both(&dir, &sample()) {
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
}

/// Asserts that a listing that goes on after a name begins at the first child that sorts after
/// it: after each child's own name, at the one after it; after a name before the first, at the
/// first; after a name between two, at the next; after the last or past it, at the end; and a
/// file or an entry past the end lists nothing.
#[test]
fn children_after_a_name_begin_at_the_next_name() {
    let dir = scratch();
    for image in both(&dir, &sample()) {
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
}

/// Asserts that a path leads to its entry: the empty path to the root, a name and a nested path
/// to theirs, and that `find_entry` gives the entry `find` finds.
#[test]
fn a_path_leads_to_its_entry() {
    let dir = scratch();
    for image in both(&dir, &sample()) {
        assert_eq!(image.find(""), Ok(0));
        assert_eq!(image.find("d"), Ok(1));
        assert_eq!(image.find("l"), Ok(6));
        assert_eq!(image.find("d/x"), Ok(7));
        assert_eq!(image.find("d/z"), Ok(9));
        assert_eq!(image.find_entry("d/z"), Ok(image.entry(9).unwrap()));
    }
}

/// Asserts that a name a directory lacks is `NotFound`, and a name below a file or a link, even
/// one that does not exist, is `NotADirectory`: nothing below it is followed.
#[test]
fn a_path_that_leads_nowhere_says_why() {
    let dir = scratch();
    for image in both(&dir, &sample()) {
        assert_eq!(image.find("e"), Err(ImagePathError::NotFound));
        assert_eq!(image.find("d/q"), Err(ImagePathError::NotFound));
        assert_eq!(image.find("d/"), Err(ImagePathError::NotFound));
        assert_eq!(image.find("e/x"), Err(ImagePathError::NotFound));
        assert_eq!(image.find("f/x"), Err(ImagePathError::NotADirectory));
        assert_eq!(image.find("l/x"), Err(ImagePathError::NotADirectory));
        assert_eq!(image.find("d/x/y"), Err(ImagePathError::NotADirectory));
        assert_eq!(image.find("g/q/r"), Err(ImagePathError::NotADirectory));
        assert_eq!(image.find_entry("e"), Err(ImagePathError::NotFound));
    }
}

/// Asserts that `is_dir` and `name_of` answer for every entry: the directories are the root and
/// `d`, and each name is the entry's own.
#[test]
fn is_dir_and_name_of_answer_for_each_entry() {
    let dir = scratch();
    for image in both(&dir, &sample()) {
        let dirs: Vec<_> = (0..10).filter(|&i| image.is_dir(i)).collect();
        assert_eq!(dirs, [0, 1]);
        let names: Vec<_> = (0..10).map(|i| image.name_of(i)).collect();
        assert_eq!(names, ["", "d", "f", "g", "h", "i", "l", "x", "y", "z"]);
    }
}

/// Asserts that a name two directories share is stored once and read for both, and found in each.
#[test]
fn a_name_shared_by_two_directories_is_read_in_both() {
    let bytes = encode(vec![
        file("d/x", 0o644, 1, "k", 0, 1),
        file("e/x", 0o644, 1, "k", 1, 2),
        link("e/y", "x"),
    ]);
    let dir = scratch();
    for image in both(&dir, &bytes) {
        assert_eq!(image.find("d/x"), Ok(3));
        assert_eq!(image.find("e/x"), Ok(4));
        assert_eq!(image.name_of(3), "x");
        assert_eq!(image.name_of(4), "x");
        assert_eq!(image.find("e/y"), Ok(5));
    }
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
    let dir = scratch();
    for image in both(&dir, &bytes) {
        assert_eq!(image.name_of(1), "a");
        assert_eq!(image.name_of(2), "ab");
        assert_eq!(image.find("a"), Ok(1));
        assert_eq!(image.find("ab"), Ok(2));
    }
}

/// Asserts that a name of 255 bytes, the longest a record holds, is read whole and found by its
/// name, beside a name one byte shorter that sorts before it.
#[test]
fn a_name_of_the_longest_length_is_read_and_found() {
    let long = "n".repeat(255);
    let shorter = "n".repeat(254);
    let bytes = encode(vec![
        file(&long, 0o644, 1, "k", 0, 1),
        file(&shorter, 0o644, 1, "k", 1, 2),
    ]);
    let dir = scratch();
    for image in both(&dir, &bytes) {
        assert_eq!(image.name_of(1), shorter);
        assert_eq!(image.name_of(2), long);
        assert_eq!(image.child(0, &long), Some(2));
        assert_eq!(image.child(0, &shorter), Some(1));
        assert_eq!(image.children_after(0, &shorter), 2..3);
    }
}

/// The entries of a generated tree of `dirs` directories, each with `files` files, a link and a
/// directory of three files below it.
fn large(dirs: usize, files: usize) -> Vec<ListEntry> {
    let mut entries = Vec::new();
    for d in 0..dirs {
        let top = format!("d{d:04}");
        entries.push(dir(&top, 0o755));
        for f in 0..files {
            let at = (d * files + f) as u64;
            let sha = (at % 251) as u8;
            entries.push(file(
                &format!("{top}/f{f:04}"),
                0o644,
                1 + at % 7,
                "k",
                at,
                sha,
            ));
        }
        entries.push(link(&format!("{top}/link"), "f0000"));
        for f in 0..3 {
            entries.push(empty_file(&format!("{top}/sub/g{f}")));
        }
    }
    entries
}

/// Asserts, on a generated tree of more than 20,000 entries, that the mapped list answers every
/// read as the heap list does: each entry, mode, name and kind; each directory's children, its
/// link count (2 and its directories), each child found by its name, and the listing after each
/// name; and the paths of a deep file and a link.
#[test]
fn a_mapped_list_answers_entry_child_children_mode_and_links_as_the_heap_image_does() {
    let bytes = encode(large(200, 100));
    let dir = scratch();
    let [heap, mapped] = both(&dir, &bytes);
    assert!(heap.len() > 20_000);
    assert_eq!(mapped.len(), heap.len());
    let links = |image: &ListImage, at: u32| {
        2 + image.children(at).filter(|kid| image.is_dir(*kid)).count()
    };
    for at in 0..heap.len() as u32 {
        assert_eq!(mapped.entry(at), heap.entry(at), "entry {at}");
        assert_eq!(mapped.mode(at), heap.mode(at), "mode {at}");
        assert_eq!(mapped.is_dir(at), heap.is_dir(at), "is_dir {at}");
        assert_eq!(mapped.children(at), heap.children(at), "children {at}");
        if heap.is_dir(at) {
            assert_eq!(links(&mapped, at), links(&heap, at), "links {at}");
            for kid in heap.children(at) {
                let name = heap.name_of(kid);
                assert_eq!(mapped.child(at, &name), Some(kid), "{name}");
                assert_eq!(
                    mapped.children_after(at, &name),
                    heap.children_after(at, &name)
                );
            }
        }
    }
    assert_eq!(links(&heap, 0), 202);
    let deep = heap.find("d0199/sub/g2").unwrap();
    assert_eq!(mapped.find("d0199/sub/g2"), Ok(deep));
    assert_eq!(
        mapped.find_entry("d0007/link"),
        heap.find_entry("d0007/link")
    );
}

/// Asserts that a mapped list takes its bytes in whole pages, and a list on the heap none, and
/// that the debug text says so with no bytes of the list.
#[test]
fn a_mapped_list_takes_whole_pages_and_a_heap_list_none() {
    let dir = scratch();
    let bytes = sample();
    let [heap, mapped] = both(&dir, &bytes);
    let page = rustix::param::page_size() as u64;
    assert!(bytes.len() as u64 <= page);
    assert_eq!(mapped.mapped_bytes(), page);
    assert_eq!(heap.mapped_bytes(), 0);
    assert_eq!(in_pages(page + 1), 2 * page);
    assert_eq!(in_pages(0), 0);
    assert_eq!(
        format!("{mapped:?}"),
        format!(
            "ListImage {{ bytes: {}, mapped: true, entries: 10, keys: 2, .. }}",
            bytes.len()
        )
    );
}

/// Asserts that a file whose length is not the list's is not mapped, nor one opened without read,
/// and that `stored` reads the list on the heap then, as it does for a file that is gone; and maps
/// the file that holds the list.
#[test]
fn a_file_that_is_not_the_list_or_cannot_be_read_is_not_mapped() {
    let dir = scratch();
    let bytes = sample();
    let image = TreeImage::checked(Bytes::from(bytes.clone())).unwrap();
    let path = dir.join("image");

    std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    let short = ListImage::mapped(&image, File::open(&path).unwrap()).unwrap_err();
    assert_eq!(short.kind(), io::ErrorKind::InvalidData);
    assert!(!ListImage::stored(&image, &path).is_mapped());

    std::fs::write(&path, &bytes).unwrap();
    let write_only = OpenOptions::new().write(true).open(&path).unwrap();
    assert!(ListImage::mapped(&image, write_only).is_err());

    assert!(ListImage::stored(&image, &path).is_mapped());
    assert!(!ListImage::stored(&image, &dir.join("gone")).is_mapped());
}

/// Asserts what an image file that cannot be read as a list gives: a file that is not there and a
/// directory, the system's error; a file past the largest list, `TooLarge` before it is read; and
/// bytes that are no list, the walk's refusal.
#[test]
fn an_image_file_that_cannot_be_read_says_why() {
    let dir = scratch();
    let path = dir.join("image");
    let gone = ListImage::open_file(&path).unwrap_err();
    assert!(matches!(&gone, ImageFileError::Io(why) if why.kind() == io::ErrorKind::NotFound));

    std::fs::create_dir(&path).unwrap();
    assert!(matches!(
        ListImage::open_file(&path),
        Err(ImageFileError::Io(_))
    ));
    std::fs::remove_dir(&path).unwrap();

    let large = File::create(&path).unwrap();
    large.set_len(MAX_TREE_LIST_BYTES as u64 + 1).unwrap();
    let too_large = ListImage::open_file(&path).unwrap_err();
    assert_eq!(
        too_large.to_string(),
        format!(
            "{} bytes, more than a tree list's {MAX_TREE_LIST_BYTES}",
            MAX_TREE_LIST_BYTES + 1
        )
    );

    std::fs::write(
        &path,
        b"not a tree list at all, but long enough to hold a header....",
    )
    .unwrap();
    let junk = ListImage::open_file(&path).unwrap_err();
    assert!(matches!(
        junk,
        ImageFileError::List(TreeListV2Error::BadMagic)
    ));
    assert_eq!(junk.to_string(), TreeListV2Error::BadMagic.to_string());
}

/// Changes the bytes of the image file at `path`, which holds the list `bytes`, in place: a
/// record's name moved past the names (`d`), a name no longer text (`f`), the root's child count
/// raised to the largest, a link's target moved to the largest offset with no length (`l`) and to
/// a length past the names (`d/z`). The file keeps its length: nothing cuts an image short.
fn damage(path: &Path, bytes: &[u8]) {
    let file = OpenOptions::new().write(true).open(path).unwrap();
    let names = le32(bytes, 48);
    let write = |at: usize, what: &[u8]| file.write_all_at(what, at as u64).unwrap();
    write(rec(bytes, 1) + 4, &u32::MAX.to_le_bytes());
    let f = le32(bytes, rec(bytes, 2) + 4);
    write(names + f, &[0xff]);
    write(rec(bytes, 0) + 16, &u64::MAX.to_le_bytes());
    write(rec(bytes, 6) + 16, &u64::MAX.to_le_bytes());
    write(rec(bytes, 6) + 24, &0u64.to_le_bytes());
    write(rec(bytes, 9) + 24, &u64::MAX.to_le_bytes());
    assert_eq!(std::fs::metadata(path).unwrap().len(), bytes.len() as u64);
}

/// Asserts that `mapped`, a mapping of a file [`damage`] changed, reads the damage as absent
/// names, empty targets and children cut to the entries; nothing panics, every entry and listing
/// is still answered, and the names the damage left alone are read as before.
fn assert_damage_read_as_absent(mapped: &ListImage) {
    assert!(mapped.is_mapped());
    assert_eq!(mapped.name_of(1), "");
    assert_eq!(mapped.name_of(2), "");
    assert_eq!(mapped.children(0), 1..10);
    let target = |at| match mapped.entry(at).unwrap().kind {
        ImageKind::Symlink { target } => target,
        other => panic!("not a link: {other:?}"),
    };
    assert_eq!(target(6), "");
    assert_eq!(target(9), "");
    for at in 0..12 {
        let _ = (mapped.entry(at), mapped.mode(at), mapped.children(at));
    }
    for name in ["d", "f", "g", "z", ""] {
        let _ = (mapped.child(0, name), mapped.children_after(0, name));
        let _ = (mapped.find(name), mapped.find_entry(name));
    }
    assert_eq!(mapped.child(0, "g"), Some(3));
}

/// Asserts that bytes changed in place after the walk and before the map (the list walked from
/// the bytes as written, the file then damaged, then mapped with no new walk) read as absent, not
/// as a panic. The mapping is made after the change, so it reads the changed bytes on every
/// system.
#[test]
fn bytes_damaged_after_the_walk_read_as_absent_not_a_panic() {
    let dir = scratch();
    let bytes = sample();
    let path = dir.join("image");
    std::fs::write(&path, &bytes).unwrap();
    let image = TreeImage::checked(Bytes::copy_from_slice(&bytes)).unwrap();
    damage(&path, &bytes);
    let mapped = ListImage::mapped(&image, File::open(&path).unwrap()).unwrap();
    assert_damage_read_as_absent(&mapped);
}

/// Asserts that bytes changed in place under a mapping, after a read through it, read as absent,
/// not as a panic. Linux alone: whether a private mapping shows a change made to its file after it
/// was mapped is left to the system by POSIX; Linux shows it in every page the mapping has not
/// written (it writes none), while macOS may keep the bytes of a page it has already read.
#[cfg(target_os = "linux")]
#[test]
fn bytes_damaged_after_the_map_read_as_absent_not_a_panic() {
    let dir = scratch();
    let bytes = sample();
    let [_, mapped] = both(&dir, &bytes);
    assert_eq!(mapped.name_of(1), "d");
    damage(&dir.join("image"), &bytes);
    assert_damage_read_as_absent(&mapped);
}

/// Asserts that a list removed while it is mapped is still read through its mapping: removing a
/// file never changes what a mapping of it reads.
#[test]
fn a_list_removed_while_it_is_mapped_is_still_read() {
    let dir = scratch();
    let [heap, mapped] = both(&dir, &sample());
    std::fs::remove_file(dir.join("image")).unwrap();
    for at in 0..10 {
        assert_eq!(mapped.entry(at), heap.entry(at));
    }
}

/// Asserts that the image file of a list the walk accepts at the node's limits is accepted by
/// `open_file`, which checks no limit, as `checked` does.
#[test]
fn open_file_checks_the_shape_and_no_limit() {
    let dir = scratch();
    let path = dir.join("image");
    std::fs::write(&path, sample()).unwrap();
    let tight = TreeLimits {
        max_entries: 1,
        ..TreeLimits::default()
    };
    assert!(TreeImage::open(Bytes::from(sample()), &tight, |_| {}).is_err());
    assert_eq!(ListImage::open_file(&path).unwrap().len(), 10);
}
