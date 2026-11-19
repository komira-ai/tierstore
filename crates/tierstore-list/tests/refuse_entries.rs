//! Every way `TreeListL2::open` refuses an entry: the root, an unknown kind, a directory nothing
//! holds, children not where the breadth-first order puts them, a wrong parent, a name outside the
//! names or not a name (empty, `.`, `..`, with `/` or NUL, not UTF-8), siblings out of order or
//! equal, a path over 4096 bytes, a file in no pack or past its pack's end, an empty file with a
//! digest, a link target that is empty, too long, outside the names, not UTF-8 or with a NUL, and
//! entries no directory holds. Each test patches the entry records, names or digests of a valid
//! list and asserts the whole error value, with the number of the entry at fault.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{Tree, recrc, section, small_tree};
use tierstore_fetch_api::list::NodeIndex;
use tierstore_list::{EntryFault, Malformed, OpenError, TreeListL2};

const ENTRY: usize = 32;

fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// `put_uNN` with the offset computed first: the offset is read from the bytes being edited.
macro_rules! p16 {
    ($bytes:expr, $at:expr, $value:expr) => {{
        let at = $at;
        put_u16($bytes, at, $value)
    }};
}
macro_rules! p32 {
    ($bytes:expr, $at:expr, $value:expr) => {{
        let at = $at;
        put_u32($bytes, at, $value)
    }};
}
macro_rules! p64 {
    ($bytes:expr, $at:expr, $value:expr) => {{
        let at = $at;
        put_u64($bytes, at, $value)
    }};
}

fn get_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// The offset of entry record `index`. Fields: parent 0, name offset 4, pack or first child 8, kind
/// and mode 12 (u16), name length 14, offset or child count 16, size or target length 24.
fn entry_at(l2: &[u8], index: usize) -> usize {
    section(l2, 0).0 + ENTRY * index
}

/// The number of the entry named `name` in `l2` (names are unique in the trees below).
fn index_of(l2: &[u8], name: &[u8]) -> usize {
    let list = TreeListL2::open(l2.to_vec()).unwrap();
    (0..u64::from(list.len()))
        .find(|index| list.node(NodeIndex(*index)).unwrap().name == name)
        .unwrap() as usize
}

/// Where the names begin.
fn names_at(l2: &[u8]) -> usize {
    section(l2, 5).0
}

fn refused(l2: Vec<u8>) -> OpenError {
    TreeListL2::open(l2).unwrap_err()
}

fn entry_fault(index: u32, fault: EntryFault) -> OpenError {
    OpenError::Malformed(Malformed::Entry { index, fault })
}

/// The L2 list of `tree`, edited by `edit`.
fn edited(tree: &Tree, edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let (_, mut l2) = tree.lists();
    edit(&mut l2);
    recrc(&mut l2);
    l2
}

/// A tree of an empty file `a`, an empty file `b`, a link `c` to a target no name has, and a
/// directory `d` of no children: entries 1 to 4.
fn flat() -> Tree {
    Tree::default()
        .empty_file("a")
        .empty_file("b")
        .link("c", "TARGETTEXT")
        .dir("d", 0o755)
}

/// The position of `needle` in the names of `l2`.
fn name_pos(l2: &[u8], needle: &[u8]) -> usize {
    let at = names_at(l2);
    at + l2[at..]
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap()
}

// ---- the root ----------------------------------------------------------------------------------

#[test]
fn a_root_that_is_not_a_directory_is_root() {
    for kind_mode in [0u16, 2 << 12, 3 << 12, 0xf000 | 0o755] {
        let l2 = edited(&small_tree(), |l2| {
            p16!(l2, entry_at(l2, 0) + 12, kind_mode);
        });
        assert_eq!(
            refused(l2),
            entry_fault(0, EntryFault::Root),
            "kind_mode {kind_mode:#x}"
        );
    }
}

#[test]
fn a_root_whose_parent_is_not_0_is_root() {
    let l2 = edited(&small_tree(), |l2| p32!(l2, entry_at(l2, 0), 1));
    assert_eq!(refused(l2), entry_fault(0, EntryFault::Root));
}

