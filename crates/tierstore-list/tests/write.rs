//! The writer of the L2 tree list, the cells and the round trip: `CellHasher` gives the cell
//! digests of a pack as its bytes stream (the same as `PackCells::of` and as an independent
//! SHA-256 of each 1 MiB slice, whatever the pieces the bytes come in); `sidecar_key` is
//! `cas/l2/<id>`; `sidecar_from_v2` is deterministic, its result opens with
//! `TreeListL2::open_sidecar` under the SHA-256 of the version 2 list and holds the packs, cells,
//! flags and tree that were written, copies the version 2 list's sections byte for byte, and sets
//! `pack_flags` bit 0 exactly where the files cover the pack. The refusals are in
//! `write_errors.rs` and `write_v2.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{Tree, pack_bytes, section, sha256, small_tree};
use tierstore::ListKind;
use tierstore_fetch_api::ids::{PackKey, PackRoot};
use tierstore_fetch_api::list::{ListL2, NodeIndex, PackRecord};
use tierstore_list::{
    CellHasher, NodeKind, PACK_SHARED, PackCells, PackInfo, SIDECAR_PREFIX, TreeListL2,
    sidecar_from_v2, sidecar_key,
};

const MIB: usize = 1 << 20;

/// Where section `index` of the version 2 list `v2` (0 entries, 1 sha256, 2 key offsets, 3 keys,
/// 4 names, 5 ranges) begins, and its length in bytes, as its header says.
fn v2_section(v2: &[u8], index: usize) -> (usize, usize) {
    let offset = u32::from_le_bytes(v2[16 + 8 * index..20 + 8 * index].try_into().unwrap());
    let count = u32::from_le_bytes(v2[20 + 8 * index..24 + 8 * index].try_into().unwrap());
    let len = match index {
        0 | 1 => count as usize * 32,
        2 => (count as usize + 1) * 4,
        5 => count as usize * 24,
        _ => count as usize,
    };
    (offset as usize, len)
}

/// A tree of one pack of 100 bytes and one file of 50 bytes at its start.
fn one_file_tree() -> Tree {
    Tree::default()
        .pack("out/p", pack_bytes(5, 100))
        .file("f", "out/p", 0, 50)
}

/// The path `path` of `list`, by `dir_lookup` from the root.
fn lookup(list: &TreeListL2<Vec<u8>>, path: &str) -> NodeIndex {
    let mut at = NodeIndex(0);
    for part in path.split('/') {
        at = list
            .dir_lookup(at, part.as_bytes())
            .unwrap_or_else(|| panic!("no {path} in the list"));
    }
    at
}

/// The `pack_flags` bit 0 `tree` has for pack `key`, by marking every byte a file names.
fn expected_flags(tree: &Tree, key: &str) -> u8 {
    let mut seen = vec![false; tree.packs[key].len()];
    for entry in &tree.entries {
        if let ListKind::File {
            size,
            source: Some(source),
            ..
        } = &entry.kind
            && source.key.as_ref() == key
        {
            seen[source.offset as usize..(source.offset + size) as usize].fill(true);
        }
    }
    u8::from(seen.iter().all(|byte| *byte))
}

