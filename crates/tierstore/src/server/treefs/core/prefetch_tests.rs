//! Prefetch: an attach that asks for it fills and pins every blob of the tree in the background.
//! The fills are the upstream's (`Rig::upstream`); the pins are the cache's (`Rig::node`), which a
//! test looks at by the bytes it keeps pinned and by the blobs the `unpin` of a tree takes away.

use std::sync::Arc;

use super::super::{Core, Looked};
use super::BATCH;
use crate::server::local_cache::Blob;
use crate::server::treefs::ino;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{
    Built, PACK, Rig, Upstream, file, open_cached, sample, sha, until,
};

const RUN: &str = "run-1";

/// The mount the rigs' core pins for.
const MOUNT: &str = "mount-1";

/// The bytes of the four non-empty files of the sample tree: `big.bin`, `bin/cc`, `bin/tool` and
/// `include/a.h`.
const SAMPLE_BYTES: u64 = 300_000 + 3_000 + 10 + 6;

/// Lets every fill go on when dropped, so that a test that fails with a fill held ends.
struct Permitted(Arc<Upstream>);

impl Drop for Permitted {
    fn drop(&mut self) {
        self.0.permit_verb("fill", 1 << 20);
    }
}

/// The blobs the fills asked for, in order (the manifests, which have no size, are left out).
fn blob_fills(rig: &Rig) -> Vec<String> {
    rig.upstream
        .asked("fill")
        .iter()
        .filter(|line| line.contains(" from "))
        .map(|line| line.split(' ').nth(1).unwrap().to_owned())
        .collect()
}

fn root(rig: &Rig) -> String {
    rig.upstream.built.lock().unwrap().root.clone()
}

/// The digests of the sample tree's non-empty files in manifest order: `big.bin`, `bin/cc`,
/// `bin/tool`, `include/a.h`.
fn sample_blobs() -> Vec<String> {
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let cc: Vec<u8> = (0..3000u32).map(|i| (i % 241) as u8).collect();
    vec![sha(&big), sha(&cc), sha(b"tool bytes"), sha(b"int a;")]
}

/// Asserts that an attach with `prefetch` is answered while the first fill is still in flight,
/// and that then every non-empty blob is filled once, in manifest order, and pinned for the tree,
/// so that opening one afterwards asks for neither.
#[test]
fn a_prefetching_attach_is_answered_before_the_fills_and_fills_in_manifest_order() {
    let rig = Rig::new("tfs-prefetch-order", sample());
    rig.upstream.hold_fills();
    let path = rig.core.attach_asking(&root(&rig), RUN, true).unwrap();
    assert!(path.ends_with(root(&rig)));
    assert_eq!(rig.core.attachments().len(), 1);
    until(|| !blob_fills(&rig).is_empty());
    assert_eq!(rig.core.prefetching(), 1);
    assert_eq!(rig.node.pinned_bytes(), 0);
    rig.upstream.release_fills();
    until(|| rig.core.prefetching() == 0);
    assert_eq!(blob_fills(&rig), sample_blobs());
    assert_eq!(rig.node.pinned_bytes(), SAMPLE_BYTES);
    let (fills, pins) = (blob_fills(&rig).len(), rig.core.pins.pins_asked());
    let _ = open_cached(&rig.core, ino::of(1, 3), false).unwrap();
    assert_eq!(blob_fills(&rig).len(), fills);
    assert_eq!(rig.core.pins.pins_asked(), pins);
    assert_eq!(rig.node.unpin(MOUNT, &root(&rig)).unwrap(), 4);
}

