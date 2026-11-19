//! The tree mount's namespace operations against a real tree (the rig of `tierstore-tree`) and no
//! kernel: raw requests go to [`TreeFs::handle`] and the replies are read from the kernel's end
//! of a socket pair. The harness is `tree_fs_harness.rs`. It proves:
//! - LOOKUP answers every entry's inode (index + 1), type bits, mode, size, link count, owner,
//!   fixed times and validity; a missing name is inode 0; an inode that is not an entry is
//!   `ENOENT`; GETATTR equals LOOKUP and is stable.
//! - READLINK answers a link's target and refuses a file (`EINVAL`).
//! - READDIR and READDIRPLUS list `.`, `..` and the children in name order with offsets of
//!   position + 1, resume at any offset, list a directory of any size across calls with no
//!   entry lost or repeated, and carry the child's entry in READDIRPLUS; a file is `ENOTDIR`.
//! - STATFS, OPEN and OPENDIR (always `ENOSYS`, counted), RELEASE, FLUSH, FSYNC, the writes
//!   (`EROFS`), an unknown opcode and a malformed body.
//! - A damaged image reads as `EIO` in a listing, never a panic.
//!
//! Catches: an inode off by one, a link count that forgets `.` or counts files, a type that does
//! not match its entry, a listing that repeats or drops an entry at a buffer's end, `.` or `..`
//! with an entry in READDIRPLUS, and an OPEN that is answered.

use std::os::unix::fs::FileExt as _;

use tierstore_fetch_api::list::NodeIndex;
use tierstore_list::{ENTRY_LEN, Section, TreeListL2};
use tsfuse::abi::{Attr, StatfsOut};

use super::TreeFsStats;
use super::tree_fs_harness::*;
use crate::server::treefs::core::{FIXED_MTIME, TREE_TTL};

const ENOENT: i32 = 2;
const EIO: i32 = 5;
const ENOTDIR: i32 = 20;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;
const ENOSYS: i32 = 38;

// ---- LOOKUP and GETATTR ----

/// The attributes the tests expect of each entry of [`sample`]: path, type bits with mode, size,
/// link count.
const SAMPLE_ATTRS: &[(&str, u32, u64, u32)] = &[
    ("bin", S_IFDIR | 0o755, 0, 2),
    ("bin/cc", S_IFREG | 0o644, MIB + 10, 1),
    ("bin/ld", S_IFREG | 0o644, MIB, 1),
    ("bin/x", S_IFREG | 0o755, 10, 1),
    ("lib", S_IFDIR | 0o750, 0, 2),
    ("lib/a.txt", S_IFREG | 0o644, 60, 1),
    ("lib/b.txt", S_IFREG | 0o644, 40, 1),
    ("lib/empty", S_IFREG | 0o644, 0, 1),
    ("lib/self", S_IFLNK | 0o777, 5, 1),
    ("tmp", S_IFDIR | 0o1777, 0, 2),
];

fn check_attr(attr: &Attr, ino: u64, mode: u32, size: u64, nlink: u32, what: &str) {
    assert_eq!(attr.ino, ino, "{what}: ino");
    assert_eq!(attr.mode, mode, "{what}: mode");
    assert_eq!(attr.size, size, "{what}: size");
    assert_eq!(attr.nlink, nlink, "{what}: nlink");
    assert_eq!((attr.uid, attr.gid), (1000, 1000), "{what}: owner");
    assert_eq!(
        (attr.atime, attr.mtime, attr.ctime),
        (FIXED_MTIME, FIXED_MTIME, FIXED_MTIME),
        "{what}: times"
    );
    assert_eq!(
        (attr.atimensec, attr.mtimensec, attr.ctimensec),
        (0, 0, 0),
        "{what}: nanoseconds"
    );
}

