//! The reader of an open L2 list, `TreeListL2` and its `ListL2` impl, proved against values the
//! tests compute on their own: pack records, cell digests and pack roots against `sha2` over the
//! pack bytes; nodes, children, lookups, file digests and the shape against the entries the tree
//! was built from; ranges against tierstore's version 2 reader; the projection digest against the
//! SHA-256 of the version 2 list; and the same answers over `Vec<u8>`, `&[u8]` and `Arc<[u8]>`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use support::{Tree, pack_bytes, sha256, small_tree};
use tierstore::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeListV2};
use tierstore_fetch_api::ids::{PackKey, PackRoot};
use tierstore_fetch_api::list::{ListL2, NodeIndex, PackRecord};
use tierstore_list::{
    Node, NodeKind, PackCells, PackInfo, PackRange, Shape, TreeListL2, pack_root, sidecar_from_v2,
};

const MIB: u64 = 1 << 20;

fn open(tree: &Tree) -> TreeListL2<Vec<u8>> {
    TreeListL2::open(tree.lists().1).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `sha256("TSPACK1" || size_le64 || 20u8 || digests...)` over the cells of `bytes`, by `sha2`.
fn independent_root(bytes: &[u8]) -> [u8; 32] {
    let mut input = b"TSPACK1".to_vec();
    input.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    input.push(20);
    for cell in bytes.chunks(MIB as usize) {
        input.extend_from_slice(&sha256(cell));
    }
    sha256(&input)
}

/// Every path of the tree: the entries' paths and the directories they imply.
fn expected_paths(tree: &Tree) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for entry in &tree.entries {
        let mut at = 0;
        while let Some(slash) = entry.path[at..].find('/') {
            paths.insert(entry.path[..at + slash].to_owned());
            at += slash + 1;
        }
        paths.insert(entry.path.clone());
    }
    paths
}

/// The path of each node, by following parents from the root; entries come after their parent.
fn node_paths<B: AsRef<[u8]>>(list: &TreeListL2<B>) -> Vec<String> {
    let mut paths: Vec<String> = vec![String::new()];
    for index in 1..u64::from(list.len()) {
        let node = list.node(NodeIndex(index)).unwrap();
        let parent = &paths[node.parent.0 as usize];
        let name = std::str::from_utf8(node.name).unwrap();
        let path = if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}/{name}")
        };
        paths.push(path);
    }
    paths
}

/// A tree of several levels, with a link, an empty directory, an executable and a sticky directory.
fn deep_tree() -> Tree {
    let mut tree = Tree::default()
        .pack("p/1", pack_bytes(3, 3000))
        .pack("p/2", pack_bytes(4, 700))
        .file("a/b/c/d/deep.txt", "p/1", 0, 1000)
        .file("a/b/c/d/e/f/g/h", "p/1", 1000, 500)
        .file("a/b/c/x", "p/1", 1500, 1500)
        .file("a/b/y", "p/2", 0, 300)
        .link("a/b/lnk", "../y")
        .dir("a/empty", 0o700)
        .file("a/z", "p/2", 300, 400)
        .file("m", "p/2", 0, 700)
        .file("m2/n", "p/2", 10, 20)
        .dir("zz/sticky", 0o1777)
        .empty_file("a/b/c/nothing");
    tree.entries.push(ListEntry {
        path: "a/b/run.sh".to_owned(),
        kind: ListKind::File {
            mode: Mode::new(0o755).unwrap(),
            size: 100,
            source: Some(BlobSource {
                key: Arc::from("p/1"),
                offset: 2000,
            }),
            sha256: Some(TreeDigest::from_bytes(sha256(
                &tree.packs["p/1"][2000..2100],
            ))),
        },
    });
    tree
}