#[test]
fn a_root_with_a_name_is_root() {
    let l2 = edited(&small_tree(), |l2| {
        let at = entry_at(l2, 0) + 14;
        l2[at] = 1;
    });
    assert_eq!(refused(l2), entry_fault(0, EntryFault::Root));
}

// ---- kinds, children, parents, orphans ---------------------------------------------------------

#[test]
fn an_unknown_kind_is_refused_with_the_kind() {
    for kind in [3u16, 4, 15] {
        let l2 = edited(&small_tree(), |l2| {
            p16!(l2, entry_at(l2, 1) + 12, kind << 12 | 0o644);
        });
        assert_eq!(
            refused(l2),
            entry_fault(1, EntryFault::UnknownKind(kind as u8)),
            "kind {kind}"
        );
    }
}

#[test]
fn a_child_whose_parent_is_another_directory_is_parent_with_that_number() {
    let l2 = edited(&small_tree(), |l2| p32!(l2, entry_at(l2, 2), 7));
    assert_eq!(refused(l2), entry_fault(2, EntryFault::Parent(7)));
    let l2 = edited(&small_tree(), |l2| p32!(l2, entry_at(l2, 5), 0));
    assert_eq!(refused(l2), entry_fault(5, EntryFault::Parent(0)));
}

#[test]
fn a_child_whose_parent_is_a_number_past_the_entries_is_parent() {
    let l2 = edited(&small_tree(), |l2| p32!(l2, entry_at(l2, 4), u32::MAX));
    assert_eq!(refused(l2), entry_fault(4, EntryFault::Parent(u32::MAX)));
}

#[test]
fn a_directory_that_is_the_child_of_a_wrong_parent_is_parent_before_its_kind_is_judged() {
    // Parent is judged first: entry 1's parent 2 and kind 3 are reported as the parent.
    let l2 = edited(&small_tree(), |l2| {
        p32!(l2, entry_at(l2, 1), 2);
        p16!(l2, entry_at(l2, 1) + 12, 3 << 12);
    });
    assert_eq!(refused(l2), entry_fault(1, EntryFault::Parent(2)));
}

#[test]
fn a_root_whose_first_child_is_not_entry_1_is_children() {
    for first in [0u32, 2, 9] {
        let l2 = edited(&small_tree(), |l2| p32!(l2, entry_at(l2, 0) + 8, first));
        assert_eq!(
            refused(l2),
            entry_fault(0, EntryFault::Children),
            "first {first}"
        );
    }
}

#[test]
fn a_directory_whose_children_run_past_the_last_entry_is_children() {
    // The root has 3 children and 9 entries after it; one more than the entries left.
    let l2 = edited(&small_tree(), |l2| p64!(l2, entry_at(l2, 0) + 16, 10));
    assert_eq!(refused(l2), entry_fault(0, EntryFault::Children));
    let l2 = edited(&small_tree(), |l2| {
        p64!(l2, entry_at(l2, 0) + 16, u64::MAX);
    });
    assert_eq!(refused(l2), entry_fault(0, EntryFault::Children));
}

#[test]
fn a_directory_whose_first_child_is_not_after_the_children_before_it_is_children() {
    let (_, l2) = small_tree().lists();
    let lib = index_of(&l2, b"lib");
    let first = get_u32(&l2, entry_at(&l2, lib) + 8);
    for wrong in [first - 1, first + 1] {
        let l2 = edited(&small_tree(), |l2| {
            p32!(l2, entry_at(l2, lib) + 8, wrong);
        });
        assert_eq!(
            refused(l2),
            entry_fault(lib as u32, EntryFault::Children),
            "first {wrong}"
        );
    }
}

#[test]
fn a_directory_that_no_directory_holds_is_unreachable() {
    // The root holds nothing: the first entry, a directory, is before any children.
    let l2 = edited(&small_tree(), |l2| p64!(l2, entry_at(l2, 0) + 16, 0));
    assert_eq!(refused(l2), entry_fault(1, EntryFault::Unreachable));
}

