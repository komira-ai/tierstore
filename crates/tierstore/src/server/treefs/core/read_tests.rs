//! The reads by inode of a frontend with no open, and the core that does not count lookups.

use super::super::Looked;
use super::{Core, Errno};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Rig, sample};

const RUN: &str = "run-1";

/// A rig over the sample tree with `RUN` attached, whose core counts lookups as FUSE's does.
fn attached(name: &str) -> Rig {
    attach(Rig::new(name, sample()))
}

/// A rig over the sample tree with `RUN` attached, whose core does not count lookups.
fn attached_uncounted(name: &str) -> Rig {
    attach(Rig::uncounted(name, sample()))
}

fn attach(rig: Rig) -> Rig {
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    rig
}

fn blob_with_length(rig: &Rig, length: usize) -> Vec<u8> {
    let built = rig.upstream.built.lock().unwrap();
    built
        .blobs
        .values()
        .find(|bytes| bytes.len() == length)
        .cloned()
        .unwrap()
}

/// Asserts that a file is read by its inode with no open: whole, in parts at any offset, and a
/// request for more than the file has gives the rest; two reads open no handle.
#[test]
fn read_ino_returns_the_bytes_of_a_cached_file_at_any_offset() {
    let rig = attached("tfs-read-bytes");
    let core = &rig.core;
    let big = blob_with_length(&rig, 300_000);
    let ino = ino::of(1, 1);
    assert_eq!(core.read_ino(ino, 0, 300_000).unwrap(), big);
    assert_eq!(core.read_ino(ino, 1000, 5).unwrap(), big[1000..1005]);
    assert_eq!(core.read_ino(ino, 299_998, 100).unwrap(), big[299_998..]);
    assert_eq!(core.read_ino(ino, 0, u32::MAX).unwrap(), big);
    assert_eq!(core.read_ino(ino, 7, 0).unwrap(), Vec::<u8>::new());
    assert_eq!(core.open_files(), 0);
}

/// Asserts that a file of no bytes reads as empty and asks the fill socket for nothing.
#[test]
fn read_ino_of_an_empty_file_is_empty() {
    let rig = attached("tfs-read-empty");
    let fills = rig.upstream.asked("fill").len();
    assert!(rig.core.read_ino(ino::of(1, 5), 0, 100).unwrap().is_empty());
    assert_eq!(rig.upstream.asked("fill").len(), fills);
}

/// Asserts that a directory of a tree, the root of a tree, and a directory of the namespace are
/// `EISDIR`.
#[test]
fn read_ino_of_a_directory_is_eisdir() {
    let rig = attached("tfs-read-dir");
    for dir in [ino::of(1, 2), ino::of(1, 0), ino::ROOT, ino::TREES] {
        assert_eq!(rig.core.read_ino(dir, 0, 10), Err(Errno::IsDir), "{dir}");
    }
}

/// Asserts that a symbolic link is `EINVAL` and an inode the tree does not have is `ENOENT`.
#[test]
fn read_ino_of_a_link_is_inval() {
    let rig = attached("tfs-read-link");
    assert_eq!(rig.core.read_ino(ino::of(1, 8), 0, 10), Err(Errno::Inval));
    assert_eq!(rig.core.read_ino(ino::of(1, 99), 0, 10), Err(Errno::NoEnt));
    assert_eq!(rig.core.read_ino(ino::of(9, 1), 0, 10), Err(Errno::NoEnt));
}

/// Asserts that a blob the cache lacks is filled once through the fill socket, from the source the
/// manifest names or from `cas/l/<sha256>` where it names none, and then read from the cache; and
/// that the blob is pinned for the tree.
#[test]
fn read_ino_fills_a_missing_blob_through_the_fill_socket() {
    let rig = attached("tfs-read-fill");
    let core = &rig.core;
    let tool = blob_with_length(&rig, 10);
    let manifests = rig.upstream.asked("fill").len();
    assert_eq!(core.read_ino(ino::of(1, 4), 0, 100).unwrap(), tool);
    let fills = rig.upstream.asked("fill");
    assert_eq!(fills.len(), manifests + 1, "{fills:?}");
    assert!(
        fills.last().unwrap().contains(" 10 from cas/l/"),
        "{fills:?}"
    );
    assert_eq!(core.read_ino(ino::of(1, 4), 2, 3).unwrap(), tool[2..5]);
    assert_eq!(rig.upstream.asked("fill").len(), manifests + 1);
    assert_eq!(rig.upstream.asked("pin").len(), 1);
    let big = blob_with_length(&rig, 300_000);
    assert_eq!(core.read_ino(ino::of(1, 1), 4, 4).unwrap(), big[4..8]);
    assert!(
        rig.upstream
            .asked("fill")
            .last()
            .unwrap()
            .contains("from packs/one @ 4096")
    );
}