/// Asserts that a tree asked to prefetch twice while it is prefetching has one prefetch, and that
/// a file whose bytes appear twice is filled once.
#[test]
fn one_prefetch_runs_per_tree_and_a_blob_in_two_files_is_filled_once() {
    let built = Built::of(vec![
        ("a", file(b"same")),
        ("b", file(b"same")),
        ("c", file(b"other")),
    ]);
    let rig = Rig::new("tfs-prefetch-once", built);
    rig.upstream.hold_fills();
    let root = root(&rig);
    let _ = rig.core.attach_asking(&root, RUN, true).unwrap();
    let _ = rig.core.attach_asking(&root, "run-2", true).unwrap();
    until(|| !blob_fills(&rig).is_empty());
    assert_eq!(rig.core.prefetching(), 1);
    rig.upstream.release_fills();
    until(|| rig.core.prefetching() == 0);
    assert_eq!(blob_fills(&rig), [sha(b"same"), sha(b"other")]);
}

/// Asserts that a blob that cannot be had is recorded on the attachment as a failed open records
/// it and is not pinned, and the other blobs are filled and pinned.
#[test]
fn a_blob_that_fails_is_recorded_and_the_others_go_on() {
    let rig = Rig::new("tfs-prefetch-fails", sample());
    let tool = sha(b"tool bytes");
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(tool.clone(), "upstream 404".to_owned());
    let _ = rig.core.attach_asking(&root(&rig), RUN, true).unwrap();
    until(|| rig.core.prefetching() == 0);
    assert_eq!(blob_fills(&rig).len(), 4);
    assert_eq!(rig.node.pinned_bytes(), SAMPLE_BYTES - 10);
    let failures = rig.core.detach(RUN).unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(
        (failures[0].path.as_str(), failures[0].digest.as_str()),
        ("/bin/tool", tool.as_str())
    );
}

/// Asserts that a prefetch stops at the next blob when its tree is detached, and pins nothing
/// after the detach.
#[test]
fn a_detach_stops_the_prefetch_and_nothing_is_pinned_after_it() {
    let rig = Rig::new("tfs-prefetch-detach", sample());
    let _ = rig.core.attach(&root(&rig), RUN).unwrap();
    rig.upstream.hold_fills();
    let thread = {
        let core = Arc::clone(&rig.core);
        std::thread::spawn(move || core.prefetch_here(1))
    };
    until(|| !blob_fills(&rig).is_empty());
    assert!(rig.core.detach(RUN).is_some());
    rig.upstream.release_fills();
    thread.join().unwrap();
    assert_eq!(blob_fills(&rig).len(), 1);
    assert_eq!(rig.node.pinned_bytes(), 0);
}

/// Asserts that a prefetch goes on when the last run detaches while a lookup has the tree attached
/// with no run: with a fill paused mid-prefetch, the run's detach unpins nothing (it takes no lock
/// of the tree's pins), and once the fill goes on every blob of the tree is filled and pinned for
/// it, the implicit attachment still there.
#[test]
fn a_prefetch_goes_on_after_the_last_run_detaches_while_a_lookup_has_the_tree() {
    let rig = Rig::new("tfs-prefetch-implicit", sample());
    let digest = root(&rig);
    assert!(matches!(
        rig.core.lookup(ino::TREES, &digest),
        Ok(Looked::Found(_))
    ));
    assert_eq!(rig.core.implicit(), vec![1]);
    rig.upstream.hold_fills();
    let _ = rig.core.attach_asking(&digest, RUN, true).unwrap();
    until(|| !blob_fills(&rig).is_empty());
    assert!(rig.core.detach(RUN).is_some());
    assert_eq!(rig.core.pins.unpins_asked(), 0);
    rig.upstream.release_fills();
    until(|| rig.core.prefetching() == 0);
    assert_eq!(blob_fills(&rig), sample_blobs());
    assert_eq!(rig.node.pinned_bytes(), SAMPLE_BYTES);
    assert_eq!(rig.core.implicit(), vec![1]);
    assert_eq!(rig.core.pins.unpins_asked(), 0);
    // Every pin is the tree's own: the blobs the tree's unpin takes away are all of them.
    assert_eq!(rig.node.unpin(MOUNT, &digest).unwrap(), 4);
    assert_eq!(rig.node.pinned_bytes(), 0);
}