/// Opens the sidecar of `tree` under the SHA-256 of its version 2 list and checks that it holds
/// what was written: the packs, cells, flags, roots, the version 2 list's sections byte for byte,
/// the section layout, every entry of the tree and its ranges.
fn check_round_trip(tree: &Tree) -> TreeListL2<Vec<u8>> {
    let (v2, l2) = tree.lists();
    let id = sha256(&v2);
    let list = TreeListL2::open_sidecar(Some(l2.clone()), &id).unwrap();
    assert_eq!(list.tree_id(), &id);
    assert_eq!(list.projection_digest(), id);

    // Layout: the header, then each section at the next multiple of 8 with zeros between.
    assert_eq!(&l2[..8], b"TSL2TREE");
    let units = [32, 32, 48, 32, 1, 1, 24, 1];
    let mut at = 192usize;
    for (index, unit) in units.iter().enumerate() {
        let (offset, count) = section(&l2, index);
        assert_eq!(
            offset,
            at.div_ceil(8) * 8,
            "section {index} begins at align8"
        );
        assert!(l2[at..offset].iter().all(|byte| *byte == 0));
        at = offset + count as usize * unit;
    }
    assert_eq!(at, l2.len(), "nothing after the extensions");
    assert_eq!(section(&l2, 7).1, 0, "no extension records");

    // The sections of the version 2 list are copied: entries, sha256, keys, names, ranges.
    for (v2_index, l2_index) in [(0, 0), (1, 1), (3, 4), (4, 5), (5, 6)] {
        let (from, len) = v2_section(&v2, v2_index);
        let (to, count) = section(&l2, l2_index);
        assert_eq!(count as usize * units[l2_index], len);
        assert_eq!(l2[to..to + len], v2[from..from + len]);
    }

    // The packs and their cells.
    assert_eq!(list.pack_count() as usize, tree.packs.len());
    let mut cell_first = 0u64;
    for (index, (key, bytes)) in tree.packs.iter().enumerate() {
        let index = index as u32;
        let cells = bytes.len().div_ceil(MIB);
        assert_eq!(
            list.pack_record(index),
            Some(PackRecord {
                key: PackKey(key.clone()),
                pack_size: bytes.len() as u64,
                cell_first,
                cell_count: cells as u32,
                cell_shift: 20,
                pack_flags: expected_flags(tree, key),
            })
        );
        assert_eq!(list.pack_key(index), Some(key.as_str()));
        assert_eq!(list.upload_part_size(index), Some(0));
        let mut joined = Vec::new();
        for (cell, slice) in bytes.chunks(MIB).enumerate() {
            let digest = sha256(slice);
            assert_eq!(list.cell_digest(index, cell as u32), Some(digest));
            joined.extend_from_slice(&digest);
        }
        assert_eq!(list.cell_digest(index, cells as u32), None);
        let mut root = b"TSPACK1".to_vec();
        root.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        root.push(20);
        root.extend_from_slice(&joined);
        assert_eq!(list.pack_root(index), Some(PackRoot::Digest(sha256(&root))));
        cell_first += cells as u64;
    }
    assert_eq!(list.pack_record(tree.packs.len() as u32), None);

    // The tree: every entry written is there, with its kind, mode, bytes and digest.
    let ranges: Vec<_> = list.ranges().collect();
    for entry in &tree.entries {
        let node = list.node(lookup(&list, &entry.path)).unwrap();
        match &entry.kind {
            ListKind::File {
                mode,
                size,
                source,
                sha256: digest,
            } => {
                assert_eq!(node.mode, mode.bits(), "{}", entry.path);
                let expected_digest = digest.map_or_else(|| sha256(b""), |d| *d.as_bytes());
                assert_eq!(
                    list.file_sha256(lookup(&list, &entry.path)),
                    Some(expected_digest)
                );
                match source {
                    None => assert_eq!(
                        node.kind,
                        NodeKind::File {
                            pack: None,
                            offset: 0,
                            size: 0
                        }
                    ),
                    Some(source) => {
                        let pack = tree
                            .packs
                            .keys()
                            .position(|key| key.as_str() == source.key.as_ref())
                            .unwrap() as u32;
                        assert_eq!(
                            node.kind,
                            NodeKind::File {
                                pack: Some(pack),
                                offset: source.offset,
                                size: *size
                            },
                            "{}",
                            entry.path
                        );
                        assert!(
                            ranges.iter().any(|range| range.pack == pack
                                && range.offset <= source.offset
                                && source.offset + size <= range.offset + range.len),
                            "a range holds {}",
                            entry.path
                        );
                    }
                }
            }
            ListKind::Dir { mode } => {
                assert_eq!(node.mode, mode.bits());
                assert!(matches!(node.kind, NodeKind::Dir { .. }));
            }
            ListKind::Symlink { target } => assert_eq!(
                node.kind,
                NodeKind::Symlink {
                    target: target.as_bytes()
                }
            ),
        }
    }
    list
}

