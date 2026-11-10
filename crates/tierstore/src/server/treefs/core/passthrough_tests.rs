//! The core's opens where passthrough was chosen: one backing id per blob, shared by every open,
//! file and tree of it; the table; the failures; and the fill that an open waits for.

use std::sync::Arc;

use super::{Errno, Opened};
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::testing::{Built, FakeKernel, Rig, file, sample, until};
use crate::tree_manifest::TreeDigest;

/// A rig over the sample tree with passthrough chosen and the tree attached for `run-1`.
fn passthrough(name: &str) -> Rig {
    let rig = Rig::new(name, sample());
    rig.core.enable_passthrough();
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    rig
}

/// The backing of an open that must be a passthrough one.
fn backed(opened: Opened) -> (u64, u32) {
    match opened {
        Opened::Backed { fh, backing } => (fh, backing.id),
        Opened::Cached(fh) => panic!("a cached open, handle {fh}, where passthrough was due"),
    }
}

fn digest_of(bytes: &[u8]) -> TreeDigest {
    TreeDigest::from_hex(&crate::server::treefs::testing::sha(bytes)).unwrap()
}

/// Asserts the rule of 6.4: the first open of a non-empty file registers its cache file and every
/// later open of it, by anyone, gets the same id with no registration; an empty file is a cached
/// open; the id is closed at the last release and not before.
#[test]
fn a_blob_has_one_id_for_as_long_as_any_open_of_it_lives() {
    let rig = passthrough("tfs-pt-one-id");
    let kernel = FakeKernel::new();
    let tool = ino::of(1, 4);
    let (first, id) = backed(kernel.open(&rig.core, tool).unwrap());
    let (second, again) = backed(kernel.open(&rig.core, tool).unwrap());
    assert_eq!((id, again), (100, 100));
    assert_ne!(first, second);
    assert_eq!(*locked(&kernel.registered), [10]);
    let table = rig.core.backing_table().unwrap();
    assert_eq!(table.opens(&digest_of(b"tool bytes")), 2);
    rig.core.release(first);
    assert!(locked(&kernel.closed).is_empty());
    rig.core.release(second);
    assert_eq!(*locked(&kernel.closed), [100]);
    assert_eq!(table.live(), 0);
    // A new open after the last release registers afresh.
    assert_eq!(backed(kernel.open(&rig.core, tool).unwrap()).1, 101);
    let empty = kernel.open(&rig.core, ino::of(1, 5)).unwrap();
    assert!(matches!(empty, Opened::Cached(_)));
}

/// Asserts that two inodes whose files have the same contents are served from one id, and that a
/// blob in two trees is one id too, while each tree pins it once.
#[test]
fn two_inodes_and_two_trees_with_one_blob_share_one_id() {
    let mut built = Built::of(vec![("a", file(b"same")), ("b", file(b"same"))]);
    let other = built.add_tree(vec![("c", file(b"same")), ("d", file(b"different"))]);
    let one = built.root.clone();
    let rig = Rig::new("tfs-pt-shared", built);
    rig.core.enable_passthrough();
    let _ = rig.core.attach(&one, "run-1").unwrap();
    let _ = rig.core.attach(&other, "run-2").unwrap();
    let kernel = FakeKernel::new();
    let (_, a) = backed(kernel.open(&rig.core, ino::of(1, 1)).unwrap());
    let (_, b) = backed(kernel.open(&rig.core, ino::of(1, 2)).unwrap());
    let (_, c) = backed(kernel.open(&rig.core, ino::of(2, 1)).unwrap());
    let (_, d) = backed(kernel.open(&rig.core, ino::of(2, 2)).unwrap());
    assert_eq!((a, b, c), (100, 100, 100));
    assert_eq!(d, 101);
    assert_eq!(kernel.registered.lock().unwrap().len(), 2);
    let pins = rig.upstream.asked("pin");
    let named = |tree: &str| pins.iter().filter(|line| line.contains(tree)).count();
    assert_eq!((named(&one), named(&other)), (1, 2));
}