#[test]
fn entries_no_directory_holds_are_orphans_from_the_first_of_them() {
    let files = || {
        Tree::default()
            .empty_file("x")
            .empty_file("y")
            .empty_file("z")
    };
    let l2 = edited(&files(), |l2| p64!(l2, entry_at(l2, 0) + 16, 2));
    assert_eq!(refused(l2), OpenError::Malformed(Malformed::Orphans(3)));
    let l2 = edited(&files(), |l2| p64!(l2, entry_at(l2, 0) + 16, 1));
    assert_eq!(refused(l2), OpenError::Malformed(Malformed::Orphans(2)));
}

#[test]
fn an_orphan_that_is_a_directory_is_unreachable_not_an_orphan() {
    // flat(): the root, a, b, c, and the directory d at 4, which the root, told it has 2 children,
    // does not hold.
    let l2 = edited(&flat(), |l2| p64!(l2, entry_at(l2, 0) + 16, 2));
    assert_eq!(refused(l2), entry_fault(4, EntryFault::Unreachable));
}

// ---- names -------------------------------------------------------------------------------------

#[test]
fn a_name_past_the_end_of_the_names_is_name_bounds() {
    for offset in [u32::MAX, 1 << 24] {
        let l2 = edited(&flat(), |l2| p32!(l2, entry_at(l2, 1) + 4, offset));
        assert_eq!(
            refused(l2),
            entry_fault(1, EntryFault::NameBounds),
            "offset {offset}"
        );
    }
}

#[test]
fn a_name_that_begins_inside_the_names_and_ends_past_them_is_name_bounds() {
    let (_, l2) = flat().lists();
    let names = section(&l2, 5).1 as u32;
    let l2 = edited(&flat(), |l2| {
        p32!(l2, entry_at(l2, 1) + 4, names - 1);
        {
            let at = entry_at(l2, 1) + 14;
            l2[at] = 2;
        }
    });
    assert_eq!(refused(l2), entry_fault(1, EntryFault::NameBounds));
}

#[test]
fn a_name_of_no_bytes_is_bad_name() {
    let l2 = edited(&flat(), |l2| {
        let at = entry_at(l2, 1) + 14;
        l2[at] = 0;
    });
    assert_eq!(refused(l2), entry_fault(1, EntryFault::BadName));
}

/// The list of one empty file named `path` whose name bytes are set to `name` (as long as `path`).
fn renamed(path: &str, name: &[u8]) -> Vec<u8> {
    assert_eq!(path.len(), name.len());
    edited(&Tree::default().empty_file(path), |l2| {
        let at = name_pos(l2, path.as_bytes());
        l2[at..at + name.len()].copy_from_slice(name);
    })
}

#[test]
fn the_names_dot_and_dot_dot_are_bad_name() {
    assert_eq!(
        refused(renamed("x", b".")),
        entry_fault(1, EntryFault::BadName)
    );
    assert_eq!(
        refused(renamed("xy", b"..")),
        entry_fault(1, EntryFault::BadName)
    );
}

#[test]
fn names_that_only_look_like_dot_names_are_not_refused() {
    for name in [&b"..."[..], b".a", b"a.", b"..a", b"a.."] {
        let path = "x".repeat(name.len());
        assert!(TreeListL2::open(renamed(&path, name)).is_ok(), "{name:?}");
    }
}

#[test]
fn a_name_with_a_slash_is_bad_name() {
    assert_eq!(
        refused(renamed("xyz", b"a/b")),
        entry_fault(1, EntryFault::BadName)
    );
    assert_eq!(
        refused(renamed("x", b"/")),
        entry_fault(1, EntryFault::BadName)
    );
}

#[test]
fn a_name_with_a_nul_is_bad_name() {
    assert_eq!(
        refused(renamed("xyz", b"a\0b")),
        entry_fault(1, EntryFault::BadName)
    );
    assert_eq!(
        refused(renamed("x", b"\0")),
        entry_fault(1, EntryFault::BadName)
    );
}

#[test]
fn a_name_that_is_not_utf8_is_bad_name() {
    assert_eq!(
        refused(renamed("xyz", &[0xc3, 0x28, 0x41])),
        entry_fault(1, EntryFault::BadName)
    );
    assert_eq!(
        refused(renamed("x", &[0xff])),
        entry_fault(1, EntryFault::BadName)
    );
}