/// Asserts that a fill that fails is `EIO`, is recorded on the attachments of the tree for the
/// path, and that the file reads once the upstream has the blob.
#[test]
fn read_ino_of_a_fill_failure_is_eio() {
    let rig = attached("tfs-read-fail");
    let core = &rig.core;
    let cc = {
        let built = rig.upstream.built.lock().unwrap();
        built
            .blobs
            .iter()
            .find(|(_, bytes)| bytes.len() == 3000)
            .map(|(sha, _)| sha.clone())
            .unwrap()
    };
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(cc.clone(), "no space".to_owned());
    assert_eq!(core.read_ino(ino::of(1, 3), 0, 10), Err(Errno::Io));
    let failures = core.detach(RUN).unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].path, "/bin/cc");
    assert_eq!(failures[0].digest, cc);
    assert_eq!(failures[0].reason, "no space");
}

/// Asserts that a read at the end of a file, and past it, is empty and fills nothing for a blob
/// the cache lacks.
#[test]
fn read_ino_past_the_end_is_empty() {
    let rig = attached("tfs-read-past");
    let core = &rig.core;
    let fills = rig.upstream.asked("fill").len();
    assert!(
        core.read_ino(ino::of(1, 1), 300_000, 10)
            .unwrap()
            .is_empty()
    );
    assert!(
        core.read_ino(ino::of(1, 1), 400_000, 10)
            .unwrap()
            .is_empty()
    );
    assert!(
        core.read_ino(ino::of(1, 1), u64::MAX, 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(rig.upstream.asked("fill").len(), fills);
}

/// Asserts that a cache file that the system cannot read is `EIO`: here a directory in the place
/// of the blob's file.
#[test]
fn read_ino_of_a_cache_file_that_cannot_be_read_is_eio() {
    let rig = attached("tfs-read-io");
    let tool = {
        let built = rig.upstream.built.lock().unwrap();
        built
            .blobs
            .iter()
            .find(|(_, bytes)| bytes.len() == 10)
            .map(|(sha, _)| sha.clone())
            .unwrap()
    };
    let path = crate::server::local_cache::blob_path(&rig.cache, &tool);
    std::fs::create_dir_all(&path).unwrap();
    assert_eq!(rig.core.read_ino(ino::of(1, 4), 0, 10), Err(Errno::Io));
}

fn bin_of(core: &Core, root: &str) -> u64 {
    let Looked::Found(_) = core.lookup(ino::TREES, root).unwrap() else {
        panic!("the tree is not found");
    };
    match core.lookup(ino::of(1, 0), "bin").unwrap() {
        Looked::Found(entry) => entry.attr.ino,
        other => panic!("{other:?}"),
    }
}

/// Asserts that a core that does not count lookups drops a tree at its detach however many lookups
/// were made, and reads it again, with the same inode numbers, when a request names it.
#[test]
fn no_forget_core_drops_a_tree_at_its_detach_with_lookups_outstanding() {
    let rig = attached_uncounted("tfs-read-nofgt-drop");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let bin = bin_of(core, &root);
    core.count(bin);
    assert_eq!(core.trees_in_memory(), 1);
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.trees_in_memory(), 0);
    assert_eq!(core.getattr(bin).unwrap().attr.ino, bin);
    assert_eq!(core.trees_in_memory(), 1);
}

/// Asserts that lookups, a counted listing entry and a forget of a core that does not count change
/// nothing about what stays in memory: an attached tree stays, and a forget does not drop it.
#[test]
fn no_forget_core_does_not_count_lookups() {
    let rig = attached_uncounted("tfs-read-nofgt-count");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let bin = bin_of(core, &root);
    core.count(bin);
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 1);
    assert!(core.detach(RUN).is_some());
    // A forget after the detach finds nothing counted and nothing in memory.
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 0);
}