#[test]
fn pack_count_and_records_of_small_tree_are_exact() {
    let tree = small_tree();
    let list = open(&tree);
    assert_eq!(list.pack_count(), 2);
    assert_eq!(
        list.pack_record(0),
        Some(PackRecord {
            key: PackKey("out/a/0".to_owned()),
            pack_size: 2 * MIB + MIB / 2,
            cell_first: 0,
            cell_count: 3,
            cell_shift: 20,
            // The files leave a gap in the pack: not fully covered.
            pack_flags: 0,
        })
    );
    assert_eq!(
        list.pack_record(1),
        Some(PackRecord {
            key: PackKey("out/b/0".to_owned()),
            pack_size: 100,
            cell_first: 3,
            cell_count: 1,
            cell_shift: 20,
            pack_flags: 1,
        })
    );
}

#[test]
fn pack_record_past_the_end_is_none() {
    let list = open(&small_tree());
    assert_eq!(list.pack_record(2), None);
    assert_eq!(list.pack_record(u32::MAX), None);
}

#[test]
fn shared_pack_has_the_shared_flag_beside_fully_covered() {
    let tree = small_tree();
    let v2 = tree.v2();
    let l2 = sidecar_from_v2(&v2, |key| {
        tree.info(key).map(|info| PackInfo {
            shared: key == "out/b/0",
            ..info
        })
    })
    .unwrap();
    let list = TreeListL2::open(l2).unwrap();
    assert_eq!(list.pack_record(0).unwrap().pack_flags, 0);
    assert_eq!(list.pack_record(1).unwrap().pack_flags, 3);
}

#[test]
fn cell_digest_equals_independent_sha256_of_each_cell_slice() {
    let tree = small_tree();
    let list = open(&tree);
    for (number, bytes) in tree.packs.values().enumerate() {
        let cells: Vec<&[u8]> = bytes.chunks(MIB as usize).collect();
        for (cell, slice) in cells.iter().enumerate() {
            assert_eq!(
                list.cell_digest(number as u32, cell as u32),
                Some(sha256(slice)),
                "pack {number} cell {cell}"
            );
        }
        assert_eq!(list.cell_digest(number as u32, cells.len() as u32), None);
    }
}

#[test]
fn cell_digest_of_the_short_last_cell_hashes_only_its_bytes() {
    let tree = small_tree();
    let list = open(&tree);
    let bytes = &tree.packs["out/a/0"];
    assert_eq!(
        list.cell_digest(0, 2),
        Some(sha256(&bytes[2 * MIB as usize..]))
    );
}

#[test]
fn cell_digest_past_the_cells_or_the_packs_is_none() {
    let list = open(&small_tree());
    assert_eq!(list.cell_digest(0, 3), None);
    assert_eq!(list.cell_digest(1, 1), None);
    assert_eq!(list.cell_digest(0, u32::MAX), None);
    assert_eq!(list.cell_digest(2, 0), None);
    assert_eq!(list.cell_digest(u32::MAX, 0), None);
}

#[test]
fn pack_root_equals_independent_computation_and_the_function() {
    let tree = small_tree();
    let list = open(&tree);
    for (number, (_, bytes)) in tree.packs.iter().enumerate() {
        let expected = independent_root(bytes);
        assert_eq!(
            ListL2::pack_root(&list, number as u32),
            Some(PackRoot::Digest(expected))
        );
        let cells = PackCells::of(bytes);
        let joined: Vec<u8> = cells.digests.iter().flatten().copied().collect();
        assert_eq!(pack_root(bytes.len() as u64, &joined), expected);
    }
}

#[test]
fn pack_root_is_stable_on_a_second_call() {
    let list = open(&small_tree());
    let first = ListL2::pack_root(&list, 0);
    let second = ListL2::pack_root(&list, 0);
    assert!(first.is_some());
    assert_eq!(first, second);
    let other = ListL2::pack_root(&list, 1);
    assert_ne!(first, other);
    assert_eq!(ListL2::pack_root(&list, 1), other);
}

#[test]
fn pack_root_past_the_end_is_none() {
    let list = open(&small_tree());
    assert_eq!(ListL2::pack_root(&list, 2), None);
    assert_eq!(ListL2::pack_root(&list, u32::MAX), None);
}

