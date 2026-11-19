//! `Image`: a tree's L2 list opened, checked and read by copying records out of a mapping.
//!
//! Proves:
//! - every entry's node, name, link target and lookup equal what `TreeListL2` over the same bytes
//!   says, for a tree of nested directories, files (empty and of several cells), links, a name of
//!   the longest length and a directory of many children;
//! - a lookup of a name before the first child, between two, after the last, in a file, and of a
//!   node past the end, finds nothing;
//! - a list of another tree, a version 2 list, a v1 list, a truncated list and an empty file are
//!   refused with the error that says why; an unreadable file is an I/O error;
//! - the advice reports the record sections' bytes advised, and with a lock whole pages locked;
//! - the packs, the shape, the length and the tree id are the list's.
//!
//! Catches: a lookup that compares names as strings or off by one at either end of the children;
//! a node decoded with the wrong record field; a version this release cannot read opened anyway.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::fs::File;

use support::{Tree, find, open_image, rich_tree, small_tree, tree_id, write_image};
use tierstore_fetch_api::ids::PackId;
use tierstore_fetch_api::list::{ListL2 as _, NodeIndex};
use tierstore_list::{Section, TreeListL2};
use tierstore_testkit::dir::TestDir;
use tierstore_tree::{Image, ImageOptions, NAME_MAX, NodeKind, TreeError};