/// Asserts that LOOKUP answers every entry with its inode (its image index plus one), its type
/// bits and mode, size, a link count of 1 for files and links and 2 plus the subdirectories for
/// a directory, the mount's owner, the fixed times, and a validity of `TREE_TTL` for name and
/// attributes.
#[test]
fn lookup_answers_every_entry_with_inode_attributes_and_validity() {
    let mut h = Harness::new("lookup-attrs", &sample());
    for &(path, mode, size, nlink) in SAMPLE_ATTRS {
        let entry = h.resolve(path);
        let ino = h.index_of(path).0 + 1;
        assert_eq!(
            entry.nodeid, ino,
            "{path}: node id is the image index plus one"
        );
        assert_eq!(entry.generation, 0, "{path}: generation");
        assert_eq!(
            entry.entry_valid,
            TREE_TTL.as_secs(),
            "{path}: entry validity"
        );
        assert_eq!(
            entry.attr_valid,
            TREE_TTL.as_secs(),
            "{path}: attr validity"
        );
        assert_eq!((entry.entry_valid_nsec, entry.attr_valid_nsec), (0, 0));
        check_attr(&entry.attr, ino, mode, size, nlink, path);
        assert_eq!(entry.attr.blocks, size.div_ceil(512), "{path}: blocks");
    }
    // The root is inode 1 and has the three directories under it.
    let (attr, valid) = h.getattr(ROOT).attr();
    assert_eq!(valid, TREE_TTL.as_secs());
    check_attr(
        &attr,
        ROOT,
        S_IFDIR | u32::from(h.tree.image().node(NodeIndex(0)).expect("root").mode),
        0,
        5,
        "root",
    );
}

/// Asserts that a name that is not there is answered with inode 0 and the validity of a name
/// (the kernel keeps it as a negative entry), in a directory, in the root and below a file.
#[test]
fn lookup_of_a_missing_name_answers_inode_zero_valid_for_the_ttl() {
    let mut h = Harness::new("lookup-missing", &sample());
    let lib = h.resolve("lib").nodeid;
    let file = h.resolve("lib/a.txt").nodeid;
    for (parent, name) in [
        (ROOT, "nope"),
        (lib, "a.tx"),
        (lib, "a.txtt"),
        (file, "x"),
        (lib, ""),
    ] {
        let entry = h.lookup(parent, name).entry();
        assert_eq!(entry.nodeid, 0, "{name:?} in {parent}");
        assert_eq!(entry.entry_valid, TREE_TTL.as_secs());
        assert_eq!(entry.attr_valid, TREE_TTL.as_secs());
    }
}

/// Asserts that LOOKUP and GETATTR of inode 0 and of an inode past the last entry are `ENOENT`.
#[test]
fn lookup_and_getattr_of_inodes_that_are_not_entries_are_enoent() {
    let mut h = Harness::new("enoent", &sample());
    let past = h.tree.image().len() + 1;
    for nodeid in [0, past, u64::MAX] {
        assert_eq!(
            h.lookup(nodeid, "bin").errno(),
            ENOENT,
            "LOOKUP in {nodeid}"
        );
        assert_eq!(h.getattr(nodeid).errno(), ENOENT, "GETATTR of {nodeid}");
        assert_eq!(
            h.call(READLINK, nodeid, &[]).errno(),
            ENOENT,
            "READLINK of {nodeid}"
        );
    }
    // The last entry itself is there.
    assert_eq!(h.getattr(past - 1).errno(), 0);
}

/// Asserts that GETATTR answers what LOOKUP did, and that size and mtime do not change between
/// calls or after the file's bytes have been read (the kernel never drops a page cache of it).
#[test]
fn getattr_equals_lookup_and_size_and_mtime_are_stable() {
    let mut h = Harness::new("getattr", &sample());
    for &(path, mode, size, nlink) in SAMPLE_ATTRS {
        let entry = h.resolve(path);
        let first = h.getattr(entry.nodeid).attr();
        let second = h.getattr(entry.nodeid).attr();
        assert_eq!(first, second, "{path}: stable");
        assert_eq!(first.1, TREE_TTL.as_secs());
        check_attr(&first.0, entry.nodeid, mode, size, nlink, path);
        assert_eq!(first.0, entry.attr, "{path}: GETATTR equals LOOKUP");
    }
    let a_txt = h.resolve("lib/a.txt").nodeid;
    let before = h.getattr(a_txt).attr();
    let pack = h.publish_file("lib/a.txt");
    assert_eq!(pack, 1);
    let read = h.call(READ, a_txt, &read_body(0, 60));
    assert_eq!(read.ok(), h.content("lib/a.txt"));
    assert_eq!(h.getattr(a_txt).attr(), before);
}

// ---- READLINK ----

/// Asserts that READLINK answers a link's target and `EINVAL` for a file and for a directory.
#[test]
fn readlink_answers_the_target_of_a_link_and_refuses_other_entries() {
    let mut h = Harness::new("readlink", &sample());
    let link = h.resolve("lib/self").nodeid;
    assert_eq!(h.call(READLINK, link, &[]).ok(), b"a.txt");
    let file = h.resolve("lib/a.txt").nodeid;
    assert_eq!(h.call(READLINK, file, &[]).errno(), EINVAL);
    assert_eq!(h.call(READLINK, ROOT, &[]).errno(), EINVAL);
}

