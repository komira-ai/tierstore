//! The core in a process that took a connection over: file handles and backing ids of the process
//! before it.

use std::sync::Arc;

use super::super::{Core, Errno, Opened};
use crate::server::treefs::backing::{self, Saved};
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::testing::{FakeKernel, Rig, sample, sha};
use crate::tree_manifest::TreeDigest;

/// The inode numbers of the sample tree, which is tree number 1.
fn tool() -> u64 {
    ino::of(1, 4)
}
fn empty() -> u64 {
    ino::of(1, 5)
}
fn include() -> u64 {
    ino::of(1, 6)
}
fn header() -> u64 {
    ino::of(1, 7)
}

fn tool_blob() -> TreeDigest {
    TreeDigest::from_hex(&sha(b"tool bytes")).unwrap()
}

/// A rig over the sample tree with passthrough chosen and the tree attached.
fn rig(name: &str) -> Rig {
    let rig = Rig::new(name, sample());
    rig.core.enable_passthrough();
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    rig
}

/// The core of the process after `rig`'s, with its backing table adopted from the file the process
/// before wrote, and the kernel that closes ids.
fn successor(rig: &Rig, kernel: &FakeKernel) -> Arc<Core> {
    let core = rig.successor();
    let saved = backing::saved(&rig.runtime.join(backing::FILE)).unwrap();
    core.adopt_passthrough(&saved, &|id| kernel.wrap(id));
    core
}

fn backed(opened: Opened) -> (u64, u32) {
    match opened {
        Opened::Backed { fh, backing } => (fh, backing.id),
        Opened::Cached(fh) => panic!("a cached open, handle {fh}, where passthrough was due"),
    }
}