fn dir(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

#[test]
fn every_entry_reads_as_the_list_reads_it() {
    let dir = dir("image-every-entry");
    let l2 = rich_tree().lists().1;
    let image = open_image(dir.path(), &l2);
    let list = TreeListL2::open(l2.clone()).unwrap();
    assert_eq!(u64::from(list.len()), image.len());
    let mut kinds = [0usize; 3];
    let mut buf = [0u8; NAME_MAX];
    for index in 0..image.len() {
        let want = list.node(NodeIndex(index)).unwrap();
        let node = image.node(NodeIndex(index)).unwrap();
        assert_eq!(node.index, NodeIndex(index));
        assert_eq!(node.parent, want.parent, "entry {index}");
        assert_eq!(node.mode, want.mode, "entry {index}");
        assert_eq!(
            image.name(&node, &mut buf).unwrap(),
            want.name,
            "entry {index}"
        );
        match (node.kind, want.kind) {
            (
                NodeKind::Dir { first, count },
                tierstore_list::NodeKind::Dir { first: f, count: c },
            ) => {
                assert_eq!((first, count), (f, c), "entry {index}");
                assert_eq!(image.read_link(&node), None, "a directory has no target");
                kinds[0] += 1;
            }
            (
                NodeKind::File { pack, offset, size },
                tierstore_list::NodeKind::File {
                    pack: p,
                    offset: o,
                    size: s,
                },
            ) => {
                assert_eq!((pack, offset, size), (p, o, s), "entry {index}");
                assert_eq!(image.read_link(&node), None, "a file has no target");
                kinds[1] += 1;
            }
            (NodeKind::Symlink { len }, tierstore_list::NodeKind::Symlink { target }) => {
                assert_eq!(len, target.len() as u64, "entry {index}");
                assert_eq!(image.read_link(&node).unwrap(), target, "entry {index}");
                kinds[2] += 1;
            }
            (got, want) => panic!("entry {index}: image {got:?}, list {want:?}"),
        }
    }
    assert!(kinds.iter().all(|&n| n > 0), "{kinds:?}");
}

#[test]
fn lookup_of_every_entry_by_its_name_finds_it() {
    let dir = dir("image-lookup-all");
    let l2 = rich_tree().lists().1;
    let image = open_image(dir.path(), &l2);
    let mut buf = [0u8; NAME_MAX];
    for index in 1..image.len() {
        let node = image.node(NodeIndex(index)).unwrap();
        let name = image.name(&node, &mut buf).unwrap().to_vec();
        assert_eq!(image.lookup(node.parent, &name), Some(NodeIndex(index)));
    }
    assert_eq!(image.node(NodeIndex(0)).unwrap().parent, NodeIndex(0));
    let mut buf = [0u8; NAME_MAX];
    assert_eq!(
        image.name(&image.node(NodeIndex(0)).unwrap(), &mut buf),
        Some(&b""[..])
    );
}

#[test]
fn lookup_of_a_missing_name_finds_nothing_at_either_end_and_between() {
    let dir = dir("image-lookup-missing");
    let l2 = small_tree().lists().1;
    let image = open_image(dir.path(), &l2);
    let lib = find(&image, "lib");
    // lib's children: a.txt b.txt empty self
    for name in [
        &b"A"[..],
        b"a",
        b"a.txt0",
        b"b",
        b"c",
        b"empty0",
        b"zzz",
        b"",
    ] {
        assert_eq!(
            image.lookup(lib, name),
            None,
            "{:?}",
            String::from_utf8_lossy(name)
        );
    }
    for name in [&b"a.txt"[..], b"b.txt", b"empty", b"self"] {
        assert!(image.lookup(lib, name).is_some());
    }
    // A name longer than any entry's.
    assert_eq!(image.lookup(lib, &[b'a'; 400]), None);
}

#[test]
fn lookup_in_a_file_a_link_a_missing_node_or_an_empty_directory_finds_nothing() {
    let dir = dir("image-lookup-notdir");
    let l2 = rich_tree().lists().1;
    let image = open_image(dir.path(), &l2);
    let file = find(&image, "lib/a.txt");
    let link = find(&image, "lib/self");
    let empty_dir = find(&image, "deep/one/two/empty-dir");
    assert_eq!(image.lookup(file, b"x"), None);
    assert_eq!(image.lookup(link, b"x"), None);
    assert_eq!(image.lookup(empty_dir, b"x"), None);
    assert_eq!(image.lookup(NodeIndex(image.len()), b"x"), None);
    assert_eq!(image.lookup(NodeIndex(u64::MAX), b"x"), None);
}

#[test]
fn a_node_past_the_last_entry_is_none() {
    let dir = dir("image-node-past");
    let image = open_image(dir.path(), &small_tree().lists().1);
    assert!(image.node(NodeIndex(image.len() - 1)).is_some());
    assert!(image.node(NodeIndex(image.len())).is_none());
    assert!(image.node(NodeIndex(u64::MAX)).is_none());
}

#[test]
fn the_longest_name_is_copied_whole() {
    let dir = dir("image-long-name");
    let image = open_image(dir.path(), &rich_tree().lists().1);
    let long = "n".repeat(255);
    let node = image
        .node(image.lookup(find(&image, "deep"), long.as_bytes()).unwrap())
        .unwrap();
    let mut buf = [0u8; NAME_MAX];
    assert_eq!(image.name(&node, &mut buf).unwrap(), long.as_bytes());
}

#[test]
fn a_list_of_another_tree_is_malformed() {
    let dir = dir("image-wrong-tree");
    let l2 = small_tree().lists().1;
    let path = write_image(dir.path(), &l2);
    let mut other = tree_id(&l2);
    other[0] ^= 1;
    let error =
        Image::open(File::open(path).unwrap(), &other, ImageOptions::default()).unwrap_err();
    assert!(matches!(error, TreeError::Malformed(_)), "{error}");
    assert!(error.to_string().starts_with("tree image: "), "{error}");
}

#[test]
fn a_version_2_list_and_a_v1_list_are_unsupported() {
    let dir = dir("image-versions");
    let (v2, l2) = small_tree().lists();
    for (name, bytes) in [("v2", v2), ("v1", b"{\"files\":[]}".to_vec())] {
        let path = write_image(dir.path(), &bytes);
        let error = Image::open(
            File::open(path).unwrap(),
            &tree_id(&l2),
            ImageOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(error, TreeError::ListVersionUnsupported(_)),
            "{name}: {error}"
        );
    }
}

#[test]
fn a_truncated_list_and_an_empty_file_are_refused() {
    let dir = dir("image-truncated");
    let l2 = small_tree().lists().1;
    let id = tree_id(&l2);
    for len in [0, 10, 100, l2.len() / 2, l2.len() - 1] {
        let path = write_image(dir.path(), &l2[..len]);
        let error =
            Image::open(File::open(path).unwrap(), &id, ImageOptions::default()).unwrap_err();
        assert!(
            matches!(
                error,
                TreeError::Malformed(_) | TreeError::ListVersionUnsupported(_)
            ),
            "{len}: {error}"
        );
    }
}

#[test]
fn a_file_that_cannot_be_read_is_an_io_error_naming_what_failed() {
    let dir = dir("image-unreadable");
    let l2 = small_tree().lists().1;
    let path = write_image(dir.path(), &l2);
    let write_only = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let error = Image::open(write_only, &tree_id(&l2), ImageOptions::default()).unwrap_err();
    match &error {
        TreeError::Io { what, .. } => assert_eq!(*what, "read"),
        other => panic!("{other}"),
    }
    assert_eq!(error.to_string(), "tree image: read");
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn a_directory_is_not_an_image() {
    let dir = dir("image-is-dir");
    let l2 = small_tree().lists().1;
    let error = Image::open(
        File::open(dir.path()).unwrap(),
        &tree_id(&l2),
        ImageOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(error, TreeError::Io { what: "read", .. }),
        "{error}"
    );
}

#[test]
fn the_advice_counts_the_record_sections_as_advised_and_nothing_locked_by_default() {
    let dir = dir("image-advice");
    let l2 = small_tree().lists().1;
    let list = TreeListL2::open(l2.clone()).unwrap();
    let image = open_image(dir.path(), &l2);
    let len = |s| {
        let r = list.section_range(s);
        r.end - r.start
    };
    let advice = image.advice();
    assert_eq!(
        advice.willneed_bytes,
        len(Section::Entries) + len(Section::Names)
    );
    assert!(advice.willneed_bytes > 0);
    assert_eq!(advice.locked_bytes, 0);
    assert_eq!(advice.lock_refused, None);
}

#[expect(
    clippy::disallowed_macros,
    reason = "a skip says why on the test's output, as a test has no operator"
)]
#[test]
fn a_hot_mount_locks_the_whole_pages_that_cover_both_sections() {
    let dir = dir("image-lock");
    let l2 = small_tree().lists().1;
    let list = TreeListL2::open(l2.clone()).unwrap();
    let path = write_image(dir.path(), &l2);
    let image = Image::open(
        File::open(path).unwrap(),
        &tree_id(&l2),
        ImageOptions { lock: true },
    )
    .unwrap();
    let page = rustix::param::page_size() as u64;
    let pages = |s| {
        let r = list.section_range(s);
        r.end.div_ceil(page) * page - r.start / page * page
    };
    let advice = image.advice();
    if advice.lock_refused.is_some() {
        // A memlock limit (no CAP_IPC_LOCK): the refusal is reported, and the image serves.
        eprintln!(
            "skip lock assertions: refused with {:?}",
            advice.lock_refused
        );
        assert_eq!(advice.locked_bytes, 0);
    } else {
        assert_eq!(
            advice.locked_bytes,
            pages(Section::Entries) + pages(Section::Names)
        );
        assert_eq!(advice.locked_bytes % page, 0);
    }
    assert!(image.lookup(NodeIndex(0), b"lib").is_some());
}