// ---- READDIR and READDIRPLUS ----

fn name_of(listed: &[Listed]) -> Vec<&str> {
    listed.iter().map(|e| e.name.as_str()).collect()
}

/// Asserts that READDIR and READDIRPLUS of the root and of a subdirectory list `.`, `..` and then
/// the children in name order, each with its inode, its type and the offset that resumes after it
/// (its position plus one); `.` is the directory, `..` its parent (the root's own parent is the
/// root); READDIRPLUS carries the child's entry and none for `.` and `..`.
#[test]
fn readdir_lists_dot_dotdot_and_children_in_name_order_with_inodes_types_and_offsets() {
    let mut h = Harness::new("readdir", &sample());
    let (bin, lib, tmp) = (h.resolve("bin"), h.resolve("lib"), h.resolve("tmp"));
    let root_want = [
        (".", ROOT, 4),
        ("..", ROOT, 4),
        ("bin", bin.nodeid, 4),
        ("lib", lib.nodeid, 4),
        ("tmp", tmp.nodeid, 4),
    ];
    let lib_want = [
        (".", lib.nodeid, 4),
        ("..", ROOT, 4),
        ("a.txt", h.resolve("lib/a.txt").nodeid, 8),
        ("b.txt", h.resolve("lib/b.txt").nodeid, 8),
        ("empty", h.resolve("lib/empty").nodeid, 8),
        ("self", h.resolve("lib/self").nodeid, 10),
    ];
    for (dir, want) in [(ROOT, &root_want[..]), (lib.nodeid, &lib_want[..])] {
        for plus in [false, true] {
            let got = listing(h.readdir(dir, 0, 4096, plus).ok(), plus);
            assert_eq!(got.len(), want.len(), "dir {dir} plus {plus}");
            for (position, (listed, &(name, ino, typ))) in got.iter().zip(want).enumerate() {
                assert_eq!(listed.name, name);
                assert_eq!(listed.ino, ino, "{name}: ino");
                assert_eq!(listed.typ, typ, "{name}: type");
                assert_eq!(listed.off, position as u64 + 1, "{name}: offset");
                match (plus, position) {
                    (false, _) => assert_eq!(listed.entry, None),
                    (true, 0 | 1) => assert_eq!(listed.entry, Some((0, 0)), "{name}: no entry"),
                    (true, _) => {
                        let looked = h.lookup(dir, name).entry();
                        assert_eq!(listed.entry, Some((looked.nodeid, looked.attr.mode)));
                    }
                }
            }
        }
    }
}

/// Asserts that a READDIR resumed at the offset of an entry in the middle returns the rest, that
/// an offset at or past the end returns an empty answer, and that a buffer too small for one
/// entry returns an empty answer too.
#[test]
fn readdir_resumes_at_an_offset_and_ends_at_the_end() {
    let mut h = Harness::new("readdir-resume", &sample());
    for plus in [false, true] {
        let all = listing(h.readdir(ROOT, 0, 4096, plus).ok(), plus);
        assert_eq!(name_of(&all), [".", "..", "bin", "lib", "tmp"]);
        let rest = listing(h.readdir(ROOT, 3, 4096, plus).ok(), plus);
        assert_eq!(name_of(&rest), ["lib", "tmp"]);
        assert_eq!(rest.iter().map(|e| e.off).collect::<Vec<_>>(), [4, 5]);
        assert_eq!(rest, all[3..], "the rest is what the full listing had");
        assert!(h.readdir(ROOT, 5, 4096, plus).ok().is_empty());
        assert!(h.readdir(ROOT, 99, 4096, plus).ok().is_empty());
        assert!(
            h.readdir(ROOT, 0, 10, plus).ok().is_empty(),
            "no room for one entry"
        );
        assert!(h.readdir(ROOT, 0, 0, plus).ok().is_empty());
    }
}

