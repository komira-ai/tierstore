//! The core in a process that took a connection over: file handles of the process before it, and
//! the backing ids an earlier release registered on the connection.

use std::sync::Arc;

use super::super::{Core, Errno, Opened};
use crate::server::local_cache::trees::testing::fs_at;
use crate::server::treefs::backing::{self, Saved};
use crate::server::treefs::handle;
use crate::server::treefs::ino;
use crate::server::treefs::live_testing::{Live, walk};
use crate::server::treefs::locked;
use crate::server::treefs::testing::open_cached;
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

/// A rig over the sample tree with the tree attached.
fn rig(name: &str) -> Rig {
    let rig = Rig::new(name, sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    rig
}

/// The line an earlier release kept for the blob of `tool`, with the id 100, counted exactly.
fn tool_line() -> Saved {
    Saved {
        blob: tool_blob(),
        id: 100,
        opens: 2,
        exact: true,
    }
}

/// The core of the process after `rig`'s, over a connection an earlier release answered with
/// passthrough and whose file holds `lines`; the ids are made by `kernel`.
fn successor(rig: &Rig, kernel: &FakeKernel, lines: &[Saved]) -> Arc<Core> {
    let path = rig.runtime.join(backing::FILE);
    let core = rig.successor();
    core.adopt_backing(&path, lines, &|id| kernel.wrap(id));
    core
}

/// The handle and the id of an open that must be answered with an adopted id.
fn adopted(opened: Opened) -> (u64, u32) {
    match opened {
        Opened::Adopted { fh, id } => (fh, FakeKernel::id_of(&id).unwrap()),
        Opened::Cached(fh) => panic!("a cached open, handle {fh}, where an adopted id was due"),
    }
}

/// Asserts that a read of a handle the process before gave out makes the cached open again and
/// answers the bytes, that a second read uses the open it made, and that the release ends it.
#[test]
fn a_read_of_a_handle_an_earlier_process_gave_makes_the_open_again() {
    let rig = rig("tfs-adopt-read");
    let fh = open_cached(&rig.core, header(), false).unwrap();
    let next = rig.successor();
    assert_eq!(next.read(fh, 0, 6), Err(Errno::Inval));
    assert_eq!(next.read_open(header(), fh, 0, 6).unwrap(), b"int a;");
    assert_eq!(next.open_files(), 1);
    assert_eq!(next.read_open(header(), fh, 4, 100).unwrap(), b"a;");
    next.release(fh);
    assert_eq!(next.open_files(), 0);
    // The first process's own handle is still its own.
    assert_eq!(rig.core.read_open(header(), fh, 0, 3).unwrap(), b"int");
}

/// Asserts that the handle of an empty file is made again as an empty open, that a name that is not
/// a file is `EINVAL` and one that is not there `EIO` (a read is never answered `ENOENT`), and that
/// a handle of this process that is not open is `EINVAL` as before.
#[test]
fn a_foreign_handle_of_something_that_is_not_a_readable_file_is_refused() {
    let rig = rig("tfs-adopt-refused");
    let foreign = handle::make(1, 77);
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
        Err(Errno::Io)
    );
    let own = handle::make(2, 9);
    assert_eq!(next.read_open(header(), own, 0, 10), Err(Errno::Inval));
}

/// Asserts that the handle of a passthrough open an earlier release gave out (bit 63 set) is told
/// by its generation as any other: a read the kernel sends for it makes a cached open of the file
/// and answers its bytes, and its release ends that open.
#[test]
fn a_handle_an_earlier_release_marked_passthrough_is_an_earlier_process_handle() {
    let rig = rig("tfs-adopt-marked");
    let marked = (1 << 63) | handle::make(1, 5);
    let next = rig.successor();
    assert_eq!(next.read_open(tool(), marked, 0, 4).unwrap(), b"tool");
    assert_eq!(next.open_files(), 1);
    next.release(marked);
    assert_eq!(next.open_files(), 0);
}

/// Asserts that the opens of a blob an earlier release registered an id for are answered with that
/// id, every time, with no fill and no registration, and that a read of one is a fault (the kernel
/// reads those itself); that the opens of every other file are cached opens; and that no release,
/// nor the end of the core, closes the id: the connection keeps it.
#[test]
fn the_opens_of_a_blob_with_an_adopted_id_are_answered_with_it_and_it_is_never_closed() {
    let rig = rig("tfs-adopt-answer");
    let kernel = FakeKernel::new();
    let next = successor(&rig, &kernel, &[tool_line()]);
    assert_eq!(next.adopted_ids(), 1);
    let fills = rig.upstream.asked("fill").len();
    let (one, id) = adopted(next.open(tool(), false).unwrap());
    let (two, again) = adopted(next.open(tool(), false).unwrap());
    assert_eq!((id, again), (100, 100));
    assert_ne!(one, two);
    assert_eq!(rig.upstream.asked("fill").len(), fills);
    assert_eq!(next.read(one, 0, 4), Err(Errno::Io));
    let header = match next.open(header(), false).unwrap() {
        Opened::Cached(fh) => fh,
        Opened::Adopted { .. } => panic!("an adopted id for a blob that has none"),
    };
    assert_eq!(next.read(header, 0, 6).unwrap(), b"int a;");
    assert!(matches!(next.open(empty(), false), Ok(Opened::Cached(_))));
    for fh in [one, two, header] {
        next.release(fh);
    }
    assert!(locked(&kernel.closed).is_empty());
    drop(next);
    assert!(locked(&kernel.closed).is_empty());
}