/// Asserts that the blob in flight when the tree is detached is not pinned when it lands.
#[test]
fn the_last_blob_to_land_after_a_detach_is_not_pinned() {
    let rig = Rig::new(
        "tfs-prefetch-detach-last",
        Built::of(vec![("only", file(b"one"))]),
    );
    let _ = rig.core.attach(&root(&rig), RUN).unwrap();
    rig.upstream.hold_fills();
    let thread = {
        let core = Arc::clone(&rig.core);
        std::thread::spawn(move || core.prefetch_here(1))
    };
    until(|| !blob_fills(&rig).is_empty());
    assert!(rig.core.detach(RUN).is_some());
    rig.upstream.release_fills();
    thread.join().unwrap();
    assert_eq!(blob_fills(&rig), [sha(b"one")]);
    assert_eq!(rig.node.pinned_bytes(), 0);
}

/// Asserts that blobs are pinned in requests of at most `BATCH`, the last one with the rest: with
/// the fills let through one `BATCH` at a time, the cache holds the pins of the first `BATCH`
/// blobs, and no more, once the fill after them has been received (the prefetch fills and pins one
/// tree on one thread, so the pin of a batch comes before the next fill), and all of them at the
/// end.
#[test]
fn pins_go_in_batches() {
    let files: Vec<(String, Vec<u8>)> = (0..BATCH + 10)
        .map(|i| (format!("f{i:04}"), format!("contents {i}").into_bytes()))
        .collect();
    let built = Built::of(files.iter().map(|(n, c)| (n.as_str(), file(c))).collect());
    let rig = Rig::new("tfs-prefetch-batches", built);
    let _ = rig.core.attach(&root(&rig), RUN).unwrap();
    rig.upstream.gate_verb("fill");
    let _permitted = Permitted(Arc::clone(&rig.upstream));
    let thread = {
        let core = Arc::clone(&rig.core);
        std::thread::spawn(move || core.prefetch_here(1))
    };
    rig.upstream.permit_verb("fill", BATCH);
    rig.upstream.wait_for_verb("fill", BATCH + 1);
    let first: usize = files.iter().take(BATCH).map(|(_, c)| c.len()).sum();
    let all: usize = files.iter().map(|(_, c)| c.len()).sum();
    assert_eq!(rig.node.pinned_bytes(), first as u64);
    rig.upstream.permit_verb("fill", 1 << 20);
    thread.join().unwrap();
    assert_eq!(rig.node.pinned_bytes(), all as u64);
}

/// A core that pins for the mount `hold`, which the cache keeps for its own holds and refuses to
/// pin for, over the cache of `rig`.
fn refused_pins(rig: &Rig) -> Arc<Core> {
    let mut settings = rig.core.settings().clone();
    settings.mount_id = "hold".to_owned();
    let runtime = rig.dir.path().join("run-refused");
    std::fs::create_dir_all(&runtime).unwrap();
    Arc::new(Core::new(
        settings,
        rig.asks(),
        TreeTable::create(&runtime).unwrap(),
        &runtime,
    ))
}

/// Asserts that a pin the cache refuses (here, for the mount id its holds are pinned under) leaves
/// the blobs unpinned, so that the open of one asks again, and that a prefetch of a tree already
/// pinned fills and pins nothing more.
#[test]
fn a_refused_pin_is_asked_again_by_an_open_and_a_pinned_tree_is_not_prefetched_twice() {
    let rig = Rig::new("tfs-prefetch-refused", sample());
    let refusing = refused_pins(&rig);
    let _ = refusing.attach_asking(&root(&rig), RUN, true).unwrap();
    until(|| refusing.prefetching() == 0);
    assert_eq!(blob_fills(&rig).len(), 4);
    assert_eq!(rig.node.pinned_bytes(), 0);
    let pins = refusing.pins.pins_asked();
    let _ = open_cached(&refusing, ino::of(1, 4), false).unwrap();
    assert_eq!(refusing.pins.pins_asked(), pins + 1);
    assert_eq!(rig.node.pinned_bytes(), 0);
    // The core of the mount the cache pins for prefetches the blobs the cache holds now.
    let _ = rig.core.attach(&root(&rig), RUN).unwrap();
    rig.core.prefetch_here(1);
    assert_eq!(blob_fills(&rig).len(), 4);
    assert_eq!(rig.node.pinned_bytes(), SAMPLE_BYTES);
    assert_eq!(rig.core.tree(1).unwrap().pinned.len(), 4);
    rig.core.prefetch_here(1);
    assert_eq!(blob_fills(&rig).len(), 4);
    assert_eq!(rig.node.pinned_bytes(), SAMPLE_BYTES);
    assert_eq!(rig.core.tree(1).unwrap().pinned.len(), 4);
}