/// Asserts that a directory is listed whole across buffers of any size, one entry to a few, with
/// every entry once and in order, by READDIR and READDIRPLUS.
#[test]
fn readdir_across_small_buffers_loses_and_repeats_no_entry() {
    let mut h = Harness::new("readdir-small", &sample());
    let lib = h.resolve("lib").nodeid;
    for plus in [false, true] {
        let whole = listing(h.readdir(lib, 0, 4096, plus).ok(), plus);
        // One entry's size: READDIR 24 + name to 8 bytes (32 for "."), READDIRPLUS 152 + the same.
        let one = if plus { 160 } else { 32 };
        for size in [one, one + 8, 2 * one, 3 * one + 7] {
            let pieces = h.list_all(lib, size, plus);
            assert_eq!(pieces, whole, "plus {plus} size {size}");
        }
    }
}

/// Asserts that a directory of 302 entries, far above one 4 KiB buffer, is listed across several
/// calls, with offsets 1.. in sequence, every name once and in byte order, and subdirectories
/// before the files; and that its link count counts the two subdirectories.
#[test]
fn readdir_of_a_directory_larger_than_one_buffer_lists_every_entry_once() {
    let mut h = Harness::new("readdir-wide", &wide(300));
    let dir = h.resolve("wide");
    assert_eq!(
        dir.attr.nlink, 4,
        "2 plus two subdirectories, not the files"
    );
    let mut want = vec![
        ".".to_owned(),
        "..".to_owned(),
        "d0".to_owned(),
        "d1".to_owned(),
    ];
    want.extend((0..300).map(|n| format!("f{n:03}")));
    for plus in [false, true] {
        let first_page = listing(h.readdir(dir.nodeid, 0, 4096, plus).ok(), plus);
        assert!(
            first_page.len() < want.len(),
            "one buffer does not hold it all"
        );
        let all = h.list_all(dir.nodeid, 4096, plus);
        assert_eq!(
            all.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
            want,
            "plus {plus}"
        );
        assert!(all.iter().enumerate().all(|(i, e)| e.off == i as u64 + 1));
        assert_eq!(all[2].typ, 4);
        assert_eq!(all[4].typ, 8);
        if plus {
            let mid = &all[150];
            let looked = h.lookup(dir.nodeid, &mid.name).entry();
            assert_eq!(mid.entry, Some((looked.nodeid, looked.attr.mode)));
            assert_eq!(mid.ino, looked.nodeid);
        }
    }
}

/// Asserts that READDIR and READDIRPLUS of a file are `ENOTDIR` and of an inode that is not an
/// entry `ENOENT`.
#[test]
fn readdir_of_a_file_is_enotdir_and_of_a_missing_inode_enoent() {
    let mut h = Harness::new("readdir-notdir", &sample());
    let file = h.resolve("bin/cc").nodeid;
    let link = h.resolve("lib/self").nodeid;
    for plus in [false, true] {
        assert_eq!(h.readdir(file, 0, 4096, plus).errno(), ENOTDIR);
        assert_eq!(h.readdir(link, 0, 4096, plus).errno(), ENOTDIR);
        assert_eq!(h.readdir(0, 0, 4096, plus).errno(), ENOENT);
    }
}

/// Asserts that a listing whose children were damaged after the open (a child of a kind this
/// release does not know; a name that points out of the names) is `EIO`, not a panic and not a
/// short listing, while a directory of undamaged children still lists.
#[test]
fn readdir_of_a_damaged_directory_is_eio() {
    let mut h = Harness::new("readdir-damaged", &sample());
    let list = TreeListL2::open(h.l2.clone()).expect("the list opens");
    let entries = list.section_range(Section::Entries).start;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(h.rig.dir().join("tree.l2"))
        .expect("the image file");
    let record = |path: &str| entries + h.index_of(path).0 * ENTRY_LEN as u64;
    let bin_record = record("bin");
    // A record's kind is the high nibble of the byte at 13; its name's offset is bytes 4..8.
    file.write_all_at(&[0xf0], record("lib/b.txt") + 13)
        .expect("damage");
    file.write_all_at(&[0xff; 4], record("bin/x") + 4)
        .expect("damage");
    let (bin, lib) = (h.resolve("bin").nodeid, h.resolve("lib").nodeid);
    for plus in [false, true] {
        assert_eq!(
            h.readdir(lib, 0, 4096, plus).errno(),
            EIO,
            "unknown kind, plus {plus}"
        );
        assert_eq!(
            h.readdir(bin, 0, 4096, plus).errno(),
            EIO,
            "name out of range, plus {plus}"
        );
        assert_eq!(
            h.readdir(ROOT, 0, 4096, plus).errno(),
            0,
            "the root is intact"
        );
    }
    // A directory whose count was raised to the last number: a position near it names a child
    // past the last node number, which is EIO, not an overflow.
    file.write_all_at(&u64::MAX.to_le_bytes(), bin_record + 16)
        .expect("damage");
    assert_eq!(h.readdir(bin, u64::MAX - 1, 4096, false).errno(), EIO);
}