/// Asserts that a core that counts lookups, as FUSE's does, keeps a detached tree in memory until
/// the last lookup is forgotten.
#[test]
fn forget_core_keeps_a_tree_until_forgotten_after_detach_unchanged() {
    let rig = attached("tfs-read-forget");
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let bin = bin_of(core, &root);
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.trees_in_memory(), 1);
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 1);
    core.forget(ino::of(1, 0), 1);
    assert_eq!(core.trees_in_memory(), 0);
}

/// Asserts the cache file of a file by inode: its bytes and its length, none for a file of no
/// bytes, and the refusals for a directory, the namespace, a link and an inode no tree has.
#[test]
fn blob_file_gives_the_cache_file_and_the_length_of_a_file() {
    use std::os::unix::fs::FileExt as _;
    let rig = attached_uncounted("tfs-blob-file");
    let core = &rig.core;
    let (file, length) = core.blob_file(ino::of(1, 4)).unwrap().unwrap();
    assert_eq!(length, 10);
    let mut bytes = [0u8; 10];
    file.read_exact_at(&mut bytes, 0).unwrap();
    assert_eq!(bytes.to_vec(), blob_with_length(&rig, 10));
    assert!(core.blob_file(ino::of(1, 5)).unwrap().is_none());
    for dir in [ino::of(1, 2), ino::ROOT, ino::TREES] {
        assert_eq!(core.blob_file(dir).unwrap_err(), Errno::IsDir, "{dir}");
    }
    assert_eq!(core.blob_file(ino::of(1, 8)).unwrap_err(), Errno::Inval);
    assert_eq!(core.blob_file(ino::of(1, 99)).unwrap_err(), Errno::NoEnt);
    assert_eq!(core.blob_file(ino::of(9, 1)).unwrap_err(), Errno::NoEnt);
}

/// Asserts the parent of an inode: the directory above an entry, `trees/` for the root of a tree,
/// the mount's root for `trees/` and for itself, and the refusals for an inode that is none.
#[test]
fn parent_is_the_directory_above() {
    let rig = attached_uncounted("tfs-parent");
    let core = &rig.core;
    assert_eq!(core.parent(ino::of(1, 4)).unwrap(), ino::of(1, 2));
    assert_eq!(core.parent(ino::of(1, 2)).unwrap(), ino::of(1, 0));
    assert_eq!(core.parent(ino::of(1, 0)).unwrap(), ino::TREES);
    assert_eq!(core.parent(ino::TREES).unwrap(), ino::ROOT);
    assert_eq!(core.parent(ino::ROOT).unwrap(), ino::ROOT);
    assert_eq!(core.parent(3).unwrap_err(), Errno::NoEnt);
    assert_eq!(core.parent(ino::of(1, 99)).unwrap_err(), Errno::NoEnt);
    assert_eq!(core.parent(ino::of(9, 0)).unwrap_err(), Errno::NoEnt);
}

/// Asserts a listing from the last inode seen: all entries after 0, in order and without `.` and
/// `..`; those above an inode that is or is not an entry; the end when the sink says stop; and
/// the refusals for the namespace, a file and a tree that is not there.
#[test]
fn readdir_after_lists_the_entries_above_an_inode() {
    let rig = attached_uncounted("tfs-readdir-after");
    let core = &rig.core;
    let list = |dir: u64, after: u64, most: usize| {
        let mut names = Vec::new();
        core.readdir_after(dir, after, &mut |item| {
            names.push(item.name);
            names.len() < most
        })
        .map(|()| names)
    };
    let root = ino::of(1, 0);
    assert_eq!(
        list(root, 0, 99).unwrap(),
        ["big.bin", "bin", "empty.h", "include", "lib"]
    );
    assert_eq!(
        list(root, ino::of(1, 2), 99).unwrap(),
        ["empty.h", "include", "lib"]
    );
    assert_eq!(
        list(root, ino::of(1, 3), 99).unwrap(),
        ["empty.h", "include", "lib"]
    );
    assert!(list(root, ino::of(1, 8), 99).unwrap().is_empty());
    assert_eq!(list(root, 0, 2).unwrap(), ["big.bin", "bin"]);
    assert_eq!(list(ino::of(1, 2), 0, 99).unwrap(), ["cc", "tool"]);
    assert_eq!(list(ino::ROOT, 0, 9), Err(Errno::Acces));
    assert_eq!(list(ino::of(1, 1), 0, 9), Err(Errno::NotDir));
    assert_eq!(list(ino::of(9, 0), 0, 9), Err(Errno::NoEnt));
}