#[test]
fn an_entry_may_name_a_longer_run_of_the_bytes_another_entrys_name_is_in() {
    // Entry 2 names the bytes of entry 1's name with a longer length that runs into the next name
    // `b`: "ab" is a good name, and sorts after "a".
    let (_, l2) = flat().lists();
    let x = get_u32(&l2, entry_at(&l2, 1) + 4);
    let y = get_u32(&l2, entry_at(&l2, 2) + 4);
    assert_eq!(y, x + 1);
    let l2 = edited(&flat(), |l2| {
        p32!(l2, entry_at(l2, 2) + 4, x);
        {
            let at = entry_at(l2, 2) + 14;
            l2[at] = 2;
        }
    });
    assert!(TreeListL2::open(l2).is_ok());
}

#[test]
fn siblings_with_the_same_name_are_not_ascending() {
    let l2 = edited(&flat(), |l2| {
        let first = get_u32(l2, entry_at(l2, 1) + 4);
        p32!(l2, entry_at(l2, 2) + 4, first);
    });
    assert_eq!(refused(l2), entry_fault(2, EntryFault::NotAscending));
}

#[test]
fn siblings_out_of_order_are_not_ascending() {
    let l2 = edited(&flat(), |l2| {
        let (a, b) = (entry_at(l2, 1) + 4, entry_at(l2, 2) + 4);
        let (first, second) = (get_u32(l2, a), get_u32(l2, b));
        p32!(l2, a, second);
        p32!(l2, b, first);
    });
    assert_eq!(refused(l2), entry_fault(2, EntryFault::NotAscending));
}

#[test]
fn a_first_child_sorts_after_the_empty_name_and_a_prefix_sorts_before_its_extension() {
    // "ab" after "a" is ascending (checked above); "a" after "ab" is not.
    let (_, l2) = flat().lists();
    let x = get_u32(&l2, entry_at(&l2, 1) + 4);
    let l2 = edited(&flat(), |l2| {
        // Entry 1 is "ab" and entry 2 is "b": make entry 2 "a": "ab" >= "a".
        {
            let at = entry_at(l2, 1) + 14;
            l2[at] = 2;
        }
        p32!(l2, entry_at(l2, 2) + 4, x);
        {
            let at = entry_at(l2, 2) + 14;
            l2[at] = 1;
        }
    });
    assert_eq!(refused(l2), entry_fault(2, EntryFault::NotAscending));
}

#[test]
fn names_that_repeat_in_different_directories_are_not_siblings() {
    let tree = Tree::default()
        .dir("a", 0o755)
        .dir("b", 0o755)
        .empty_file("a/f")
        .empty_file("b/f");
    let (_, l2) = tree.lists();
    assert!(TreeListL2::open(l2).is_ok());
}

// ---- paths -------------------------------------------------------------------------------------

/// A chain of directories, each the only child of the one before, whose names have the lengths in
/// `lens`; the names are made of different letters so that none repeats.
fn chain(lens: &[usize]) -> Tree {
    let mut tree = Tree::default();
    let mut path = String::new();
    for (at, len) in lens.iter().enumerate() {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(&char::from(b'a' + at as u8).to_string().repeat(*len));
        tree = tree.dir(&path, 0o755);
    }
    tree
}

#[test]
fn a_path_of_exactly_4096_bytes_is_not_refused() {
    // 17 names of 4080 bytes together and 16 slashes.
    let mut lens = vec![255; 15];
    lens.extend([128, 127]);
    let (_, l2) = chain(&lens).lists();
    let list = TreeListL2::open(l2).unwrap();
    assert_eq!(list.shape().max_depth, 17);
}

#[test]
fn a_path_of_4097_bytes_is_path_too_long_at_the_entry_that_has_it() {
    let mut lens = vec![255; 15];
    lens.extend([128, 128]);
    let (_, l2) = chain(&lens).lists();
    assert_eq!(refused(l2), entry_fault(17, EntryFault::PathTooLong));
}