/// The `pack_flags` of the only pack of a tree of one pack of 100 bytes whose files are `spans`
/// (offset, length).
fn flags_of_spans(spans: &[(u64, u64)]) -> u8 {
    let mut tree = Tree::default().pack("out/p", pack_bytes(9, 100));
    for (index, (offset, len)) in spans.iter().enumerate() {
        tree = tree.file(&format!("f{index}"), "out/p", *offset, *len);
    }
    let list = check_round_trip(&tree);
    list.pack_record(0).unwrap().pack_flags
}

#[test]
fn cell_hasher_of_no_bytes_has_size_zero_and_no_digests() {
    let hasher = CellHasher::new();
    assert_eq!(hasher.size(), 0);
    assert_eq!(
        hasher.finish(),
        PackCells {
            size: 0,
            digests: vec![]
        }
    );
}

#[test]
fn cell_hasher_empty_update_changes_nothing() {
    let mut hasher = CellHasher::new();
    hasher.update(&[]);
    assert_eq!(hasher.size(), 0);
    hasher.update(b"ab");
    hasher.update(&[]);
    assert_eq!(hasher.size(), 2);
    assert_eq!(
        hasher.finish(),
        PackCells {
            size: 2,
            digests: vec![sha256(b"ab")]
        }
    );
}

#[test]
fn cell_hasher_one_byte_is_one_short_cell() {
    let mut hasher = CellHasher::new();
    hasher.update(&[0x42]);
    assert_eq!(hasher.size(), 1);
    assert_eq!(
        hasher.finish(),
        PackCells {
            size: 1,
            digests: vec![sha256(&[0x42])]
        }
    );
}

#[test]
fn cell_hasher_exactly_one_mib_is_one_digest() {
    let bytes = pack_bytes(1, MIB);
    let mut hasher = CellHasher::new();
    hasher.update(&bytes);
    assert_eq!(hasher.size(), MIB as u64);
    assert_eq!(
        hasher.finish(),
        PackCells {
            size: MIB as u64,
            digests: vec![sha256(&bytes)]
        }
    );
}

#[test]
fn cell_hasher_one_mib_and_a_byte_is_two_digests_the_second_of_one_byte() {
    let bytes = pack_bytes(2, MIB + 1);
    let mut hasher = CellHasher::new();
    hasher.update(&bytes);
    assert_eq!(
        hasher.finish(),
        PackCells {
            size: MIB as u64 + 1,
            digests: vec![sha256(&bytes[..MIB]), sha256(&bytes[MIB..])]
        }
    );
}

#[test]
fn cell_hasher_two_mib_in_one_update_has_no_empty_third_cell() {
    let bytes = pack_bytes(3, 2 * MIB);
    let mut hasher = CellHasher::new();
    hasher.update(&bytes);
    assert_eq!(
        hasher.finish(),
        PackCells {
            size: 2 * MIB as u64,
            digests: vec![sha256(&bytes[..MIB]), sha256(&bytes[MIB..])]
        }
    );
}

#[test]
fn cell_hasher_pieces_straddling_cell_boundaries_match_the_whole_and_independent_sha256() {
    let bytes = pack_bytes(4, 3 * MIB + MIB / 2);
    let mut hasher = CellHasher::new();
    let mut rest = &bytes[..];
    let mut fed = 0u64;
    for piece in [1, 7, MIB - 1, MIB + 1] {
        hasher.update(&rest[..piece]);
        rest = &rest[piece..];
        fed += piece as u64;
        assert_eq!(hasher.size(), fed, "size() as bytes are fed");
    }
    hasher.update(rest);
    assert_eq!(hasher.size(), bytes.len() as u64);
    let cells = hasher.finish();
    assert_eq!(cells, PackCells::of(&bytes));
    assert_eq!(
        cells.digests,
        vec![
            sha256(&bytes[..MIB]),
            sha256(&bytes[MIB..2 * MIB]),
            sha256(&bytes[2 * MIB..3 * MIB]),
            sha256(&bytes[3 * MIB..]),
        ]
    );
    assert_eq!(cells.size, bytes.len() as u64);
}

#[test]
fn cell_hasher_one_byte_at_a_time_over_a_boundary_matches_the_whole() {
    let bytes = pack_bytes(6, MIB + 3);
    let mut hasher = CellHasher::new();
    for byte in &bytes[..MIB + 3] {
        hasher.update(std::slice::from_ref(byte));
    }
    assert_eq!(hasher.finish(), PackCells::of(&bytes));
}