#[test]
fn two_lists_with_the_same_pack_bytes_give_one_pack_root_even_when_the_trees_differ() {
    let first = small_tree();
    let second = Tree::default()
        .pack("out/b/0", first.packs["out/b/0"].clone())
        .file("only/part", "out/b/0", 10, 30)
        .link("only/ln", "part");
    let (a, b) = (open(&first), open(&second));
    assert_ne!(a.tree_id(), b.tree_id());
    // Pack "out/b/0" is number 1 of the first list and 0 of the second.
    assert_eq!(a.pack_record(1).unwrap().key, b.pack_record(0).unwrap().key);
    assert_eq!(ListL2::pack_root(&a, 1), ListL2::pack_root(&b, 0));
}

#[test]
fn a_changed_pack_gives_another_pack_root() {
    let first = small_tree();
    let mut changed = first.packs["out/b/0"].clone();
    changed[99] ^= 1;
    let second = Tree::default()
        .pack("out/b/0", changed)
        .file("lib/a.txt", "out/b/0", 0, 60);
    let (a, b) = (open(&first), open(&second));
    assert_ne!(ListL2::pack_root(&a, 1), ListL2::pack_root(&b, 0));
}

#[test]
fn pack_root_depends_on_the_size_of_the_pack() {
    // Two packs whose bytes hash to equal cell digests of one cell each differ in size only.
    let one = Tree::default().pack("k", vec![7; 10]).file("f", "k", 0, 10);
    let two = Tree::default().pack("k", vec![7; 11]).file("f", "k", 0, 10);
    assert_ne!(
        ListL2::pack_root(&open(&one), 0),
        ListL2::pack_root(&open(&two), 0)
    );
}

#[test]
fn pack_key_and_upload_part_size_of_each_pack() {
    let list = open(&small_tree());
    assert_eq!(list.pack_key(0), Some("out/a/0"));
    assert_eq!(list.pack_key(1), Some("out/b/0"));
    assert_eq!(list.pack_key(2), None);
    assert_eq!(list.upload_part_size(0), Some(0));
    assert_eq!(list.upload_part_size(1), Some(0));
    assert_eq!(list.upload_part_size(2), None);
}

#[test]
fn upload_part_size_reads_the_value_the_writer_was_given() {
    let tree = small_tree();
    let v2 = tree.v2();
    let l2 = sidecar_from_v2(&v2, |key| {
        tree.info(key).map(|info| PackInfo {
            upload_part_size: if key == "out/a/0" { 8 * MIB } else { 5 },
            ..info
        })
    })
    .unwrap();
    let list = TreeListL2::open(l2).unwrap();
    assert_eq!(list.upload_part_size(0), Some(8 * MIB));
    assert_eq!(list.upload_part_size(1), Some(5));
}

#[test]
fn node_of_every_kind_of_small_tree_has_exact_values() {
    let list = open(&small_tree());
    let at = |index: u64| list.node(NodeIndex(index)).unwrap();
    let root = at(0);
    assert_eq!(
        root,
        Node {
            parent: NodeIndex(0),
            name: b"",
            mode: 0o755,
            kind: NodeKind::Dir { first: 1, count: 3 },
        }
    );
    assert_eq!(
        at(1),
        Node {
            parent: NodeIndex(0),
            name: b"bin",
            mode: 0o755,
            kind: NodeKind::Dir { first: 4, count: 2 },
        }
    );
    assert_eq!(
        at(2),
        Node {
            parent: NodeIndex(0),
            name: b"lib",
            mode: 0o755,
            kind: NodeKind::Dir { first: 6, count: 4 },
        }
    );
    assert_eq!(
        at(3),
        Node {
            parent: NodeIndex(0),
            name: b"tmp",
            mode: 0o1777,
            kind: NodeKind::Dir {
                first: 10,
                count: 0
            },
        }
    );
    assert_eq!(
        at(4),
        Node {
            parent: NodeIndex(1),
            name: b"cc",
            mode: 0o644,
            kind: NodeKind::File {
                pack: Some(0),
                offset: 0,
                size: MIB + 10,
            },
        }
    );
    assert_eq!(
        at(5),
        Node {
            parent: NodeIndex(1),
            name: b"ld",
            mode: 0o644,
            kind: NodeKind::File {
                pack: Some(0),
                offset: MIB + 4096,
                size: MIB,
            },
        }
    );
    assert_eq!(
        at(6),
        Node {
            parent: NodeIndex(2),
            name: b"a.txt",
            mode: 0o644,
            kind: NodeKind::File {
                pack: Some(1),
                offset: 0,
                size: 60,
            },
        }
    );
    assert_eq!(
        at(7),
        Node {
            parent: NodeIndex(2),
            name: b"b.txt",
            mode: 0o644,
            kind: NodeKind::File {
                pack: Some(1),
                offset: 60,
                size: 40,
            },
        }
    );
    assert_eq!(
        at(8),
        Node {
            parent: NodeIndex(2),
            name: b"empty",
            mode: 0o644,
            kind: NodeKind::File {
                pack: None,
                offset: 0,
                size: 0,
            },
        }
    );
    assert_eq!(
        at(9),
        Node {
            parent: NodeIndex(2),
            name: b"self",
            mode: 0,
            kind: NodeKind::Symlink { target: b"a.txt" },
        }
    );
}