/// Asserts that a prefetch of a tree that is not there does nothing, and that blobs already in
/// the cache are pinned without a fill.
#[test]
fn a_missing_tree_is_not_prefetched_and_cached_blobs_are_not_filled() {
    let rig = Rig::new("tfs-prefetch-cached", sample());
    rig.core.prefetch_here(77);
    assert_eq!(rig.node.pinned_bytes(), 0);
    let _ = rig.core.attach(&root(&rig), RUN).unwrap();
    // The cache fills the blobs, as an earlier mount's opens did.
    for blob in sample_blobs() {
        let (key, offset) = {
            let built = rig.upstream.built.lock().unwrap();
            built
                .offsets
                .get(&blob)
                .map_or_else(|| (format!("cas/l/{blob}"), 0), |at| (PACK.to_owned(), *at))
        };
        let size = rig.upstream.built.lock().unwrap().blobs[&blob].len() as u64;
        let wanted = Blob {
            sha256: &blob,
            size,
            from: &key,
            offset,
        };
        rig.work
            .handle()
            .block_on(rig.node.fill_blob(&wanted))
            .unwrap();
    }
    let filled = blob_fills(&rig).len();
    assert_eq!(filled, 4);
    rig.core.prefetch_here(1);
    assert_eq!(blob_fills(&rig).len(), filled);
    assert_eq!(rig.node.pinned_bytes(), SAMPLE_BYTES);
}

/// Asserts that a prefetched blob is opened with no fill, and reads its bytes from the cache.
#[test]
fn a_prefetched_blob_is_opened_with_no_fill() {
    let rig = Rig::new("tfs-prefetch-open", sample());
    let _ = rig.core.attach_asking(&root(&rig), RUN, true).unwrap();
    until(|| rig.core.prefetching() == 0);
    let fills = blob_fills(&rig).len();
    let fh = open_cached(&rig.core, ino::of(1, 4), false).unwrap();
    assert_eq!(rig.core.read(fh, 0, 100).unwrap(), b"tool bytes");
    assert_eq!(blob_fills(&rig).len(), fills);
}

/// Asserts that a prefetch whose tree was detached and attached again with prefetch is replaced:
/// the old thread stops at its next blob and pins nothing, the new one fills and pins the tree
/// once, and the old thread's end leaves the new one's marker.
#[test]
fn a_prefetch_replaced_by_a_detach_and_a_new_attach_stops_and_pins_nothing() {
    let rig = Rig::new("tfs-prefetch-replaced", sample());
    let _ = rig.core.attach(&root(&rig), RUN).unwrap();
    rig.upstream.hold_fills();
    let old = {
        let core = Arc::clone(&rig.core);
        std::thread::spawn(move || core.prefetch_here(1))
    };
    until(|| !blob_fills(&rig).is_empty());
    assert!(rig.core.detach(RUN).is_some());
    assert_eq!(rig.core.prefetching(), 0);
    let _ = rig.core.attach_asking(&root(&rig), RUN, true).unwrap();
    assert_eq!(rig.core.prefetching(), 1);
    rig.upstream.release_fills();
    old.join().unwrap();
    until(|| rig.core.prefetching() == 0);
    assert_eq!(rig.node.pinned_bytes(), SAMPLE_BYTES);
    assert_eq!(rig.node.unpin(MOUNT, &root(&rig)).unwrap(), 4);
}