/// Asserts that a blob evicted from the cache while open is served again from its live id with no
/// fill, and that the kernel is not asked to register anything for it.
#[test]
fn an_evicted_blob_that_is_still_open_is_opened_again_from_its_id_with_no_fill() {
    let rig = passthrough("tfs-pt-evicted");
    let kernel = FakeKernel::new();
    let cc = ino::of(1, 3);
    let (_, id) = backed(kernel.open(&rig.core, cc).unwrap());
    let sha = crate::server::treefs::testing::sha(
        &(0..3000u32).map(|i| (i % 241) as u8).collect::<Vec<u8>>(),
    );
    std::fs::remove_file(crate::server::local_cache::blob_path(&rig.cache, &sha)).unwrap();
    let fills = rig.upstream.asked("fill").len();
    let (_, again) = backed(kernel.open(&rig.core, cc).unwrap());
    assert_eq!((id, again), (100, 100));
    assert_eq!(rig.upstream.asked("fill").len(), fills);
    assert_eq!(kernel.registered.lock().unwrap().len(), 1);
}

/// Asserts that a fill that fails fails the open, is recorded on the attachment, registers
/// nothing, and does not keep the blob from being opened once the fill works.
#[test]
fn a_failed_fill_fails_the_open_registers_nothing_and_the_next_open_can_register() {
    let rig = passthrough("tfs-pt-fill-fails");
    let kernel = FakeKernel::new();
    let tool = digest_of(b"tool bytes").to_string();
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(tool.clone(), "upstream 503".to_owned());
    assert!(matches!(
        kernel.open(&rig.core, ino::of(1, 4)),
        Err(Errno::Io)
    ));
    assert!(kernel.registered.lock().unwrap().is_empty());
    assert_eq!(rig.core.backing_table().unwrap().live(), 0);
    let failures = rig.core.detach("run-1").unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(
        (failures[0].path.as_str(), failures[0].reason.as_str()),
        ("/bin/tool", "upstream 503")
    );
    let _ = rig.upstream.failing.lock().unwrap().remove(&tool);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    assert_eq!(
        backed(kernel.open(&rig.core, ino::of(1, 4)).unwrap()).1,
        100
    );
}

/// Asserts that a registration the kernel refuses (no `CAP_SYS_ADMIN`) fails the open with `EIO`
/// and leaves no id, and that the open works when the kernel accepts.
#[test]
fn a_registration_the_kernel_refuses_fails_the_open_and_leaves_no_id() {
    let rig = passthrough("tfs-pt-refused");
    let kernel = FakeKernel::new();
    kernel
        .refusing
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(matches!(
        kernel.open(&rig.core, ino::of(1, 4)),
        Err(Errno::Io)
    ));
    assert_eq!(rig.core.backing_table().unwrap().live(), 0);
    // The attachment records no failure: the blob was had.
    assert_eq!(rig.core.detach("run-1").unwrap().len(), 0);
    kernel
        .refusing
        .store(false, std::sync::atomic::Ordering::Release);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    assert_eq!(
        backed(kernel.open(&rig.core, ino::of(1, 4)).unwrap()).1,
        100
    );
}

/// Asserts that a read request for a passthrough open (which the kernel serves itself) is an
/// error, not empty data.
#[test]
fn a_read_that_reaches_the_core_for_a_passthrough_open_is_an_error() {
    let rig = passthrough("tfs-pt-read");
    let kernel = FakeKernel::new();
    let (fh, _) = backed(kernel.open(&rig.core, ino::of(1, 4)).unwrap());
    assert_eq!(rig.core.read(fh, 0, 10), Err(Errno::Io));
    rig.core.release(fh);
    // A handle released twice, or never opened, is nothing.
    rig.core.release(fh);
    rig.core.release(9999);
    assert_eq!(rig.core.backing_table().unwrap().live(), 0);
}