#[test]
fn node_past_the_end_is_none() {
    let list = open(&small_tree());
    assert_eq!(list.len(), 10);
    assert!(list.node(NodeIndex(9)).is_some());
    assert_eq!(list.node(NodeIndex(10)), None);
    assert_eq!(list.node(NodeIndex(u64::MAX)), None);
}

#[test]
fn executable_file_and_sticky_directory_keep_their_modes() {
    let tree = deep_tree();
    let list = open(&tree);
    let paths = node_paths(&list);
    let index_of = |path: &str| paths.iter().position(|p| p == path).unwrap() as u64;
    let run = list.node(NodeIndex(index_of("a/b/run.sh"))).unwrap();
    assert_eq!(run.mode, 0o755);
    assert_eq!(
        run.kind,
        NodeKind::File {
            pack: Some(0),
            offset: 2000,
            size: 100,
        }
    );
    let sticky = list.node(NodeIndex(index_of("zz/sticky"))).unwrap();
    assert_eq!(sticky.mode, 0o1777);
    let empty = list.node(NodeIndex(index_of("a/empty"))).unwrap();
    assert_eq!(empty.mode, 0o700);
    assert!(matches!(empty.kind, NodeKind::Dir { count: 0, .. }));
    let link = list.node(NodeIndex(index_of("a/b/lnk"))).unwrap();
    assert_eq!(link.kind, NodeKind::Symlink { target: b"../y" });
    assert_eq!(link.mode, 0);
}

#[test]
fn children_ranges_of_small_tree_and_none_for_a_file_or_past_the_end() {
    let list = open(&small_tree());
    assert_eq!(list.children(NodeIndex(0)), Some(1..4));
    assert_eq!(list.children(NodeIndex(1)), Some(4..6));
    assert_eq!(list.children(NodeIndex(2)), Some(6..10));
    assert_eq!(list.children(NodeIndex(3)), Some(10..10));
    // A file, an empty file and a link are not directories.
    assert_eq!(list.children(NodeIndex(4)), None);
    assert_eq!(list.children(NodeIndex(8)), None);
    assert_eq!(list.children(NodeIndex(9)), None);
    assert_eq!(list.children(NodeIndex(10)), None);
    assert_eq!(list.children(NodeIndex(u64::MAX)), None);
}

#[test]
fn file_sha256_of_files_equals_sha256_of_their_bytes() {
    let tree = small_tree();
    let list = open(&tree);
    let a = &tree.packs["out/a/0"];
    let b = &tree.packs["out/b/0"];
    let at = |index: u64| list.file_sha256(NodeIndex(index));
    assert_eq!(at(4), Some(sha256(&a[..(MIB + 10) as usize])));
    assert_eq!(
        at(5),
        Some(sha256(&a[(MIB + 4096) as usize..(2 * MIB + 4096) as usize]))
    );
    assert_eq!(at(6), Some(sha256(&b[..60])));
    assert_eq!(at(7), Some(sha256(&b[60..])));
}