#[test]
fn cell_hasher_default_is_new() {
    let (fresh, default) = (CellHasher::new(), CellHasher::default());
    assert_eq!(format!("{fresh:?}"), format!("{default:?}"));
    assert_eq!(default.size(), 0);
    assert_eq!(default.finish(), CellHasher::new().finish());
    let mut a = CellHasher::new();
    let mut b = CellHasher::default();
    a.update(b"same");
    b.update(b"same");
    assert_eq!(a.finish(), b.finish());
}

#[test]
fn cell_hasher_debug_shows_size_and_closed_cells() {
    let mut hasher = CellHasher::new();
    assert_eq!(
        format!("{hasher:?}"),
        "CellHasher { size: 0, cells: 0, .. }"
    );
    hasher.update(&pack_bytes(1, MIB + 5));
    assert_eq!(
        format!("{hasher:?}"),
        "CellHasher { size: 1048581, cells: 1, .. }"
    );
}

#[test]
fn pack_cells_of_no_bytes_is_the_default() {
    assert_eq!(PackCells::of(&[]), PackCells::default());
}

#[test]
fn sidecar_key_is_cas_l2_and_the_lowercase_hex_of_the_id() {
    assert_eq!(SIDECAR_PREFIX, "cas/l2/");
    let mut id = [0u8; 32];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(9).wrapping_add(0xa);
    }
    let expected: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(expected.len(), 64);
    assert_eq!(sidecar_key(&id), format!("cas/l2/{expected}"));
    assert_eq!(
        sidecar_key(&[0xab; 32]),
        format!("cas/l2/{}", "ab".repeat(32))
    );
    assert_eq!(
        sidecar_key(&[0; 32]),
        "cas/l2/0000000000000000000000000000000000000000000000000000000000000000"
    );
}

#[test]
fn sidecar_from_v2_same_inputs_twice_give_the_same_bytes() {
    let tree = small_tree();
    let v2 = tree.v2();
    let first = sidecar_from_v2(&v2, |key| tree.info(key)).unwrap();
    let second = sidecar_from_v2(&v2, |key| tree.info(key)).unwrap();
    assert_eq!(first, second);
}

#[test]
fn sidecar_from_v2_entries_in_another_order_give_the_same_v2_and_l2() {
    let tree = small_tree();
    let mut reversed = tree.clone();
    reversed.entries.reverse();
    assert_ne!(tree.entries[0].path, reversed.entries[0].path);
    assert_eq!(tree.v2(), reversed.v2());
    assert_eq!(tree.lists().1, reversed.lists().1);
}

#[test]
fn sidecar_from_v2_asks_for_each_key_once_in_key_order() {
    let tree = small_tree();
    let mut asked = Vec::new();
    sidecar_from_v2(&tree.v2(), |key| {
        asked.push(key.to_owned());
        tree.info(key)
    })
    .unwrap();
    assert_eq!(asked, ["out/a/0", "out/b/0"]);
}

#[test]
fn round_trip_small_tree() {
    let list = check_round_trip(&small_tree());
    assert_eq!(list.pack_count(), 2);
    // The 2.5 MiB pack has a gap (bytes MiB+10..MiB+4096 and the last 0.5 MiB), the other is whole.
    assert_eq!(list.pack_record(0).unwrap().pack_flags, 0);
    assert_eq!(list.pack_record(1).unwrap().pack_flags, 1);
    assert_eq!(list.pack_record(0).unwrap().cell_count, 3);
}

#[test]
fn round_trip_tree_of_only_the_root() {
    let list = check_round_trip(&Tree::default());
    assert_eq!(list.pack_count(), 0);
    assert_eq!(list.pack_record(0), None);
    assert_eq!(list.len(), 1);
}

#[test]
fn round_trip_tree_of_the_root_and_an_empty_dir() {
    let list = check_round_trip(&Tree::default().dir("empty", 0o700));
    assert_eq!(list.pack_count(), 0);
    assert_eq!(list.len(), 2);
}