#[test]
fn a_tree_with_only_its_root_is_empty_has_no_packs_and_locks_only_its_entries() {
    let dir = dir("image-root-only");
    let l2 = Tree::default().lists().1;
    let path = write_image(dir.path(), &l2);
    let image = Image::open(
        File::open(path).unwrap(),
        &tree_id(&l2),
        ImageOptions { lock: true },
    )
    .unwrap();
    assert!(image.is_empty());
    assert_eq!(image.len(), 1);
    assert!(image.packs().is_empty());
    assert_eq!(image.lookup(NodeIndex(0), b"a"), None);
    let list = TreeListL2::open(l2).unwrap();
    let names = list.section_range(Section::Names);
    assert_eq!(names.end - names.start, 0);
    let entries = list.section_range(Section::Entries);
    assert_eq!(image.advice().willneed_bytes, entries.end - entries.start);
    assert!(!format!("{image:?}").is_empty());
}

#[test]
fn the_length_shape_tree_id_and_packs_are_the_lists() {
    let dir = dir("image-facts");
    let l2 = rich_tree().lists().1;
    let list = TreeListL2::open(l2.clone()).unwrap();
    let image = open_image(dir.path(), &l2);
    assert!(!image.is_empty());
    assert_eq!(image.len(), u64::from(list.len()));
    assert_eq!(image.shape(), list.shape());
    assert_eq!(image.tree_id(), list.tree_id());
    assert_eq!(image.packs().len(), list.pack_count() as usize);
    for (number, pack) in image.packs().iter().enumerate() {
        let number = number as u32;
        let record = list.pack_record(number).unwrap();
        assert_eq!(
            pack.id,
            PackId {
                key: record.key,
                root: list.pack_root(number).unwrap()
            }
        );
        assert_eq!(pack.size, record.pack_size);
    }
}