#[test]
fn file_sha256_of_an_empty_file_is_the_empty_digest() {
    let list = open(&small_tree());
    assert_eq!(list.file_sha256(NodeIndex(8)), Some(sha256(b"")));
}

#[test]
fn file_sha256_of_dirs_links_and_past_the_end_is_none() {
    let list = open(&small_tree());
    for dir in 0..4 {
        assert_eq!(list.file_sha256(NodeIndex(dir)), None, "dir {dir}");
    }
    assert_eq!(list.file_sha256(NodeIndex(9)), None);
    assert_eq!(list.file_sha256(NodeIndex(10)), None);
    assert_eq!(list.file_sha256(NodeIndex(u64::MAX)), None);
}

#[test]
fn dir_lookup_finds_every_path_of_a_tree_of_several_levels() {
    let tree = deep_tree();
    let list = open(&tree);
    let paths = node_paths(&list);
    let all = expected_paths(&tree);
    assert_eq!(paths[1..].iter().cloned().collect::<BTreeSet<_>>(), all);
    assert_eq!(paths.len(), all.len() + 1);
    for (index, path) in paths.iter().enumerate().skip(1) {
        let mut at = NodeIndex(0);
        for name in path.split('/') {
            at = list.dir_lookup(at, name.as_bytes()).unwrap_or_else(|| {
                panic!("{name} of {path} not found");
            });
        }
        assert_eq!(at, NodeIndex(index as u64), "{path}");
    }
}

#[test]
fn dir_lookup_finds_each_child_of_a_wide_directory() {
    let mut tree = Tree::default().pack("k", pack_bytes(9, 64));
    let names: Vec<String> = (0..300).map(|n| format!("n{n:04}")).collect();
    for name in &names {
        tree = tree.file(name, "k", 0, 8);
    }
    let list = open(&tree);
    assert_eq!(list.shape().max_children, 300);
    for (n, name) in names.iter().enumerate() {
        assert_eq!(
            list.dir_lookup(NodeIndex(0), name.as_bytes()),
            Some(NodeIndex(1 + n as u64)),
            "{name}"
        );
    }
    assert_eq!(list.dir_lookup(NodeIndex(0), b"n0300"), None);
    assert_eq!(list.dir_lookup(NodeIndex(0), b"n"), None);
}

#[test]
fn dir_lookup_of_absent_names_is_none() {
    let list = open(&small_tree());
    let bin = NodeIndex(1);
    assert_eq!(list.dir_lookup(bin, b"cc"), Some(NodeIndex(4)));
    assert_eq!(list.dir_lookup(bin, b"ld"), Some(NodeIndex(5)));
    // Before the first child, between two, after the last.
    assert_eq!(list.dir_lookup(bin, b"a"), None);
    assert_eq!(list.dir_lookup(bin, b"ce"), None);
    assert_eq!(list.dir_lookup(bin, b"zz"), None);
    // Prefixes of a child, and a child with something after it.
    assert_eq!(list.dir_lookup(bin, b"c"), None);
    assert_eq!(list.dir_lookup(bin, b"cc2"), None);
    assert_eq!(list.dir_lookup(bin, b"l"), None);
    assert_eq!(list.dir_lookup(bin, b"ldd"), None);
    assert_eq!(list.dir_lookup(bin, b""), None);
    // A path is not a name.
    assert_eq!(list.dir_lookup(NodeIndex(0), b"bin/cc"), None);
}