#[test]
fn round_trip_tree_with_many_packs_files_dirs_and_links() {
    let sizes = [10, MIB, MIB + 1, 300_000, 2 * MIB + 5];
    let mut tree = Tree::default();
    for (index, size) in sizes.iter().enumerate() {
        tree = tree.pack(&format!("out/p{index}/0"), pack_bytes(index as u8, *size));
    }
    for (index, size) in sizes.iter().enumerate() {
        let quarter = size / 4;
        for part in 0..4usize {
            // Pack 3 has no file over its third quarter, so it has a gap.
            if index == 3 && part == 2 {
                continue;
            }
            let start = part * quarter;
            let end = if part == 3 { *size } else { start + quarter };
            tree = tree.file(
                &format!("d{index}/sub{part}/f{part}.bin"),
                &format!("out/p{index}/0"),
                start as u64,
                (end - start) as u64,
            );
        }
    }
    let tree = tree
        .empty_file("d0/empty")
        .link("d0/link", "sub0/f0.bin")
        .link("top", "d1")
        .dir("emptydir", 0o700)
        .dir("d2/ex", 0o2755);
    let list = check_round_trip(&tree);
    assert_eq!(list.pack_count(), 5);
    let flags: Vec<u8> = (0..5)
        .map(|i| list.pack_record(i).unwrap().pack_flags)
        .collect();
    assert_eq!(flags, [1, 1, 1, 0, 1]);
}

#[test]
fn round_trip_pack_covered_exactly() {
    let tree = Tree::default()
        .pack("out/p", pack_bytes(1, 3 * MIB))
        .file("a", "out/p", 0, MIB as u64)
        .file("b", "out/p", MIB as u64, 2 * MIB as u64);
    let list = check_round_trip(&tree);
    assert_eq!(list.pack_record(0).unwrap().pack_flags, 1);
}

#[test]
fn round_trip_pack_with_a_gap() {
    let tree = Tree::default()
        .pack("out/p", pack_bytes(1, 100))
        .file("a", "out/p", 0, 40)
        .file("b", "out/p", 41, 59);
    let list = check_round_trip(&tree);
    assert_eq!(list.pack_record(0).unwrap().pack_flags, 0);
}

#[test]
fn round_trip_files_sharing_one_span() {
    let tree = Tree::default()
        .pack("out/p", pack_bytes(1, 100))
        .file("a", "out/p", 0, 100)
        .file("dir/b", "out/p", 0, 100)
        .file("dir/c", "out/p", 10, 20);
    let list = check_round_trip(&tree);
    assert_eq!(list.pack_record(0).unwrap().pack_flags, 1);
}

#[test]
fn round_trip_whole_object_key_cas_l_hex() {
    let key = format!("cas/l/{}", "ab".repeat(32));
    let tree = Tree::default()
        .pack(&key, pack_bytes(7, 4096))
        .file("whole", &key, 0, 4096);
    let list = check_round_trip(&tree);
    assert_eq!(list.pack_key(0), Some(key.as_str()));
    assert_eq!(list.pack_record(0).unwrap().pack_flags, 1);
}

#[test]
fn round_trip_key_of_1024_bytes_is_accepted() {
    let key = format!("k{}", "x".repeat(1023));
    assert_eq!(key.len(), 1024);
    let tree = Tree::default()
        .pack(&key, pack_bytes(7, 64))
        .file("f", &key, 0, 64);
    let list = check_round_trip(&tree);
    assert_eq!(list.pack_key(0), Some(key.as_str()));
}

#[test]
fn round_trip_pack_of_a_file_with_one_empty_file_beside_it() {
    let tree = Tree::default()
        .pack("out/p", pack_bytes(1, 10))
        .empty_file("e")
        .file("f", "out/p", 0, 10);
    let list = check_round_trip(&tree);
    assert_eq!(list.pack_record(0).unwrap().pack_flags, 1);
}

#[test]
fn pack_flags_exact_cover_sets_bit_zero() {
    assert_eq!(flags_of_spans(&[(0, 100)]), 1);
    assert_eq!(flags_of_spans(&[(0, 50), (50, 50)]), 1);
}