// ---- the other opcodes ----

/// Asserts that STATFS answers blocks of 512 and names of 255 and nothing else.
#[test]
fn statfs_answers_block_size_512_and_name_length_255_only() {
    let mut h = Harness::new("statfs", &sample());
    let msg = h.call(STATFS, ROOT, &[]);
    let (out, rest) = StatfsOut::read_prefix(msg.ok()).expect("statfs");
    assert!(rest.is_empty());
    let st = out.st;
    assert_eq!((st.bsize, st.namelen), (512, 255));
    assert_eq!(
        (
            st.blocks, st.bfree, st.bavail, st.files, st.ffree, st.frsize
        ),
        (0, 0, 0, 0, 0, 0)
    );
}

/// Asserts that every OPEN and OPENDIR is `ENOSYS` with nothing in the reply (so no `KEEP_CACHE`
/// flag can be in it), that each is counted, and that a file and a directory alike are refused.
#[test]
fn every_open_and_opendir_is_enosys_and_counted() {
    let mut h = Harness::new("opens", &sample());
    let file = h.resolve("bin/cc").nodeid;
    for _ in 0..3 {
        let msg = h.call(OPEN, file, &[0; 8]);
        assert_eq!(msg.errno(), ENOSYS);
        assert!(
            msg.payload.is_empty(),
            "no open reply, so no FOPEN_KEEP_CACHE"
        );
    }
    for _ in 0..2 {
        let msg = h.call(OPENDIR, ROOT, &[0; 8]);
        assert_eq!(msg.errno(), ENOSYS);
        assert!(msg.payload.is_empty());
    }
    assert_eq!(
        h.fs.stats(),
        TreeFsStats {
            opens_refused: 5,
            parked_answered: 0
        }
    );
}

/// Asserts that RELEASE and RELEASEDIR are answered with an empty success, and FLUSH, FSYNC and
/// an opcode the mount does not know are `ENOSYS`.
#[test]
fn release_is_answered_and_flush_fsync_and_unknown_opcodes_are_enosys() {
    let mut h = Harness::new("release", &sample());
    let file = h.resolve("bin/cc").nodeid;
    for (opcode, nodeid) in [(RELEASE, file), (RELEASEDIR, ROOT)] {
        let msg = h.call(opcode, nodeid, &[0; 24]);
        assert!(msg.ok().is_empty(), "opcode {opcode}");
    }
    for opcode in [FLUSH, 20, 1000] {
        assert_eq!(
            h.call(opcode, file, &[0; 24]).errno(),
            ENOSYS,
            "opcode {opcode}"
        );
    }
}

/// Asserts that SETATTR, MKDIR, UNLINK, RENAME, WRITE and CREATE are `EROFS`.
#[test]
fn every_write_is_erofs() {
    let mut h = Harness::new("erofs", &sample());
    let file = h.resolve("bin/cc").nodeid;
    for (name, opcode) in [
        ("SETATTR", 4),
        ("MKDIR", 9),
        ("UNLINK", 10),
        ("RENAME", 12),
        ("WRITE", 16),
        ("CREATE", 35),
    ] {
        assert_eq!(h.call(opcode, file, &[0; 128]).errno(), EROFS, "{name}");
        assert_eq!(
            h.call(opcode, ROOT, &[0; 128]).errno(),
            EROFS,
            "{name} in the root"
        );
    }
}

/// Asserts that a request whose body is too short for its opcode (a READ, a LOOKUP whose name has
/// no end) is answered `EIO`, the parse error's errno, and not parked.
#[test]
fn a_malformed_body_is_eio() {
    let mut h = Harness::new("malformed", &sample());
    let cc = h.resolve("bin/cc").nodeid;
    assert_eq!(h.call(READ, cc, &[0; 8]).errno(), EIO);
    assert_eq!(h.call(READDIR, ROOT, &[0; 8]).errno(), EIO);
    assert_eq!(h.call(LOOKUP, ROOT, b"bin").errno(), EIO);
    assert_eq!(h.call(GETATTR, ROOT, &[]).errno(), EIO);
}