/// Asserts that opens of one blob that come while its fill is in flight register one id between
/// them: one registers, the others wait for it and share its id.
#[test]
fn opens_that_meet_in_one_fill_register_one_id() {
    let rig = passthrough("tfs-pt-race");
    let kernel = Arc::new(FakeKernel::new());
    rig.upstream.hold_fills();
    let tool = ino::of(1, 4);
    let opens: Vec<_> = (0..4)
        .map(|_| {
            let (core, kernel) = (Arc::clone(&rig.core), Arc::clone(&kernel));
            std::thread::spawn(move || backed(kernel.open(&core, tool).unwrap()).1)
        })
        .collect();
    let table = Arc::clone(rig.core.backing_table().unwrap());
    until(|| table.waiting() == 3 && !rig.upstream.asked("fill").is_empty());
    rig.upstream.release_fills();
    let ids: Vec<u32> = opens.into_iter().map(|open| open.join().unwrap()).collect();
    assert_eq!(ids, [100, 100, 100, 100]);
    assert_eq!(kernel.registered.lock().unwrap().len(), 1);
    assert_eq!(table.opens(&digest_of(b"tool bytes")), 4);
}

/// Asserts that choosing passthrough a second time changes nothing, and that before it is chosen
/// a core opens by the cached mode.
#[test]
fn passthrough_is_chosen_once_and_a_core_without_it_opens_cached() {
    let rig = Rig::new("tfs-pt-once", sample());
    assert!(!rig.core.passthrough());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let kernel = FakeKernel::new();
    assert!(matches!(
        kernel.open(&rig.core, ino::of(1, 4)),
        Ok(Opened::Cached(_))
    ));
    rig.core.enable_passthrough();
    let table = Arc::clone(rig.core.backing_table().unwrap());
    rig.core.enable_passthrough();
    assert!(Arc::ptr_eq(&table, rig.core.backing_table().unwrap()));
    assert!(rig.core.passthrough());
}

/// Asserts that the table file is made at the choice, names the blob, id and opens while the blob
/// is open, and is empty after the last release.
#[test]
fn the_backing_table_file_follows_the_first_open_and_the_last_release() {
    let rig = passthrough("tfs-pt-file");
    let file = rig.runtime.join("backing");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
    let kernel = FakeKernel::new();
    let (fh, _) = backed(kernel.open(&rig.core, ino::of(1, 4)).unwrap());
    let sha = digest_of(b"tool bytes");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("{sha} 100 1\n")
    );
    rig.core.release(fh);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
}

/// Asserts the limit of `fs.max_backing_ids` at the core: with every id live, an open of another
/// blob is a cached open that reads its bytes (no wait, no registration), a fill that fails fails
/// that open and leaves nothing counted, the release ends the count, and the blob registers an id
/// at its next open once its cached opens are released.
#[test]
fn past_the_most_ids_a_blob_is_opened_cached_and_never_waits() {
    let rig = Rig::limited(
        "tfs-pt-most",
        sample(),
        crate::server::treefs::testing::OWNER,
        1,
    );
    rig.core.enable_passthrough();
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let kernel = FakeKernel::new();
    let (tool, a_h) = (ino::of(1, 4), ino::of(1, 7));
    let (held, id) = backed(kernel.open(&rig.core, tool).unwrap());
    assert_eq!(id, 100);
    let a_digest = digest_of(b"int a;");
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(a_digest.to_string(), "upstream 503".to_owned());
    assert!(matches!(kernel.open(&rig.core, a_h), Err(Errno::Io)));
    let table = Arc::clone(rig.core.backing_table().unwrap());
    assert_eq!(table.spilled(&a_digest), 0);
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .remove(&a_digest.to_string());
    let Opened::Cached(fh) = kernel.open(&rig.core, a_h).unwrap() else {
        panic!("a backed open past the limit");
    };
    assert_eq!(rig.core.read(fh, 0, 100).unwrap(), b"int a;");
    assert_eq!((table.live(), table.spilled(&a_digest)), (1, 1));
    assert_eq!(kernel.registered.lock().unwrap().len(), 1);
    rig.core.release(fh);
    assert_eq!(table.spilled(&a_digest), 0);
    rig.core.release(held);
    assert_eq!(table.live(), 0);
    assert_eq!(backed(kernel.open(&rig.core, a_h).unwrap()).1, 101);
}