#[test]
fn dir_lookup_in_a_file_a_link_an_empty_dir_or_past_the_end_is_none() {
    let list = open(&small_tree());
    // File, empty file, link.
    assert_eq!(list.dir_lookup(NodeIndex(4), b"cc"), None);
    assert_eq!(list.dir_lookup(NodeIndex(8), b"empty"), None);
    assert_eq!(list.dir_lookup(NodeIndex(9), b"a.txt"), None);
    // An empty directory.
    assert_eq!(list.dir_lookup(NodeIndex(3), b"x"), None);
    assert_eq!(list.dir_lookup(NodeIndex(3), b""), None);
    // Past the end.
    assert_eq!(list.dir_lookup(NodeIndex(10), b"x"), None);
    assert_eq!(list.dir_lookup(NodeIndex(u64::MAX), b"x"), None);
}

#[test]
fn dir_lookup_of_a_name_that_is_a_prefix_of_a_sibling_finds_the_right_one() {
    let tree = Tree::default()
        .pack("k", pack_bytes(1, 16))
        .file("a", "k", 0, 1)
        .file("ab", "k", 0, 2)
        .file("abc", "k", 0, 3)
        .file("b", "k", 0, 4);
    let list = open(&tree);
    for (n, name) in ["a", "ab", "abc", "b"].iter().enumerate() {
        let found = list.dir_lookup(NodeIndex(0), name.as_bytes()).unwrap();
        assert_eq!(found, NodeIndex(1 + n as u64));
        assert_eq!(list.node(found).unwrap().name, name.as_bytes());
    }
    assert_eq!(list.dir_lookup(NodeIndex(0), b"aa"), None);
    assert_eq!(list.dir_lookup(NodeIndex(0), b"abcd"), None);
}

#[test]
fn walking_every_directory_gives_sorted_children_whose_parent_is_the_directory() {
    let list = open(&deep_tree());
    let mut next = 1u64;
    for dir in 0..u64::from(list.len()) {
        let Some(children) = list.children(NodeIndex(dir)) else {
            continue;
        };
        assert_eq!(children.start, next, "children of {dir} follow the last");
        next = children.end;
        let mut previous: Option<&[u8]> = None;
        for child in children {
            let node = list.node(NodeIndex(child)).unwrap();
            assert_eq!(node.parent, NodeIndex(dir));
            if let Some(previous) = previous {
                assert!(previous < node.name, "{previous:?} before {:?}", node.name);
            }
            previous = Some(node.name);
        }
    }
    assert_eq!(next, u64::from(list.len()));
}

#[test]
fn ranges_equal_tierstores_version_2_ranges_field_by_field() {
    for tree in [small_tree(), deep_tree()] {
        let (v2, l2) = tree.lists();
        let list = TreeListL2::open(l2).unwrap();
        let reference = TreeListV2::open(&v2).unwrap();
        let theirs: Vec<PackRange> = reference
            .ranges()
            .map(|range| PackRange {
                pack: range.key,
                offset: range.offset,
                len: range.len,
            })
            .collect();
        assert!(!theirs.is_empty());
        assert_eq!(list.ranges().collect::<Vec<_>>(), theirs);
    }
}

#[test]
fn ranges_of_small_tree_merge_files_of_a_pack_and_leave_its_gap() {
    let list = open(&small_tree());
    let ranges: Vec<PackRange> = list.ranges().collect();
    assert!(ranges.iter().all(|range| range.len > 0));
    // Each file's bytes are inside some range of its pack.
    let tree = small_tree();
    for (pack, offset, len) in [
        (0, 0, MIB + 10),
        (0, MIB + 4096, MIB),
        (1, 0, 60),
        (1, 60, 40),
    ] {
        assert!(
            ranges
                .iter()
                .any(|r| r.pack == pack && r.offset <= offset && offset + len <= r.offset + r.len),
            "file at {pack}:{offset}+{len}"
        );
    }
    assert_eq!(tree.packs.len(), 2);
}

#[test]
fn shape_of_small_tree_is_exact() {
    let tree = small_tree();
    let list = open(&tree);
    let path_bytes: u64 = expected_paths(&tree).iter().map(|p| p.len() as u64).sum();
    assert_eq!(
        list.shape(),
        Shape {
            entries: 10,
            files: 5,
            file_bytes: (MIB + 10) + MIB + 60 + 40,
            max_depth: 2,
            max_children: 4,
            path_bytes,
        }
    );
    // bin lib tmp bin/cc bin/ld lib/a.txt lib/b.txt lib/empty lib/self
    assert_eq!(path_bytes, 3 + 3 + 3 + 6 + 6 + 9 + 9 + 9 + 8);
}