#[test]
fn a_long_path_is_judged_at_every_depth_not_only_the_last() {
    // 16 names of 255: the 17th, of 255 more, is at 4351.
    let (_, l2) = chain(&[255; 17]).lists();
    assert_eq!(refused(l2), entry_fault(17, EntryFault::PathTooLong));
}

// ---- files -------------------------------------------------------------------------------------

#[test]
fn a_file_whose_pack_number_is_not_in_packs_is_pack_with_the_number() {
    for pack in [2u32, 3, u32::MAX] {
        let (_, l2) = small_tree().lists();
        let cc = index_of(&l2, b"cc");
        let l2 = edited(&small_tree(), |l2| p32!(l2, entry_at(l2, cc) + 8, pack));
        assert_eq!(
            refused(l2),
            entry_fault(cc as u32, EntryFault::Pack(pack)),
            "pack {pack}"
        );
    }
}

#[test]
fn a_file_whose_bytes_end_one_past_its_pack_is_outside_pack_and_one_that_ends_at_it_is_not() {
    let (_, l2) = small_tree().lists();
    let b = index_of(&l2, b"b.txt");
    // b.txt: 40 bytes at 60 of the 100-byte pack.
    let past = edited(&small_tree(), |l2| p64!(l2, entry_at(l2, b) + 16, 61));
    assert_eq!(
        refused(past),
        entry_fault(b as u32, EntryFault::OutsidePack)
    );
    let at_end = edited(&small_tree(), |l2| p64!(l2, entry_at(l2, b) + 16, 60));
    assert!(TreeListL2::open(at_end).is_ok());
}

#[test]
fn a_file_larger_than_its_pack_is_outside_pack() {
    let (_, l2) = small_tree().lists();
    let a = index_of(&l2, b"a.txt");
    let l2 = edited(&small_tree(), |l2| {
        p64!(l2, entry_at(l2, a) + 16, 0);
        p64!(l2, entry_at(l2, a) + 24, 101);
    });
    assert_eq!(refused(l2), entry_fault(a as u32, EntryFault::OutsidePack));
}

#[test]
fn a_file_whose_offset_plus_size_overflows_is_outside_pack() {
    let (_, l2) = small_tree().lists();
    let cc = index_of(&l2, b"cc");
    for (offset, size) in [(u64::MAX, 2u64), (2, u64::MAX), (u64::MAX, u64::MAX)] {
        let l2 = edited(&small_tree(), |l2| {
            p64!(l2, entry_at(l2, cc) + 16, offset);
            p64!(l2, entry_at(l2, cc) + 24, size);
        });
        assert_eq!(
            refused(l2),
            entry_fault(cc as u32, EntryFault::OutsidePack),
            "offset {offset}, size {size}"
        );
    }
}

#[test]
fn a_file_is_judged_against_its_own_pack_not_the_first() {
    // 0 .. 2.5 MiB is pack 0; b.txt, in the 100-byte pack 1, at 2 MiB is outside it.
    let (_, l2) = small_tree().lists();
    let b = index_of(&l2, b"b.txt");
    let l2 = edited(&small_tree(), |l2| {
        p64!(l2, entry_at(l2, b) + 16, 2 << 20);
    });
    assert_eq!(refused(l2), entry_fault(b as u32, EntryFault::OutsidePack));
}

#[test]
fn an_empty_file_whose_digest_is_not_that_of_no_bytes_is_empty_digest() {
    let (_, l2) = small_tree().lists();
    let empty = index_of(&l2, b"empty");
    for byte in [0usize, 15, 31] {
        let l2 = edited(&small_tree(), |l2| {
            let at = section(l2, 1).0 + 32 * empty + byte;
            l2[at] ^= 1;
        });
        assert_eq!(
            refused(l2),
            entry_fault(empty as u32, EntryFault::EmptyDigest),
            "byte {byte}"
        );
    }
    // A digest of all zeros is not the digest of no bytes either.
    let l2 = edited(&small_tree(), |l2| {
        let at = section(l2, 1).0 + 32 * empty;
        l2[at..at + 32].fill(0);
    });
    assert_eq!(
        refused(l2),
        entry_fault(empty as u32, EntryFault::EmptyDigest)
    );
}