/// Asserts that a read of a handle the process before gave out makes the cached open again and
/// answers the bytes, that a second read uses the open it made, and that the release ends it.
#[test]
fn a_read_of_a_handle_an_earlier_process_gave_makes_the_open_again() {
    let rig = Rig::new("tfs-adopt-read", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let fh = match rig
        .core
        .open(header(), false, &mut |_| unreachable!())
        .unwrap()
    {
        Opened::Cached(fh) => fh,
        Opened::Backed { .. } => panic!("passthrough was not chosen"),
    };
    let next = rig.successor();
    assert_eq!(next.read(fh, 0, 6), Err(Errno::Inval));
    assert_eq!(next.read_open(header(), fh, 0, 6).unwrap(), b"int a;");
    assert_eq!(next.open_files(), 1);
    assert_eq!(next.read_open(header(), fh, 4, 100).unwrap(), b"a;");
    next.release_open(header(), fh);
    assert_eq!(next.open_files(), 0);
    // The first process's own handle is still its own.
    assert_eq!(rig.core.read_open(header(), fh, 0, 3).unwrap(), b"int");
}

/// Asserts that the handle of an empty file is made again as an empty open, that a name that is not
/// a file is `EINVAL` and one that is not there `ENOENT`, and that a handle of this process that
/// is not open is `EINVAL` as before.
#[test]
fn a_foreign_handle_of_something_that_is_not_a_readable_file_is_refused() {
    let rig = Rig::new("tfs-adopt-refused", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let foreign = crate::server::treefs::handle::make(1, 77, false);
    let next = rig.successor();
    assert_eq!(
        next.read_open(empty(), foreign, 0, 10).unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(
        next.read_open(include(), foreign + 1, 0, 10),
        Err(Errno::Inval)
    );
    assert_eq!(
        next.read_open(ino::of(1, 99), foreign + 2, 0, 10),
        Err(Errno::NoEnt)
    );
    let own = crate::server::treefs::handle::make(2, 9, false);
    assert_eq!(next.read_open(header(), own, 0, 10), Err(Errno::Inval));
}

/// Asserts that a foreign passthrough handle is never read through the core: the kernel reads
/// those itself, so a request for one is a fault, as for a handle of this process.
#[test]
fn a_read_of_a_foreign_passthrough_handle_is_a_fault() {
    let rig = rig("tfs-adopt-backed-read");
    let kernel = FakeKernel::new();
    let (fh, _) = backed(kernel.open(&rig.core, tool()).unwrap());
    let next = successor(&rig, &kernel);
    assert_eq!(next.read_open(tool(), fh, 0, 10), Err(Errno::Inval));
    assert_eq!(next.open_files(), 0);
}

/// Asserts that the ids the process before registered, and wrote down exactly at the end, are
/// closed by the releases of the opens it made, the last one closing the id and no earlier one.
#[test]
fn the_last_release_of_an_open_an_earlier_process_made_closes_its_id() {
    let rig = rig("tfs-adopt-release");
    let old_kernel = FakeKernel::new();
    let (one, id) = backed(old_kernel.open(&rig.core, tool()).unwrap());
    let (two, _) = backed(old_kernel.open(&rig.core, tool()).unwrap());
    assert_eq!(id, 100);
    rig.core.write_backing_exact();
    let kernel = FakeKernel::new();
    let next = successor(&rig, &kernel);
    let table = next.backing_table().unwrap();
    assert_eq!(table.live(), 1);
    next.release_open(tool(), one);
    assert!(locked(&kernel.closed).is_empty());
    next.release_open(tool(), two);
    assert_eq!(*locked(&kernel.closed), [100]);
    assert_eq!(table.live(), 0);
    // Another release of the same handle finds no id to end.
    next.release_open(tool(), two);
    assert_eq!(*locked(&kernel.closed), [100]);
}

/// Asserts that a new open of a blob whose id was adopted reuses the id with no registration, that
/// the releases of the earlier opens do not close it while this process has one open, and that the
/// last release of all does.
#[test]
fn an_open_of_an_adopted_blob_reuses_its_id_and_keeps_it_for_the_earlier_opens() {
    let rig = rig("tfs-adopt-reuse");
    let old_kernel = FakeKernel::new();
    let (old, _) = backed(old_kernel.open(&rig.core, tool()).unwrap());
    rig.core.write_backing_exact();
    let kernel = FakeKernel::new();
    let next = successor(&rig, &kernel);
    let (new, id) = backed(kernel.open(&next, tool()).unwrap());
    assert_eq!(id, 100);
    assert!(locked(&kernel.registered).is_empty());
    assert_ne!(new, old);
    next.release_open(tool(), old);
    assert!(locked(&kernel.closed).is_empty());
    next.release_open(tool(), new);
    assert_eq!(*locked(&kernel.closed), [100]);
}

/// Asserts that a table that was not written at a clean stop (a crash) is a lower bound: the id is
/// kept through every release of an earlier open, and closed by the release of this process's own
/// last open only if no earlier one is unaccounted for, which here it is not, so it stays.
#[test]
fn an_id_whose_count_was_not_written_exactly_is_kept() {
    let rig = rig("tfs-adopt-inexact");
    let old_kernel = FakeKernel::new();
    let (one, _) = backed(old_kernel.open(&rig.core, tool()).unwrap());
    let (two, _) = backed(old_kernel.open(&rig.core, tool()).unwrap());
    // The file says one open, as it does after the first open and until the last release.
    let kernel = FakeKernel::new();
    let next = successor(&rig, &kernel);
    next.release_open(tool(), one);
    next.release_open(tool(), two);
    assert!(locked(&kernel.closed).is_empty());
    assert_eq!(next.backing_table().unwrap().live(), 1);
    let (own, id) = backed(kernel.open(&next, tool()).unwrap());
    assert_eq!(id, 100);
    next.release_open(tool(), own);
    assert!(locked(&kernel.closed).is_empty());
}

/// Asserts that the release of a foreign passthrough handle of an inode that is not a file, or of
/// a core with no backing table, ends nothing and does not fail.
#[test]
fn a_foreign_release_with_nothing_to_end_ends_nothing() {
    let rig = Rig::new("tfs-adopt-nothing", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let next = rig.successor();
    let foreign = crate::server::treefs::handle::make(1, 5, true);
    next.release_open(tool(), foreign);
    next.write_backing_exact();
    let kernel = FakeKernel::new();
    next.adopt_passthrough(&[], &|id| kernel.wrap(id));
    next.release_open(include(), foreign);
    next.release_open(ino::of(1, 99), foreign);
    next.release_open(tool(), foreign);
    next.release_open(empty(), foreign);
    // A handle of this process that is not open, and a cached one of an earlier process, end
    // nothing either.
    next.release_open(tool(), crate::server::treefs::handle::make(2, 77, true));
    next.release_open(tool(), crate::server::treefs::handle::make(1, 78, false));
    assert!(locked(&kernel.closed).is_empty());
}

/// Asserts that a second choice of passthrough leaves the first table (the choice is made once).
#[test]
fn passthrough_is_adopted_once() {
    let rig = rig("tfs-adopt-once");
    let kernel = FakeKernel::new();
    let first: Saved = Saved {
        blob: tool_blob(),
        id: 7,
        opens: 0,
        exact: true,
    };
    let next = rig.successor();
    next.adopt_passthrough(&[first], &|id| kernel.wrap(id));
    next.adopt_passthrough(&[], &|id| kernel.wrap(id));
    assert_eq!(next.backing_table().unwrap().live(), 1);
}