#[test]
fn shape_of_deep_tree_is_computed_from_the_entry_paths() {
    let tree = deep_tree();
    let list = open(&tree);
    let paths = expected_paths(&tree);
    let files: Vec<&ListEntry> = tree
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, ListKind::File { .. }))
        .collect();
    let file_bytes: u64 = files
        .iter()
        .map(|entry| match &entry.kind {
            ListKind::File { size, .. } => *size,
            _ => 0,
        })
        .sum();
    let mut children: BTreeMap<String, u32> = BTreeMap::new();
    for path in &paths {
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        *children.entry(parent.to_owned()).or_default() += 1;
    }
    assert_eq!(
        list.shape(),
        Shape {
            entries: paths.len() as u32 + 1,
            files: files.len() as u32,
            file_bytes,
            max_depth: paths.iter().map(|p| p.split('/').count()).max().unwrap() as u32,
            max_children: *children.values().max().unwrap(),
            path_bytes: paths.iter().map(|p| p.len() as u64).sum(),
        }
    );
    assert_eq!(list.shape().max_depth, 8);
}

#[test]
fn len_and_is_empty_of_trees_with_and_without_entries() {
    let small = open(&small_tree());
    assert_eq!(small.len(), 10);
    assert!(!small.is_empty());
    let root_only = open(&Tree::default());
    assert_eq!(root_only.len(), 1);
    assert!(root_only.is_empty());
    let one_file = open(&Tree::default().empty_file("f"));
    assert_eq!(one_file.len(), 2);
    assert!(!one_file.is_empty());
}

#[test]
fn root_only_tree_has_a_root_and_no_packs() {
    let list = open(&Tree::default());
    assert_eq!(list.pack_count(), 0);
    assert_eq!(list.pack_record(0), None);
    assert_eq!(list.ranges().count(), 0);
    assert_eq!(list.children(NodeIndex(0)), Some(1..1));
    assert_eq!(list.dir_lookup(NodeIndex(0), b"x"), None);
    assert_eq!(list.node(NodeIndex(1)), None);
    assert_eq!(
        list.shape(),
        Shape {
            entries: 1,
            ..Shape::default()
        }
    );
}

#[test]
fn bytes_returns_the_input_and_tree_id_is_the_sha256_of_the_version_2_list() {
    let (v2, l2) = small_tree().lists();
    let list = TreeListL2::open(l2.clone()).unwrap();
    assert_eq!(list.bytes(), l2.as_slice());
    assert_eq!(list.tree_id(), &sha256(&v2));
}

#[test]
fn debug_output_starts_with_the_tree_id_in_hex() {
    let (v2, l2) = small_tree().lists();
    let list = TreeListL2::open(l2).unwrap();
    let text = format!("{list:?}");
    let prefix = format!("TreeListL2 {{ tree_id: \"{}\"", hex(&sha256(&v2)));
    assert!(text.starts_with(&prefix), "{text}");
    assert!(text.contains("shape: Shape {"), "{text}");
    assert!(text.ends_with(", .. }"), "{text}");
}

fn tree_of_many_packs(count: usize) -> Tree {
    let mut tree = Tree::default();
    for n in 0..count {
        let key = format!("many/key-{n:03}");
        tree = tree
            .pack(&key, pack_bytes(n as u8, 20 + n))
            .file(&format!("f{n:03}"), &key, 0, 10);
    }
    tree
}

fn tree_without_ranges() -> Tree {
    Tree::default()
        .empty_file("e")
        .link("l", "e")
        .dir("d", 0o700)
}