#[test]
fn a_file_made_empty_by_its_size_has_to_have_the_empty_digest() {
    // bin/cc, size set to 0: its digest is that of its bytes, not of none.
    let (_, l2) = small_tree().lists();
    let cc = index_of(&l2, b"cc");
    let l2 = edited(&small_tree(), |l2| p64!(l2, entry_at(l2, cc) + 24, 0));
    assert_eq!(refused(l2), entry_fault(cc as u32, EntryFault::EmptyDigest));
}

// ---- link targets ------------------------------------------------------------------------------

/// The number of the link of `flat()`.
const Z: u32 = 3;

fn link_at(l2: &[u8]) -> usize {
    entry_at(l2, Z as usize)
}

#[test]
fn the_link_of_the_flat_tree_is_entry_3() {
    let (_, l2) = flat().lists();
    assert_eq!(index_of(&l2, b"c"), Z as usize);
    assert_eq!(index_of(&l2, b"d"), 4);
}

#[test]
fn a_link_target_of_no_bytes_is_target() {
    let l2 = edited(&flat(), |l2| {
        let at = link_at(l2);
        p64!(l2, at + 24, 0);
    });
    assert_eq!(refused(l2), entry_fault(Z, EntryFault::Target));
}

#[test]
fn a_link_target_past_the_end_of_the_names_is_target() {
    let (_, l2) = flat().lists();
    let names = section(&l2, 5).1;
    for (start, len) in [(names, 1u64), (names - 1, 2), (u64::MAX, 1), (1, u64::MAX)] {
        let l2 = edited(&flat(), |l2| {
            let at = link_at(l2);
            p64!(l2, at + 16, start);
            p64!(l2, at + 24, len);
        });
        assert_eq!(
            refused(l2),
            entry_fault(Z, EntryFault::Target),
            "start {start}, len {len}"
        );
    }
}

#[test]
fn a_link_target_whose_start_plus_length_overflows_is_target() {
    let l2 = edited(&flat(), |l2| {
        let at = link_at(l2);
        p64!(l2, at + 16, u64::MAX - 1);
        p64!(l2, at + 24, 4096);
    });
    assert_eq!(refused(l2), entry_fault(Z, EntryFault::Target));
}

#[test]
fn a_link_target_that_is_not_utf8_is_target() {
    let l2 = edited(&flat(), |l2| {
        let at = name_pos(l2, b"TARGETTEXT");
        l2[at..at + 2].copy_from_slice(&[0xc3, 0x28]);
    });
    assert_eq!(refused(l2), entry_fault(Z, EntryFault::Target));
}

#[test]
fn a_link_target_with_a_nul_is_target() {
    let l2 = edited(&flat(), |l2| {
        let at = name_pos(l2, b"TARGETTEXT");
        l2[at + 4] = 0;
    });
    assert_eq!(refused(l2), entry_fault(Z, EntryFault::Target));
}

#[test]
fn a_link_target_may_have_a_slash_and_dots_and_is_not_refused() {
    let tree = Tree::default().link("l", "../a/./b//c");
    let (_, l2) = tree.lists();
    assert!(TreeListL2::open(l2).is_ok());
}

#[test]
fn a_link_target_of_4096_bytes_is_not_refused_and_one_of_4097_is_target() {
    let ok = Tree::default().link("l", &"t".repeat(4096));
    let (_, l2) = ok.lists();
    assert!(TreeListL2::open(l2).is_ok());
    let long = Tree::default().link("l", &"t".repeat(4097));
    let (_, l2) = long.lists();
    assert_eq!(refused(l2), entry_fault(1, EntryFault::Target));
}

#[test]
fn a_link_target_is_judged_after_its_links_name_and_path() {
    // A link with a bad name and a bad target: the name is reported.
    let l2 = edited(&flat(), |l2| {
        let at = link_at(l2);
        p64!(l2, at + 24, 0);
        l2[at + 14] = 0;
    });
    assert_eq!(refused(l2), entry_fault(Z, EntryFault::BadName));
}