#[test]
fn pack_flags_gap_at_the_start_clears_bit_zero() {
    assert_eq!(flags_of_spans(&[(10, 90)]), 0);
}

#[test]
fn pack_flags_gap_in_the_middle_clears_bit_zero() {
    assert_eq!(flags_of_spans(&[(0, 40), (60, 40)]), 0);
}

#[test]
fn pack_flags_gap_at_the_end_clears_bit_zero() {
    assert_eq!(flags_of_spans(&[(0, 90)]), 0);
}

#[test]
fn pack_flags_overlapping_spans_that_cover_set_bit_zero() {
    assert_eq!(flags_of_spans(&[(0, 60), (40, 60)]), 1);
    assert_eq!(flags_of_spans(&[(40, 60), (0, 60)]), 1);
}

#[test]
fn pack_flags_a_short_span_inside_a_long_one_does_not_shrink_the_cover() {
    assert_eq!(flags_of_spans(&[(0, 100), (10, 20)]), 1);
    assert_eq!(flags_of_spans(&[(10, 20), (0, 100)]), 1);
}

#[test]
fn pack_flags_duplicate_spans() {
    assert_eq!(flags_of_spans(&[(0, 100), (0, 100)]), 1);
    assert_eq!(flags_of_spans(&[(0, 50), (0, 50)]), 0);
}

#[test]
fn pack_flags_adjacent_spans_with_no_overlap_cover() {
    assert_eq!(flags_of_spans(&[(0, 1), (1, 98), (99, 1)]), 1);
}

#[test]
fn pack_flags_shared_sets_bit_one_beside_the_cover_bit() {
    assert_eq!(PACK_SHARED, 2);
    let tree = small_tree();
    let l2 = sidecar_from_v2(&tree.v2(), |key| {
        tree.info(key).map(|mut info| {
            info.shared = key == "out/a/0";
            info
        })
    })
    .unwrap();
    let list = TreeListL2::open(l2).unwrap();
    // `out/a/0` has a gap and is shared; `out/b/0` is covered and not shared.
    assert_eq!(list.pack_record(0).unwrap().pack_flags, 2);
    assert_eq!(list.pack_record(1).unwrap().pack_flags, 1);
}

#[test]
fn pack_flags_shared_and_covered_is_three() {
    let tree = one_file_tree();
    let tree = tree
        .pack("out/q", pack_bytes(6, 20))
        .file("g", "out/q", 0, 20);
    let l2 = sidecar_from_v2(&tree.v2(), |key| {
        tree.info(key).map(|mut info| {
            info.shared = true;
            info
        })
    })
    .unwrap();
    let list = TreeListL2::open(l2).unwrap();
    // `out/p` is covered for 50 of 100 bytes, `out/q` whole.
    assert_eq!(list.pack_record(0).unwrap().pack_flags, 2);
    assert_eq!(list.pack_record(1).unwrap().pack_flags, 3);
}

#[test]
fn upload_part_size_is_copied_into_the_pack_record() {
    let tree = small_tree();
    let l2 = sidecar_from_v2(&tree.v2(), |key| {
        tree.info(key).map(|mut info| {
            if key == "out/b/0" {
                info.upload_part_size = 5 * MIB as u64 + 3;
            }
            info
        })
    })
    .unwrap();
    let list = TreeListL2::open(l2).unwrap();
    assert_eq!(list.upload_part_size(0), Some(0));
    assert_eq!(list.upload_part_size(1), Some(5 * MIB as u64 + 3));
    assert_eq!(list.upload_part_size(2), None);
}

#[test]
fn packs_and_cells_of_a_hand_made_pack_info_are_written_as_given() {
    // The digests are written as the caller gives them, not recomputed.
    let tree = one_file_tree();
    let digest = [0x5a; 32];
    let l2 = sidecar_from_v2(&tree.v2(), |_| {
        Some(PackInfo {
            cells: PackCells {
                size: 100,
                digests: vec![digest],
            },
            upload_part_size: 0,
            shared: false,
        })
    })
    .unwrap();
    let list = TreeListL2::open(l2).unwrap();
    assert_eq!(list.cell_digest(0, 0), Some(digest));
}