#[test]
fn projection_digest_is_the_sha256_of_the_version_2_list_for_several_trees() {
    let trees = [
        ("small", small_tree()),
        ("deep", deep_tree()),
        ("root only", Tree::default()),
        // An even count of keys leaves the key offsets (count + 1 words) needing padding; an
        // odd count does not.
        ("many keys even", tree_of_many_packs(40)),
        ("many keys odd", tree_of_many_packs(41)),
        ("many keys", tree_of_many_packs(300)),
        ("no ranges", tree_without_ranges()),
    ];
    for (case, tree) in trees {
        let (v2, l2) = tree.lists();
        let list = TreeListL2::open(l2).unwrap();
        assert_eq!(list.projection_digest(), sha256(&v2), "{case}");
        assert_eq!(list.check_tree_id(), Ok(()), "{case}");
    }
}

/// Asserts that the projection is the version 2 list the L2 list was made from, byte for byte,
/// for every shape of tree the digest test takes (padding after an odd and an even count of key
/// offsets, no ranges, the root alone), and that a sidecar opened for its tree id projects the
/// list that id names.
#[test]
fn projection_is_the_version_2_list_byte_for_byte_for_several_trees() {
    let trees = [
        ("small", small_tree()),
        ("deep", deep_tree()),
        ("root only", Tree::default()),
        ("many keys even", tree_of_many_packs(40)),
        ("many keys odd", tree_of_many_packs(41)),
        ("no ranges", tree_without_ranges()),
    ];
    for (case, tree) in trees {
        let (v2, l2) = tree.lists();
        let list = TreeListL2::open_sidecar(Some(l2), &sha256(&v2)).unwrap();
        assert_eq!(list.projection(), v2, "{case}");
    }
}

#[test]
fn tree_without_ranges_reads_files_less() {
    let list = open(&tree_without_ranges());
    assert_eq!(list.ranges().count(), 0);
    assert_eq!(list.pack_count(), 0);
    assert_eq!(list.len(), 4);
}

#[test]
fn many_packs_read_back_their_keys_sizes_and_cell_runs_in_key_order() {
    let tree = tree_of_many_packs(300);
    let list = open(&tree);
    assert_eq!(list.pack_count(), 300);
    for (number, (key, bytes)) in tree.packs.iter().enumerate() {
        let record = list.pack_record(number as u32).unwrap();
        assert_eq!(record.key, PackKey(key.clone()));
        assert_eq!(record.pack_size, bytes.len() as u64);
        // Every pack is one cell, so its cells begin at its number.
        assert_eq!(record.cell_first, number as u64);
        assert_eq!(record.cell_count, 1);
        assert_eq!(
            list.cell_digest(number as u32, 0),
            Some(sha256(bytes)),
            "{key}"
        );
    }
}

#[test]
fn reader_answers_alike_over_vec_slice_and_arc_bytes() {
    let (v2, l2) = small_tree().lists();
    let vec_list = TreeListL2::open(l2.clone()).unwrap();
    let slice_list = TreeListL2::open(l2.as_slice()).unwrap();
    let arc: Arc<[u8]> = Arc::from(l2.clone());
    let arc_list = TreeListL2::open(arc).unwrap();
    for (case, check) in [("slice", answers(&slice_list)), ("arc", answers(&arc_list))] {
        assert_eq!(check, answers(&vec_list), "{case}");
    }
    assert_eq!(slice_list.bytes(), l2.as_slice());
    assert_eq!(arc_list.bytes(), l2.as_slice());
    assert_eq!(arc_list.projection_digest(), sha256(&v2));
    assert_eq!(slice_list.projection_digest(), sha256(&v2));
}

type Answers = (
    u32,
    Vec<Option<PackRecord>>,
    Vec<Option<PackRoot>>,
    Vec<String>,
    Option<NodeIndex>,
    Shape,
);

fn answers<B: AsRef<[u8]> + Send + Sync>(list: &TreeListL2<B>) -> Answers {
    (
        list.pack_count(),
        (0..3).map(|pack| list.pack_record(pack)).collect(),
        (0..3).map(|pack| ListL2::pack_root(list, pack)).collect(),
        node_paths(list),
        list.dir_lookup(NodeIndex(2), b"b.txt"),
        list.shape(),
    )
}