/// Asserts that the file an earlier release kept is written again with no line marked `exact`, so
/// that an earlier release that takes the connection over from this one keeps every id; that a
/// process after this one reads the same ids from it; and that a file with no line marked is left
/// as it is.
#[test]
fn the_backing_table_is_unmarked_once_and_the_next_process_adopts_the_same_ids() {
    let rig = rig("tfs-adopt-unmark");
    let path = rig.runtime.join(backing::FILE);
    let header_blob = TreeDigest::from_hex(&sha(b"int a;")).unwrap();
    let lines = [
        tool_line(),
        Saved {
            blob: header_blob,
            id: 101,
            opens: 1,
            exact: false,
        },
    ];
    let kernel = FakeKernel::new();
    let _ = successor(&rig, &kernel, &lines);
    let mut expected = [
        format!("{} 100 2\n", tool_blob()),
        format!("{header_blob} 101 1\n"),
    ];
    expected.sort();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected.concat());
    let read = backing::saved(&path).unwrap();
    assert!(read.iter().all(|line| !line.exact));
    let after = successor(&rig, &kernel, &read);
    assert_eq!(after.adopted_ids(), 2);
    assert_eq!(adopted(after.open(header(), false).unwrap()).1, 101);
    // A file with no line marked exact is not written.
    let kept = format!("{} 100 2 a-word-a-later-release-adds\n", tool_blob());
    std::fs::write(&path, &kept).unwrap();
    let unmarked = backing::saved(&path).unwrap();
    let _ = successor(&rig, &kernel, &unmarked);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), kept);
    assert!(locked(&kernel.closed).is_empty());
}

/// Asserts that the ids are taken once: a second adoption (a hand-over that did not complete, then
/// served again) keeps the first.
#[test]
fn the_ids_are_adopted_once() {
    let rig = rig("tfs-adopt-once");
    let kernel = FakeKernel::new();
    let next = successor(&rig, &kernel, &[tool_line()]);
    next.adopt_backing(&rig.runtime.join(backing::FILE), &[], &|id| kernel.wrap(id));
    assert_eq!(next.adopted_ids(), 1);
    assert_eq!(adopted(next.open(tool(), false).unwrap()).1, 100);
}

/// Asserts that a core that adopted nothing has no adopted id and opens every file cached.
#[test]
fn a_core_with_no_adopted_ids_opens_every_file_cached() {
    let rig = rig("tfs-adopt-none");
    assert_eq!(rig.core.adopted_ids(), 0);
    let kernel = FakeKernel::new();
    let next = successor(&rig, &kernel, &[]);
    assert_eq!(next.adopted_ids(), 0);
    assert!(matches!(next.open(tool(), false), Ok(Opened::Cached(_))));
}

/// Asserts that a read of a handle of a placed file that the process before gave out makes the
/// open again as the file's span of the pack: the bytes are the file's, not the first bytes of the
/// pack, a read from the middle and one past the end are clipped to the file, and the release ends
/// the open.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_handle_of_a_placed_file_is_made_again_as_its_span() {
    let live = Live::new("tfs-adopt-placed").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let (ino, fh) = live
        .on_core(|core| {
            let ino = walk(core, &["jobs", "1f3a", "root", "bin", "other"]);
            let fh = open_cached(core, ino, false).unwrap();
            (ino, fh)
        })
        .await;
    let other = live.bench.pack.content(1).to_vec();
    assert_ne!(other[..100], live.bench.pack.content(0)[..100]);
    let next = live.successor();
    let (whole, middle, tail, past, open, closed) = tokio::task::spawn_blocking(move || {
        assert_eq!(next.read(fh, 0, 10), Err(Errno::Inval));
        let whole = next.read_open(ino, fh, 0, u32::MAX).unwrap();
        let middle = next.read_open(ino, fh, 100, 50).unwrap();
        let tail = next.read_open(ino, fh, 1990, 100).unwrap();
        let past = next.read_open(ino, fh, 2000, 100).unwrap();
        let open = next.open_files();
        next.release(fh);
        (whole, middle, tail, past, open, next.open_files())
    })
    .await
    .unwrap();
    assert_eq!(whole, other);
    assert_eq!(middle, other[100..150]);
    assert_eq!(tail, other[1990..]);
    assert!(past.is_empty());
    assert_eq!((open, closed), (1, 0));
}

/// Asserts that the handles of the other kinds of member of a placed tree that an earlier process
/// gave out are treated as those of a tree of manifests: the handle of the empty file is made
/// again as an empty open, and that of a symbolic link is `EINVAL`.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_handle_of_a_placed_empty_file_or_link_is_as_for_a_manifest_tree() {
    let live = Live::new("tfs-adopt-placed-kinds").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let (zero, link) = live
        .on_core(|core| {
            (
                walk(core, &["jobs", "1f3a", "root", "zero"]),
                walk(core, &["jobs", "1f3a", "root", "link"]),
            )
        })
        .await;
    let next = live.successor();
    let foreign = handle::make(1, 77);
    let (empty, linked) = tokio::task::spawn_blocking(move || {
        (
            next.read_open(zero, foreign, 0, 10),
            next.read_open(link, foreign + 1, 0, 10),
        )
    })
    .await
    .unwrap();
    assert_eq!(empty, Ok(Vec::new()));
    assert_eq!(linked, Err(Errno::Inval));
}
